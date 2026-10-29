/*
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
#[cfg(unix)]
use notify::{RecursiveMode, Watcher};
#[cfg(unix)]
use tokio::net::UnixStream;
use tonic::transport::Channel;
use tonic::transport::{Endpoint, Uri};
use tonic::Request;
use tower::service_fn;

use ::rpc::flame::v1 as rpc;
use rpc::instance_client::InstanceClient;
use rpc::{EmptyRequest, ExecutorAttributes};

use crate::appmgr::ApplicationManager;
use crate::shims::{ExecutorWorkDir, Shim};
use common::apis::{SessionContext, TaskContext, TaskResult, TaskState};
use common::FlameError;
use stdng::{logs::TraceFn, new_ptr, trace_fn};

/// Longest to wait for a service to report that it is listening, and then to connect to it.
#[cfg(unix)]
const SERVICE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Safety net for the watch on the ready file: it is re-checked at this interval in case a
/// directory event was missed or the directory could not be watched.
#[cfg(unix)]
const READY_RECHECK_INTERVAL: Duration = Duration::from_millis(250);

/// The file a service creates, next to its socket, once it is listening on the socket.
///
/// A socket file appears when the service binds, which can be before it listens, so the
/// executor manager waits for this file and not for the socket: it can then connect once.
pub fn ready_file(socket: &Path) -> std::path::PathBuf {
    let mut ready = socket.as_os_str().to_owned();
    ready.push(".ready");
    ready.into()
}

pub struct GrpcShim {
    client: Option<InstanceClient<Channel>>,
    endpoint: String,
}

impl GrpcShim {
    pub fn new(work_dir: &ExecutorWorkDir) -> Result<Self, FlameError> {
        Self::new_at(work_dir.socket())
    }

    pub fn new_at(endpoint: &Path) -> Result<Self, FlameError> {
        trace_fn!("GrpcShim::new");

        Ok(Self {
            client: None,
            endpoint: endpoint.to_string_lossy().to_string(),
        })
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    #[cfg(unix)]
    pub async fn connect(&mut self) -> Result<(), FlameError> {
        trace_fn!("GrpcShim::connect");

        self.wait_for_ready(SERVICE_WAIT_TIMEOUT).await?;
        tracing::debug!("Try to connect to service at <{}>", self.endpoint);

        let endpoint = Endpoint::try_from("http://[::]:50051").unwrap();
        let connect = endpoint.connect_with_connector({
            let service_addr = self.endpoint.clone();

            service_fn(move |_: Uri| {
                let service_addr = service_addr.clone();
                async move {
                    UnixStream::connect(service_addr)
                        .await
                        .map(TokioIo::new)
                        .map_err(std::io::Error::other)
                }
            })
        });
        let channel = tokio::time::timeout(SERVICE_WAIT_TIMEOUT, connect)
            .await
            .map_err(|_| {
                FlameError::Network(format!(
                    "timed out connecting to service at <{}>",
                    self.endpoint
                ))
            })?
            .map_err(|e| {
                FlameError::Network(format!(
                    "failed to connect to service at <{}>: {e}",
                    self.endpoint
                ))
            })?;

        self.client = Some(InstanceClient::new(channel));

        Ok(())
    }

    /// Wait until the service reports that it is listening, by creating the ready file next to
    /// its socket. A watch on that directory wakes this as soon as the file appears; the watch
    /// starts before the first check so a file created in between is not missed.
    #[cfg(unix)]
    async fn wait_for_ready(&self, timeout: Duration) -> Result<(), FlameError> {
        let ready = ready_file(Path::new(&self.endpoint));
        let deadline = tokio::time::Instant::now() + timeout;
        let (events, mut changed) = tokio::sync::mpsc::unbounded_channel::<()>();
        let _watcher = watch_dir(ready.parent(), events);

        loop {
            if ready.exists() {
                return self.check_socket();
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(FlameError::Network(format!(
                    "timed out waiting for service at <{}> to report ready",
                    self.endpoint
                )));
            }
            let wait = remaining.min(READY_RECHECK_INTERVAL);
            // Woken by a directory event; otherwise re-check when the interval elapses. If the
            // watcher is gone the channel is closed, so wait out the interval instead.
            if let Ok(None) = tokio::time::timeout(wait, changed.recv()).await {
                tokio::time::sleep(wait).await;
            }
        }
    }

    /// The service reported ready, so the endpoint must be its Unix socket.
    #[cfg(unix)]
    fn check_socket(&self) -> Result<(), FlameError> {
        match std::fs::symlink_metadata(&self.endpoint) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(FlameError::InvalidState(format!(
                    "instance endpoint <{}> must not be a symlink",
                    self.endpoint
                )))
            }
            Ok(metadata) if metadata.file_type().is_socket() => Ok(()),
            Ok(_) => Err(FlameError::InvalidState(format!(
                "instance endpoint <{}> is not a Unix socket",
                self.endpoint
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(FlameError::InvalidState(format!(
                    "service reported ready but its socket <{}> does not exist",
                    self.endpoint
                )))
            }
            Err(error) => Err(FlameError::Storage(error.to_string())),
        }
    }

    #[cfg(not(unix))]
    pub async fn connect(&mut self) -> Result<(), FlameError> {
        Err(FlameError::Network(
            "Unix domain sockets are not supported on this platform".to_string(),
        ))
    }

    pub fn close(&mut self) {
        if self.client.take().is_some() {
            tracing::debug!("Closed gRPC connection to service at <{}>", self.endpoint);
        }
    }
}

/// Watch `dir`, signalling `events` on every change. Returns `None` when there is no directory
/// or it cannot be watched; the periodic re-check still finds the ready file.
#[cfg(unix)]
fn watch_dir(
    dir: Option<&Path>,
    events: tokio::sync::mpsc::UnboundedSender<()>,
) -> Option<notify::RecommendedWatcher> {
    let dir = dir?;
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = events.send(());
        }
    })
    .and_then(|mut watcher| {
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    });
    match watcher {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::debug!(
                "Cannot watch <{}> for the ready file: {error}",
                dir.display()
            );
            None
        }
    }
}

#[async_trait]
impl Shim for GrpcShim {
    async fn create_instance(
        &mut self,
        _app_manager: Arc<ApplicationManager>,
    ) -> Result<(), FlameError> {
        self.connect().await
    }

    async fn on_session_enter(
        &mut self,
        ctx: &SessionContext,
    ) -> Result<super::SessionEnterResponse, FlameError> {
        trace_fn!("GrpcShim::on_session_enter");

        if let Some(ref mut client) = self.client {
            let req = Request::new(rpc::SessionContext::from(ctx.clone()));
            tracing::debug!("req: {:?}", req);
            let resp = client.on_session_enter(req).await?;
            let output = resp.into_inner();
            let result = output.result.unwrap_or_default();
            if result.return_code != 0 {
                return Err(FlameError::Internal(result.message.unwrap_or_default()));
            }
            return Ok(super::SessionEnterResponse {
                executor_attributes: output.attributes.map(new_ptr),
            });
        } else {
            return Err(FlameError::Internal(format!(
                "no connection to service at <{}>",
                self.endpoint
            )));
        }
    }

    async fn on_task_invoke(
        &mut self,
        ctx: &TaskContext,
    ) -> Result<super::TaskInvokeResponse, FlameError> {
        trace_fn!("GrpcShim::on_task_invoke");

        if let Some(ref mut client) = self.client {
            let req = Request::new(rpc::TaskContext::from(ctx.clone()));
            tracing::debug!("req: {:?}", req);
            let resp = client.on_task_invoke(req).await?;
            let output = resp.into_inner();

            // Convert rpc::TaskResult to TaskResult
            // The From trait handles return_code != 0 by setting TaskState::Failed
            let task_result: TaskResult = output.task_result.unwrap_or_default().into();

            // Log error if task failed
            if task_result.state == TaskState::Failed {
                let error_msg = task_result.message.as_deref().unwrap_or("Task failed");
                tracing::error!("Task failed: {}", error_msg);
            }

            return Ok(super::TaskInvokeResponse {
                task_result,
                attributes: output.attributes,
            });
        } else {
            return Err(FlameError::Internal(format!(
                "no connection to service at <{}>",
                self.endpoint
            )));
        }
    }

    async fn on_session_leave(&mut self) -> Result<(), FlameError> {
        trace_fn!("GrpcShim::on_session_leave");

        if let Some(ref mut client) = self.client {
            let req = Request::new(EmptyRequest::default());
            let resp = client.on_session_leave(req).await?;
            tracing::debug!("on_session_leave response: {:?}", resp);
            let output = resp.into_inner();
            if output.return_code != 0 {
                tracing::error!("on_session_leave failed: {:?}", output);
                return Err(FlameError::Internal(output.message.unwrap_or_default()));
            }
        } else {
            tracing::error!("no connection to service at <{}>", self.endpoint);
            return Err(FlameError::Internal(format!(
                "no connection to service at <{}>",
                self.endpoint
            )));
        }

        Ok(())
    }

    async fn destroy_instance(&mut self) -> Result<(), FlameError> {
        self.close();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{ApplicationContext, Shim as ShimType};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn setup_test_env(temp: &tempfile::TempDir) -> PathBuf {
        let socket_dir = temp.path().join("sockets");
        std::fs::create_dir_all(&socket_dir).unwrap();
        std::env::set_var("FLAME_SOCKET_DIR", &socket_dir);
        std::env::set_current_dir(temp.path()).unwrap();
        socket_dir
    }

    fn create_test_work_dir(executor_id: &str, temp: &tempfile::TempDir) -> ExecutorWorkDir {
        let app = ApplicationContext {
            name: "test-app".to_string(),
            shim: ShimType::Host,
            image: None,
            command: None,
            arguments: vec![],
            working_directory: None,
            environments: HashMap::new(),
            url: None,
            installer: None,
        };

        ExecutorWorkDir::new(&app, executor_id).unwrap()
    }

    #[test]
    fn test_grpc_shim_new() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-grpc-test", &temp);

        let shim = GrpcShim::new(&work_dir).unwrap();

        assert!(shim.client.is_none());
        assert!(shim.endpoint.contains("exec-grpc-test.sock"));
        assert_eq!(shim.endpoint, work_dir.socket().to_string_lossy());
    }

    #[test]
    fn test_grpc_shim_endpoint() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-endpoint-test", &temp);

        let shim = GrpcShim::new(&work_dir).unwrap();

        assert_eq!(shim.endpoint(), work_dir.socket().to_string_lossy());
    }

    #[test]
    fn test_grpc_shim_close_without_connection() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-close-test", &temp);

        let mut shim = GrpcShim::new(&work_dir).unwrap();

        shim.close();

        assert!(shim.client.is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_on_session_enter_without_connection() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-session-test", &temp);

        let mut shim = GrpcShim::new(&work_dir).unwrap();

        let ctx = SessionContext {
            session_id: "test-session".to_string(),
            application: ApplicationContext {
                name: "test-app".to_string(),
                shim: ShimType::Host,
                image: None,
                command: None,
                arguments: vec![],
                working_directory: None,
                environments: HashMap::new(),
                url: None,
                installer: None,
            },
            common_data: None,
            tokens: Default::default(),
            batch_size: 1,
        };

        let result = shim.on_session_enter(&ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no connection to service"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_on_task_invoke_without_connection() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-task-test", &temp);

        let mut shim = GrpcShim::new(&work_dir).unwrap();

        let ctx = TaskContext {
            task_id: "test-task".to_string(),
            session_id: "test-session".to_string(),
            input: None,
        };

        let result = shim.on_task_invoke(&ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no connection to service"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_on_session_leave_without_connection() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-leave-test", &temp);

        let mut shim = GrpcShim::new(&work_dir).unwrap();

        let result = shim.on_session_leave().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no connection to service"));
    }

    #[test]
    fn test_ready_file_is_next_to_the_socket() {
        assert_eq!(
            ready_file(Path::new("/var/flame/shim/exec-1.sock")),
            Path::new("/var/flame/shim/exec-1.sock.ready")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_ready_returns_when_the_ready_file_exists() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ready.sock");
        let _listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::write(ready_file(&path), b"").unwrap();
        let shim = GrpcShim::new_at(&path).unwrap();

        shim.wait_for_ready(Duration::from_secs(5)).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_ready_ignores_a_socket_that_is_not_ready() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("bound.sock");
        // Bound, but the service has not reported that it is listening.
        let _listener = tokio::net::UnixListener::bind(&path).unwrap();
        let shim = GrpcShim::new_at(&path).unwrap();

        let err = shim
            .wait_for_ready(Duration::from_millis(300))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("to report ready"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_ready_is_woken_by_the_ready_file_appearing() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("late.sock");
        let _listener = tokio::net::UnixListener::bind(&path).unwrap();
        let shim = GrpcShim::new_at(&path).unwrap();

        let (created_tx, created_rx) = tokio::sync::oneshot::channel();
        let service = tokio::spawn({
            let ready = ready_file(&path);
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                std::fs::write(&ready, b"").unwrap();
                created_tx.send(std::time::Instant::now()).unwrap();
            }
        });

        shim.wait_for_ready(Duration::from_secs(5)).await.unwrap();
        let seen = std::time::Instant::now();
        let created = created_rx.await.unwrap();
        service.await.unwrap();

        // Found by the directory event, not by the periodic re-check, which would only look
        // again READY_RECHECK_INTERVAL after the first check, long after the file appeared.
        let delay = seen.duration_since(created);
        assert!(
            delay < READY_RECHECK_INTERVAL / 2,
            "ready file noticed {delay:?} after it was created"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_ready_rejects_a_ready_service_without_a_socket() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("missing.sock");
        std::fs::write(ready_file(&path), b"").unwrap();
        let shim = GrpcShim::new_at(&path).unwrap();

        let err = shim
            .wait_for_ready(Duration::from_secs(5))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_wait_for_ready_rejects_a_regular_file_as_the_socket() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("not-a-socket");
        std::fs::write(&path, b"").unwrap();
        std::fs::write(ready_file(&path), b"").unwrap();
        let shim = GrpcShim::new_at(&path).unwrap();

        let err = shim
            .wait_for_ready(Duration::from_secs(5))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("not a Unix socket"), "{err}");
    }
}

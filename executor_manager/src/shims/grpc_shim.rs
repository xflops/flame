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

use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
#[cfg(unix)]
use tokio::net::UnixStream;
use tonic::transport::Channel;
use tonic::transport::{Endpoint, Uri};
use tonic::Request;
use tower::service_fn;

use ::rpc::flame::v1 as rpc;
use rpc::instance_client::InstanceClient;
use rpc::EmptyRequest;

use crate::shims::{ExecutorWorkDir, Shim};
use common::apis::{SessionContext, TaskContext, TaskResult, TaskState};
use common::FlameError;
use stdng::{logs::TraceFn, trace_fn};

pub struct GrpcShim {
    client: Option<InstanceClient<Channel>>,
    endpoint: String,
}

impl GrpcShim {
    pub fn new(work_dir: &ExecutorWorkDir) -> Result<Self, FlameError> {
        trace_fn!("GrpcShim::new");

        Ok(Self {
            client: None,
            endpoint: work_dir.socket().to_string_lossy().to_string(),
        })
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    #[cfg(unix)]
    pub async fn connect(&mut self) -> Result<(), FlameError> {
        trace_fn!("GrpcShim::connect");

        WaitForSvcSocketFuture::new(self.endpoint.clone()).await?;
        tracing::debug!("Try to connect to service at <{}>", self.endpoint);

        let channel = Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector({
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
            })
            .await
            .map_err(|e| {
                FlameError::Network(format!(
                    "failed to connect to service at <{}>: {e}",
                    self.endpoint
                ))
            })?;

        self.client = Some(InstanceClient::new(channel));

        Ok(())
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

#[async_trait]
impl Shim for GrpcShim {
    async fn on_session_enter(&mut self, ctx: &SessionContext) -> Result<(), FlameError> {
        trace_fn!("GrpcShim::on_session_enter");

        if let Some(ref mut client) = self.client {
            let req = Request::new(rpc::SessionContext::from(ctx.clone()));
            tracing::debug!("req: {:?}", req);
            let resp = client.on_session_enter(req).await?;
            let output = resp.into_inner();
            if output.return_code != 0 {
                return Err(FlameError::Internal(output.message.unwrap_or_default()));
            }
        } else {
            return Err(FlameError::Internal(format!(
                "no connection to service at <{}>",
                self.endpoint
            )));
        }

        Ok(())
    }

    async fn on_task_invoke(&mut self, ctx: &TaskContext) -> Result<TaskResult, FlameError> {
        trace_fn!("GrpcShim::on_task_invoke");

        if let Some(ref mut client) = self.client {
            let req = Request::new(rpc::TaskContext::from(ctx.clone()));
            tracing::debug!("req: {:?}", req);
            let resp = client.on_task_invoke(req).await?;
            let output = resp.into_inner();

            // Convert rpc::TaskResult to TaskResult
            // The From trait handles return_code != 0 by setting TaskState::Failed
            let task_result: TaskResult = output.into();

            // Log error if task failed
            if task_result.state == TaskState::Failed {
                let error_msg = task_result.message.as_deref().unwrap_or("Task failed");
                tracing::error!("Task failed: {}", error_msg);
            }

            return Ok(task_result);
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
}

struct WaitForSvcSocketFuture {
    path: String,
}

impl WaitForSvcSocketFuture {
    pub fn new(path: String) -> Self {
        Self { path }
    }
}

impl Future for WaitForSvcSocketFuture {
    type Output = Result<(), FlameError>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        if fs::exists(&self.path).unwrap_or(false) {
            Poll::Ready(Ok(()))
        } else {
            ctx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::SHIMS_TEST_LOCK;
    use super::*;
    use common::apis::{ApplicationContext, Shim as ShimType};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::tempdir;

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
        let _guard = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-endpoint-test", &temp);

        let shim = GrpcShim::new(&work_dir).unwrap();

        assert_eq!(shim.endpoint(), work_dir.socket().to_string_lossy());
    }

    #[test]
    fn test_grpc_shim_close_without_connection() {
        let _guard = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-close-test", &temp);

        let mut shim = GrpcShim::new(&work_dir).unwrap();

        shim.close();

        assert!(shim.client.is_none());
    }

    #[test]
    fn test_wait_for_svc_socket_future_new() {
        let future = WaitForSvcSocketFuture::new("/tmp/test.sock".to_string());

        assert_eq!(future.path, "/tmp/test.sock");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_on_session_enter_without_connection() {
        let _guard = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        };

        let result = shim.on_session_enter(&ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no connection to service"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_on_task_invoke_without_connection() {
        let _guard = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = SHIMS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let work_dir = create_test_work_dir("exec-leave-test", &temp);

        let mut shim = GrpcShim::new(&work_dir).unwrap();

        let result = shim.on_session_leave().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no connection to service"));
    }
}

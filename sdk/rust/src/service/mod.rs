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

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
#[cfg(unix)]
use tonic::transport::Server;
#[cfg(unix)]
use tonic::{Request, Response, Status};

#[cfg(unix)]
use self::rpc::instance_server::{Instance, InstanceServer};
use crate::apis::flame::v1 as rpc;

use crate::apis::{ApplicationID, CommonData, FlameError, TaskInput, TaskOutput};

pub use tonic::async_trait;

pub mod message {
    pub use crate::message::*;
}

#[cfg(unix)]
const FLAME_INSTANCE_ENDPOINT: &str = "FLAME_INSTANCE_ENDPOINT";

#[derive(Clone, Debug)]
pub struct ApplicationContext {
    pub name: String,
    pub image: Option<String>,
    pub command: Option<String>,
}

/// A service-owned publication slot shared with the shim. `None` means the
/// service has not published since session entry; `Some(empty)` clears attrs.
pub type ExecutorAttributesPtr = Arc<Mutex<Option<rpc::ExecutorAttributes>>>;

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub session_id: String,
    pub application: ApplicationContext,
    pub common_data: Option<CommonData>,
    executor_attributes: Option<ExecutorAttributesPtr>,
}

impl SessionContext {
    pub fn new(
        session_id: String,
        application: ApplicationContext,
        common_data: Option<CommonData>,
    ) -> Self {
        Self {
            session_id,
            application,
            common_data,
            executor_attributes: None,
        }
    }

    #[cfg(unix)]
    fn with_executor_attributes(mut self, executor_attributes: ExecutorAttributesPtr) -> Self {
        self.executor_attributes = Some(executor_attributes);
        self
    }

    /// Publish a complete replacement of this executor's opaque locality keys.
    /// The latest value is returned with the next existing shim response.
    pub fn publish<I, B>(&self, attributes: I) -> Result<(), FlameError>
    where
        I: IntoIterator<Item = B>,
        B: Into<Vec<u8>>,
    {
        let attr: HashSet<Vec<u8>> = attributes.into_iter().map(Into::into).collect();
        let executor_attributes = self.executor_attributes.as_ref().ok_or_else(|| {
            FlameError::InvalidConfig("session context is not attached to a service".to_string())
        })?;
        executor_attributes
            .lock()
            .expect("executor attributes mutex poisoned")
            .replace(rpc::ExecutorAttributes {
                attr: attr.into_iter().collect(),
            });
        Ok(())
    }

    fn executor_attributes(&self) -> Option<rpc::ExecutorAttributes> {
        self.executor_attributes.as_ref().and_then(|attributes| {
            attributes
                .lock()
                .expect("executor attributes mutex poisoned")
                .take()
        })
    }
}

#[derive(Clone, Debug)]
pub struct TaskContext {
    pub task_id: String,
    pub session_id: String,
    pub input: Option<TaskInput>,
}

#[derive(Clone, Debug)]
pub struct FlameInstance {
    session: SessionContext,
}

impl FlameInstance {
    pub fn new(session: SessionContext) -> Self {
        Self { session }
    }

    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    pub fn application(&self) -> &ApplicationContext {
        &self.session.application
    }

    pub fn application_name(&self) -> &ApplicationID {
        &self.session.application.name
    }

    pub fn common_data<T>(&self) -> Result<Option<T>, FlameError>
    where
        T: crate::message::FlameMessage,
    {
        crate::message::decode_common_data(self.session.common_data.as_ref())
    }
}

#[tonic::async_trait]
pub trait FlameService: Send + Sync + 'static {
    async fn on_session_enter(&self, _: SessionContext) -> Result<(), FlameError>;
    async fn on_task_invoke(&self, _: TaskContext) -> Result<Option<TaskOutput>, FlameError>;
    async fn on_session_leave(&self) -> Result<(), FlameError>;
}

pub type FlameServicePtr = Arc<dyn FlameService>;

#[cfg(unix)]
struct ShimService {
    service: FlameServicePtr,
    // The service receives a clone of this stable pointer at session entry.
    // `None` distinguishes no publication from an explicit empty snapshot.
    executor_attributes: ExecutorAttributesPtr,
}

#[cfg(unix)]
#[tonic::async_trait]
impl Instance for ShimService {
    async fn on_session_enter(
        &self,
        req: Request<rpc::SessionContext>,
    ) -> Result<Response<rpc::OnSessionEnterResponse>, Status> {
        tracing::debug!("ShimService::on_session_enter");

        let req = req.into_inner();
        let resp = match SessionContext::try_from(req) {
            Ok(ctx) => {
                self.executor_attributes
                    .lock()
                    .expect("executor attributes mutex poisoned")
                    .take();
                let service_ctx = ctx.with_executor_attributes(self.executor_attributes.clone());
                let response_ctx = service_ctx.clone();
                self.service
                    .on_session_enter(service_ctx)
                    .await
                    .map(|_| response_ctx)
            }
            Err(e) => Err(e),
        };

        match resp {
            Ok(ctx) => Ok(Response::new(rpc::OnSessionEnterResponse {
                result: Some(rpc::Result {
                    return_code: 0,
                    message: None,
                }),
                attributes: ctx.executor_attributes(),
            })),
            Err(e) => Ok(Response::new(rpc::OnSessionEnterResponse {
                result: Some(rpc::Result {
                    return_code: -1,
                    message: Some(e.to_string()),
                }),
                attributes: None,
            })),
        }
    }

    async fn on_task_invoke(
        &self,
        req: Request<rpc::TaskContext>,
    ) -> Result<Response<rpc::OnTaskInvokeResponse>, Status> {
        tracing::debug!("ShimService::on_task_invoke");
        let req = req.into_inner();
        let resp = self.service.on_task_invoke(TaskContext::from(req)).await;

        match resp {
            Ok(data) => Ok(Response::new(rpc::OnTaskInvokeResponse {
                task_result: Some(rpc::TaskResult {
                    return_code: 0,
                    output: data.map(|d| d.into()),
                    message: None,
                }),
                attributes: self.executor_attributes(),
            })),
            Err(e) => Ok(Response::new(rpc::OnTaskInvokeResponse {
                task_result: Some(rpc::TaskResult {
                    return_code: -1,
                    output: None,
                    message: Some(e.to_string()),
                }),
                attributes: None,
            })),
        }
    }

    async fn on_session_leave(
        &self,
        _: Request<rpc::EmptyRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        tracing::debug!("ShimService::on_session_leave");
        let resp = self.service.on_session_leave().await;
        self.executor_attributes
            .lock()
            .expect("executor attributes mutex poisoned")
            .take();

        match resp {
            Ok(_) => Ok(Response::new(rpc::Result {
                return_code: 0,
                message: None,
            })),
            Err(e) => Ok(Response::new(rpc::Result {
                return_code: -1,
                message: Some(e.to_string()),
            })),
        }
    }
}

#[cfg(unix)]
impl ShimService {
    fn executor_attributes(&self) -> Option<rpc::ExecutorAttributes> {
        self.executor_attributes
            .lock()
            .expect("executor attributes mutex poisoned")
            .take()
    }
}

#[cfg(unix)]
pub async fn run(service: impl FlameService) -> Result<(), Box<dyn std::error::Error>> {
    let shim_service = ShimService {
        service: Arc::new(service),
        executor_attributes: Arc::new(Mutex::new(None)),
    };

    let endpoint = std::env::var(FLAME_INSTANCE_ENDPOINT)
        .map_err(|_| FlameError::InvalidConfig("FLAME_INSTANCE_ENDPOINT not found".to_string()))?;

    let uds_stream = UnixListenerStream::new(UnixListener::bind(endpoint)?);

    Server::builder()
        .add_service(InstanceServer::new(shim_service))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}

#[cfg(not(unix))]
pub async fn run(_service: impl FlameService) -> Result<(), Box<dyn std::error::Error>> {
    Err(FlameError::InvalidConfig(
        "Unix domain sockets are not supported on this platform".to_string(),
    )
    .into())
}

impl From<rpc::ApplicationContext> for ApplicationContext {
    fn from(ctx: rpc::ApplicationContext) -> Self {
        Self {
            name: ctx.name.clone(),
            image: ctx.image.clone(),
            command: ctx.command.clone(),
        }
    }
}

impl TryFrom<rpc::SessionContext> for SessionContext {
    type Error = FlameError;

    fn try_from(ctx: rpc::SessionContext) -> Result<Self, Self::Error> {
        let application = ctx
            .application
            .map(ApplicationContext::from)
            .ok_or_else(|| {
                FlameError::InvalidConfig("session context missing application".to_string())
            })?;

        Ok(SessionContext::new(
            ctx.session_id.clone(),
            application,
            ctx.common_data.map(|data| data.into()),
        ))
    }
}

impl From<rpc::TaskContext> for TaskContext {
    fn from(ctx: rpc::TaskContext) -> Self {
        TaskContext {
            task_id: ctx.task_id.clone(),
            session_id: ctx.session_id.clone(),
            input: ctx.input.map(|data| data.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_context_requires_application() {
        let ctx = rpc::SessionContext {
            session_id: "ssn-1".to_string(),
            application: None,
            common_data: None,
        };

        assert!(SessionContext::try_from(ctx).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn session_context_publishes_to_shared_executor_attributes() {
        let attributes = Arc::new(Mutex::new(None));
        let context = SessionContext::new(
            "ssn-1".to_string(),
            ApplicationContext {
                name: "test-app".to_string(),
                image: None,
                command: None,
            },
            None,
        )
        .with_executor_attributes(attributes.clone());

        context.publish([b"kv-cache-key".to_vec()]).unwrap();

        assert_eq!(
            attributes.lock().unwrap().as_ref().unwrap().attr,
            vec![b"kv-cache-key".to_vec()]
        );
    }

    #[cfg(unix)]
    struct TaskPublishingService {
        session: Mutex<Option<SessionContext>>,
    }

    #[cfg(unix)]
    #[tonic::async_trait]
    impl FlameService for TaskPublishingService {
        async fn on_session_enter(&self, context: SessionContext) -> Result<(), FlameError> {
            *self.session.lock().unwrap() = Some(context);
            Ok(())
        }

        async fn on_task_invoke(
            &self,
            task: TaskContext,
        ) -> Result<Option<TaskOutput>, FlameError> {
            if task.task_id == "task-1" {
                self.session
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .publish([b"kv-cache-key".to_vec()])?;
            }
            Ok(None)
        }

        async fn on_session_leave(&self) -> Result<(), FlameError> {
            *self.session.lock().unwrap() = None;
            Ok(())
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_publication_is_returned_by_shim() {
        let shim = ShimService {
            service: Arc::new(TaskPublishingService {
                session: Mutex::new(None),
            }),
            executor_attributes: Arc::new(Mutex::new(None)),
        };

        let enter = Instance::on_session_enter(
            &shim,
            Request::new(rpc::SessionContext {
                session_id: "ssn-1".to_string(),
                application: Some(rpc::ApplicationContext {
                    name: "test-app".to_string(),
                    ..Default::default()
                }),
                common_data: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(enter.attributes.is_none());

        let invoke = Instance::on_task_invoke(
            &shim,
            Request::new(rpc::TaskContext {
                task_id: "task-1".to_string(),
                session_id: "ssn-1".to_string(),
                input: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(
            invoke.attributes.unwrap().attr,
            vec![b"kv-cache-key".to_vec()]
        );

        let next_invoke = Instance::on_task_invoke(
            &shim,
            Request::new(rpc::TaskContext {
                task_id: "task-2".to_string(),
                session_id: "ssn-1".to_string(),
                input: None,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(next_invoke.attributes.is_none());
    }
}

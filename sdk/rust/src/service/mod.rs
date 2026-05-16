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

use std::sync::Arc;

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

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub session_id: String,
    pub application: ApplicationContext,
    pub common_data: Option<CommonData>,
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
}

#[cfg(unix)]
#[tonic::async_trait]
impl Instance for ShimService {
    async fn on_session_enter(
        &self,
        req: Request<rpc::SessionContext>,
    ) -> Result<Response<rpc::Result>, Status> {
        tracing::debug!("ShimService::on_session_enter");

        let req = req.into_inner();
        let resp = self
            .service
            .on_session_enter(SessionContext::from(req))
            .await;

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

    async fn on_task_invoke(
        &self,
        req: Request<rpc::TaskContext>,
    ) -> Result<Response<rpc::TaskResult>, Status> {
        tracing::debug!("ShimService::on_task_invoke");
        let req = req.into_inner();
        let resp = self.service.on_task_invoke(TaskContext::from(req)).await;

        match resp {
            Ok(data) => Ok(Response::new(rpc::TaskResult {
                return_code: 0,
                output: data.map(|d| d.into()),
                message: None,
            })),
            Err(e) => Ok(Response::new(rpc::TaskResult {
                return_code: -1,
                output: None,
                message: Some(e.to_string()),
            })),
        }
    }

    async fn on_session_leave(
        &self,
        _: Request<rpc::EmptyRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        tracing::debug!("ShimService::on_session_leave");
        let resp = self.service.on_session_leave().await;

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
pub async fn run(service: impl FlameService) -> Result<(), Box<dyn std::error::Error>> {
    let shim_service = ShimService {
        service: Arc::new(service),
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

impl From<rpc::SessionContext> for SessionContext {
    fn from(ctx: rpc::SessionContext) -> Self {
        SessionContext {
            session_id: ctx.session_id.clone(),
            application: ctx.application.map(ApplicationContext::from).unwrap(),
            common_data: ctx.common_data.map(|data| data.into()),
        }
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

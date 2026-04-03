/*
Copyright 2023 The Flame Authors.
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
use std::pin::Pin;

use async_trait::async_trait;
use common::apis::{ApplicationAttributes, SessionAttributes};
use futures::Stream;
use serde_json::Value;
use stdng::trace_fn;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use self::rpc::frontend_server::Frontend;
use self::rpc::{
    ApplicationList, CloseSessionRequest, CreateSessionRequest, CreateTaskRequest,
    CreateWorkspaceRequest, DeleteSessionRequest, DeleteTaskRequest, DeleteWorkspaceRequest,
    ExecutorList, GetApplicationRequest, GetNodeRequest, GetNodeResponse, GetSessionRequest,
    GetTaskRequest, GetWorkspaceRequest, ListApplicationRequest, ListExecutorRequest,
    ListNodesRequest, ListSessionRequest, ListTaskRequest, ListWorkspacesRequest, NodeList,
    OpenSessionRequest, RegisterApplicationRequest, Session, SessionList, Task,
    UnregisterApplicationRequest, UpdateApplicationRequest, UpdateWorkspaceRequest,
    WatchTaskRequest,
};

use rpc::flame as rpc;

use common::apis::{self, Workspace};
use common::rbac::validate_workspace_name;
use common::FlameError;

use crate::apiserver::Flame;

fn validate_working_directory(working_dir: &Option<String>) -> Result<(), FlameError> {
    if let Some(wd) = working_dir {
        if !wd.is_empty() && !Path::new(wd).is_absolute() {
            return Err(FlameError::InvalidConfig(format!(
                "working_directory must be an absolute path, got: {wd}"
            )));
        }
    }
    Ok(())
}

#[async_trait]
impl Frontend for Flame {
    type WatchTaskStream = Pin<Box<dyn Stream<Item = Result<Task, Status>> + Send>>;
    type ListTaskStream = Pin<Box<dyn Stream<Item = Result<Task, Status>> + Send>>;

    async fn list_task(
        &self,
        req: Request<ListTaskRequest>,
    ) -> Result<Response<Self::ListTaskStream>, Status> {
        trace_fn!("Frontend::list_task");

        let req = req.into_inner();
        let ssn_id = req
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;
        let task_list = self.controller.list_task(ssn_id).map_err(Status::from)?;

        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            for task in task_list {
                if tx.is_closed() {
                    break;
                }

                if let Err(e) = tx.send(Result::<_, Status>::Ok(Task::from(&task))).await {
                    tracing::error!("Failed to send Task <{}>: {e}", task.id);
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::WatchTaskStream
        ))
    }

    async fn register_application(
        &self,
        req: Request<RegisterApplicationRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Frontend::register_application");

        let req = req.into_inner();
        let spec = req.application.ok_or(FlameError::InvalidConfig(
            "applilcation spec is missed".to_string(),
        ))?;

        if let Some(ref schema) = spec.schema {
            if let Some(ref input) = schema.input {
                let input: Value = serde_json::from_str(input)
                    .map_err(|e| FlameError::InvalidConfig(format!("invalid input schema: {e}")))?;
                jsonschema::meta::validate(&input)
                    .map_err(|e| FlameError::InvalidConfig(format!("invalid input schema: {e}")))?;
            }
            if let Some(ref output) = schema.output {
                let output: Value = serde_json::from_str(output).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid output schema: {e}"))
                })?;
                jsonschema::meta::validate(&output).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid output schema: {e}"))
                })?;
            }
            if let Some(ref common_data) = schema.common_data {
                let common_data: Value = serde_json::from_str(common_data).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid common data schema: {e}"))
                })?;
                jsonschema::meta::validate(&common_data).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid common data schema: {e}"))
                })?;
            }
        }

        validate_working_directory(&spec.working_directory)?;

        let res = self
            .controller
            .register_application(req.name, ApplicationAttributes::from(spec))
            .await;

        match res {
            Ok(..) => Ok(Response::new(rpc::Result {
                return_code: 0,
                message: None,
            })),
            Err(e) => Ok(Response::new(rpc::Result {
                return_code: -1,
                message: Some(e.to_string()),
            })),
        }
    }
    async fn unregister_application(
        &self,
        req: Request<UnregisterApplicationRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Frontend::unregister_application");

        let req = req.into_inner();
        let res = self.controller.unregister_application(req.name).await;

        match res {
            Ok(..) => Ok(Response::new(rpc::Result {
                return_code: 0,
                message: None,
            })),
            Err(e) => Ok(Response::new(rpc::Result {
                return_code: -1,
                message: Some(e.to_string()),
            })),
        }
    }

    async fn update_application(
        &self,
        req: Request<UpdateApplicationRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Frontend::update_application");

        let req = req.into_inner();
        let spec = req.application.ok_or(FlameError::InvalidConfig(
            "applilcation spec is missed".to_string(),
        ))?;

        if let Some(ref schema) = spec.schema {
            if let Some(ref input) = schema.input {
                let input: Value = serde_json::from_str(input)
                    .map_err(|e| FlameError::InvalidConfig(format!("invalid input schema: {e}")))?;
                jsonschema::meta::validate(&input)
                    .map_err(|e| FlameError::InvalidConfig(format!("invalid input schema: {e}")))?;
            }
            if let Some(ref output) = schema.output {
                let output: Value = serde_json::from_str(output).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid output schema: {e}"))
                })?;
                jsonschema::meta::validate(&output).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid output schema: {e}"))
                })?;
            }
            if let Some(ref common_data) = schema.common_data {
                let common_data: Value = serde_json::from_str(common_data).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid common data schema: {e}"))
                })?;
                jsonschema::meta::validate(&common_data).map_err(|e| {
                    FlameError::InvalidConfig(format!("invalid common data schema: {e}"))
                })?;
            }
        }

        validate_working_directory(&spec.working_directory)?;

        let res = self
            .controller
            .update_application(req.name, ApplicationAttributes::from(spec))
            .await;

        match res {
            Ok(..) => Ok(Response::new(rpc::Result {
                return_code: 0,
                message: None,
            })),
            Err(e) => Ok(Response::new(rpc::Result {
                return_code: -1,
                message: Some(e.to_string()),
            })),
        }
    }

    async fn get_application(
        &self,
        req: tonic::Request<GetApplicationRequest>,
    ) -> Result<Response<rpc::Application>, Status> {
        trace_fn!("Frontend::get_application");

        let app = self
            .controller
            .get_application(req.into_inner().name)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(rpc::Application::from(&app)))
    }

    async fn list_application(
        &self,
        req: Request<ListApplicationRequest>,
    ) -> Result<Response<ApplicationList>, Status> {
        trace_fn!("Frontend::list_application");

        let app_list = self
            .controller
            .list_application()
            .await
            .map_err(Status::from)?;

        let applications = app_list.iter().map(rpc::Application::from).collect();

        Ok(Response::new(ApplicationList { applications }))
    }

    async fn list_executor(
        &self,
        req: tonic::Request<ListExecutorRequest>,
    ) -> Result<Response<ExecutorList>, Status> {
        trace_fn!("Frontend::list_executor");

        let executor_list = self.controller.list_executor().map_err(Status::from)?;
        let executors = executor_list.iter().map(rpc::Executor::from).collect();
        Ok(Response::new(ExecutorList { executors }))
    }

    async fn list_nodes(
        &self,
        req: tonic::Request<ListNodesRequest>,
    ) -> Result<Response<NodeList>, Status> {
        trace_fn!("Frontend::list_nodes");

        let node_list = self.controller.list_node().map_err(Status::from)?;
        let nodes = node_list.iter().map(rpc::Node::from).collect();
        Ok(Response::new(NodeList { nodes }))
    }

    async fn get_node(
        &self,
        req: tonic::Request<GetNodeRequest>,
    ) -> Result<Response<GetNodeResponse>, Status> {
        trace_fn!("Frontend::get_node");

        let name = req.into_inner().name;
        let node = self
            .controller
            .get_node(&name)
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("node <{}> not found", name)))?;
        Ok(Response::new(GetNodeResponse {
            node: Some(rpc::Node::from(node)),
        }))
    }

    async fn create_session(
        &self,
        req: Request<CreateSessionRequest>,
    ) -> Result<Response<Session>, Status> {
        trace_fn!("Frontend::create_session");

        let req = req.into_inner();
        let ssn_spec = req
            .session
            .ok_or(Status::invalid_argument("session spec"))?;
        let ssn_id = req
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let attr = SessionAttributes {
            id: ssn_id,
            application: ssn_spec.application,
            slots: ssn_spec.slots,
            common_data: ssn_spec.common_data.map(apis::CommonData::from),
            min_instances: ssn_spec.min_instances,
            max_instances: ssn_spec.max_instances,
        };

        tracing::debug!(
            "Creating session with attributes: id={}, application={}, slots={}, min_instances={}, max_instances={:?}",
            attr.id,
            attr.application,
            attr.slots,
            attr.min_instances,
            attr.max_instances
        );

        let ssn = self
            .controller
            .create_session(attr)
            .await
            .map(Session::from)
            .map_err(Status::from)?;

        Ok(Response::new(ssn))
    }

    async fn delete_session(
        &self,
        req: Request<DeleteSessionRequest>,
    ) -> Result<Response<rpc::Session>, Status> {
        trace_fn!("Frontend::delete_session");

        let ssn_id = req
            .into_inner()
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let ssn = self
            .controller
            .delete_session(ssn_id)
            .await
            .map(Session::from)?;

        Ok(Response::new(ssn))
    }

    async fn open_session(
        &self,
        req: Request<OpenSessionRequest>,
    ) -> Result<Response<rpc::Session>, Status> {
        trace_fn!("Frontend::open_session");

        let req = req.into_inner();
        let ssn_id = req
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        // Convert optional SessionSpec to SessionAttributes
        let spec = req.session.map(|ssn_spec| SessionAttributes {
            id: ssn_id.clone(),
            application: ssn_spec.application,
            slots: ssn_spec.slots,
            common_data: ssn_spec.common_data.map(apis::CommonData::from),
            min_instances: ssn_spec.min_instances,
            max_instances: ssn_spec.max_instances,
        });

        let ssn = self
            .controller
            .open_session(ssn_id, spec)
            .await
            .map(Session::from)
            .map_err(Status::from)?;

        Ok(Response::new(ssn))
    }

    async fn close_session(
        &self,
        req: Request<CloseSessionRequest>,
    ) -> Result<Response<rpc::Session>, Status> {
        trace_fn!("Frontend::close_session");

        let ssn_id = req
            .into_inner()
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let ssn = self
            .controller
            .close_session(ssn_id)
            .await
            .map(rpc::Session::from)
            .map_err(Status::from)?;

        Ok(Response::new(ssn))
    }

    async fn get_session(
        &self,
        req: Request<GetSessionRequest>,
    ) -> Result<Response<Session>, Status> {
        trace_fn!("Frontend::get_session");

        let ssn_id = req
            .into_inner()
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let ssn = self
            .controller
            .get_session(ssn_id)
            .map(rpc::Session::from)
            .map_err(Status::from)?;

        Ok(Response::new(ssn))
    }
    async fn list_session(
        &self,
        req: Request<ListSessionRequest>,
    ) -> Result<Response<SessionList>, Status> {
        trace_fn!("Frontend::list_session");

        let ssn_list = self.controller.list_session().map_err(Status::from)?;

        let sessions = ssn_list.iter().map(Session::from).collect();

        Ok(Response::new(SessionList { sessions }))
    }

    async fn create_task(&self, req: Request<CreateTaskRequest>) -> Result<Response<Task>, Status> {
        trace_fn!("Frontend::create_task");

        let task_spec = req
            .into_inner()
            .task
            .ok_or(Status::invalid_argument("session spec"))?;
        let ssn_id = task_spec
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let task = self
            .controller
            .create_task(ssn_id, task_spec.input.map(apis::TaskInput::from))
            .await
            .map(Task::from)
            .map_err(Status::from)?;

        Ok(Response::new(task))
    }
    async fn delete_task(
        &self,
        req: Request<DeleteTaskRequest>,
    ) -> Result<Response<rpc::Task>, Status> {
        todo!()
    }

    async fn watch_task(
        &self,
        req: Request<WatchTaskRequest>,
    ) -> Result<Response<Self::WatchTaskStream>, Status> {
        trace_fn!("Frontend::watch_task");

        let req = req.into_inner();
        let gid = apis::TaskGID {
            ssn_id: req
                .session_id
                .parse::<apis::SessionID>()
                .map_err(|_| Status::invalid_argument("invalid session id"))?,

            task_id: req
                .task_id
                .parse::<apis::TaskID>()
                .map_err(|_| Status::invalid_argument("invalid task id"))?,
        };

        let (tx, rx) = mpsc::channel(128);

        let controller = self.controller.clone();
        tokio::spawn(async move {
            loop {
                match controller.watch_task(gid.clone()).await {
                    Ok(task) => {
                        tracing::debug!("Task <{}> state is <{}>", task.id, task.state as i32);
                        if let Err(e) = tx.send(Result::<_, Status>::Ok(Task::from(&task))).await {
                            tracing::debug!("Failed to send Task <{gid}>: {e}");
                            break;
                        }
                        if task.is_completed() {
                            tracing::debug!("Task <{}> is completed, exit.", task.id);
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to watch Task <{gid}>: {e}");
                        break;
                    }
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::WatchTaskStream
        ))
    }

    async fn get_task(&self, req: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
        trace_fn!("Frontend::get_task");

        let req = req.into_inner();
        let ssn_id = req
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let task_id = req
            .task_id
            .parse::<apis::TaskID>()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;

        let task = self
            .controller
            .get_task(ssn_id, task_id)
            .map(Task::from)
            .map_err(Status::from)?;

        Ok(Response::new(task))
    }

    async fn create_workspace(
        &self,
        req: Request<CreateWorkspaceRequest>,
    ) -> Result<Response<rpc::Workspace>, Status> {
        trace_fn!("Frontend::create_workspace");

        let req = req.into_inner();
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("workspace spec is required"))?;

        if req.name.is_empty() {
            return Err(Status::invalid_argument("workspace name is required"));
        }

        validate_workspace_name(&req.name).map_err(|e| Status::invalid_argument(e.to_string()))?;

        let workspace = Workspace {
            name: req.name,
            description: if spec.description.is_empty() {
                None
            } else {
                Some(spec.description)
            },
            labels: spec.labels,
            creation_time: chrono::Utc::now(),
        };

        let workspace = self
            .controller
            .create_workspace(&workspace)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Workspace::from(workspace)))
    }

    async fn get_workspace(
        &self,
        req: Request<GetWorkspaceRequest>,
    ) -> Result<Response<rpc::Workspace>, Status> {
        trace_fn!("Frontend::get_workspace");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("workspace name is required"));
        }

        let workspace = self
            .controller
            .get_workspace(&req.name)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("workspace '{}' not found", req.name)))?;

        Ok(Response::new(rpc::Workspace::from(workspace)))
    }

    async fn update_workspace(
        &self,
        req: Request<UpdateWorkspaceRequest>,
    ) -> Result<Response<rpc::Workspace>, Status> {
        trace_fn!("Frontend::update_workspace");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("workspace name is required"));
        }

        let existing = self
            .controller
            .get_workspace(&req.name)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("workspace '{}' not found", req.name)))?;

        let spec = req.spec.unwrap_or_default();

        let workspace = Workspace {
            name: existing.name,
            description: if spec.description.is_empty() {
                existing.description
            } else {
                Some(spec.description)
            },
            labels: if spec.labels.is_empty() {
                existing.labels
            } else {
                spec.labels
            },
            creation_time: existing.creation_time,
        };

        let workspace = self
            .controller
            .update_workspace(&workspace)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Workspace::from(workspace)))
    }

    async fn delete_workspace(
        &self,
        req: Request<DeleteWorkspaceRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Frontend::delete_workspace");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("workspace name is required"));
        }

        self.controller
            .delete_workspace(&req.name)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Result {
            return_code: 0,
            message: None,
        }))
    }

    async fn list_workspaces(
        &self,
        req: Request<ListWorkspacesRequest>,
    ) -> Result<Response<rpc::WorkspaceList>, Status> {
        trace_fn!("Frontend::list_workspaces");

        let workspaces = self
            .controller
            .list_workspaces()
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::WorkspaceList {
            workspaces: workspaces.into_iter().map(rpc::Workspace::from).collect(),
        }))
    }
}

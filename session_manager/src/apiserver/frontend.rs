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
use std::collections::HashSet;
use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use common::apis::{ApplicationAttributes, SessionAttributes, Signal};
use futures::Stream;
use serde_json::Value;
use stdng::trace_fn;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use self::rpc::frontend_server::Frontend;
use self::rpc::{
    ApplicationList, CloseSessionRequest, CreateSessionRequest, CreateTaskRequest,
    DeleteNodeRequest, DeleteSessionRequest, ExecutorList, GetApplicationRequest, GetNodeRequest,
    GetNodeResponse, GetSessionRequest, GetTaskRequest, ListApplicationsRequest,
    ListExecutorsRequest, ListNodesRequest, ListSessionsRequest, ListTasksRequest, NodeList,
    OpenSessionRequest, RegisterApplicationRequest, ResumeSessionRequest, Session, SessionList,
    SuspendSessionRequest, Task, UnregisterApplicationRequest, UpdateApplicationRequest,
    WatchTaskRequest,
};

use rpc::flame::v1 as rpc;

use common::apis::ResourceRequirement;
use common::{apis, FlameError};

use crate::apiserver::Flame;

/// Hardcoded safety-net default `resreq` applied when a session spec supplies
/// no explicit `resreq` AND `cluster.resreq` is unset.
/// 1 CPU, 1 GiB memory, 0 GPU.
///
/// Production deployments should configure `cluster.resreq`
/// explicitly so that defaults are auditable from the cluster config.
const DEFAULT_FALLBACK_RESREQ: ResourceRequirement = ResourceRequirement {
    cpu: 1,
    memory: 1024 * 1024 * 1024, // 1 GiB
    gpu: 0,
};

/// Resolve the effective `resreq` for a new session. With `slots` fully removed
/// from the API, the resolution chain is:
///
/// 1. `explicit.is_some()` → use the client-supplied resreq verbatim.
/// 2. `explicit.is_none() && cluster_default.is_some()` → apply the cluster
///    default (`cluster.resreq` from `flame-cluster.yaml`) and
///    log at `info`.
/// 3. Otherwise → apply the hardcoded `DEFAULT_FALLBACK_RESREQ` and log at
///    `info`. Production deployments should configure
///    `cluster.resreq` so case 3 never fires.
///
/// Always returns a concrete `ResourceRequirement` — `SessionAttributes.resreq`
/// is unconditionally populated by the time it leaves this function, which is
/// the invariant the scheduler plugins rely on.
fn resolve_session_resreq(
    explicit: Option<ResourceRequirement>,
    cluster_default: Option<&ResourceRequirement>,
) -> ResourceRequirement {
    if let Some(rr) = explicit {
        return rr;
    }
    if let Some(default) = cluster_default {
        tracing::info!("applying cluster.resreq default to session: {:?}", default);
        return default.clone();
    }
    tracing::info!(
        "no resreq supplied and no cluster default; applying hardcoded fallback: {:?}",
        DEFAULT_FALLBACK_RESREQ
    );
    DEFAULT_FALLBACK_RESREQ
}

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

impl Flame {
    async fn forward_task_update(
        &self,
        ssn_id: &apis::SessionID,
        task_id: apis::TaskID,
        registered_tasks: &mut HashSet<apis::TaskID>,
        tx: &mpsc::Sender<Result<Task, Status>>,
    ) -> Result<(), Status> {
        let mut task = self
            .controller
            .get_task_metadata(ssn_id.clone(), task_id)
            .await
            .map_err(Status::from)?;
        if task.is_completed() {
            // Terminal updates carry failure details in task events. Load those
            // once, after the state snapshot, instead of on every watch update.
            task = self
                .controller
                .get_task(ssn_id.clone(), task_id)
                .await
                .map_err(Status::from)?;
            registered_tasks.remove(&task_id);
        }
        tx.send(Ok(Task::from(&task)))
            .await
            .map_err(|_| Status::cancelled("task watch stream closed"))?;
        Ok(())
    }
}

#[async_trait]
impl Frontend for Flame {
    type WatchTasksStream = Pin<Box<dyn Stream<Item = Result<Task, Status>> + Send>>;
    type ListTasksStream = Pin<Box<dyn Stream<Item = Result<Task, Status>> + Send>>;

    async fn watch_tasks(
        &self,
        req: Request<tonic::Streaming<WatchTaskRequest>>,
    ) -> Result<Response<Self::WatchTasksStream>, Status> {
        let mut watch_requests = req.into_inner();
        let first = watch_requests
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("at least one task is required"))?;
        let ssn_id = first
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;
        // Subscribe before the first snapshot so no task update is lost.
        let mut task_updates = self.controller.subscribe(&ssn_id).map_err(Status::from)?;
        let (tx, rx) = mpsc::channel(128);
        let mut registered_tasks = HashSet::new();
        let task_id = first
            .task_id
            .parse::<apis::TaskID>()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;
        registered_tasks.insert(task_id);
        self.forward_task_update(&ssn_id, task_id, &mut registered_tasks, &tx)
            .await?;
        let flame = self.clone();
        tokio::spawn(async move {
            let result: Result<(), Status> = async {
                let mut requests_closed = false;
                loop {
                    if requests_closed && registered_tasks.is_empty() {
                        return Ok(());
                    }
                    tokio::select! {
                        _ = tx.closed() => return Ok(()),
                        watch_request = watch_requests.message(), if !requests_closed => {
                            match watch_request? {
                                Some(watch_request) => {
                                    if watch_request.session_id != ssn_id {
                                        return Err(Status::invalid_argument("all watched tasks must be in one session"));
                                    }
                                    let task_id = watch_request.task_id.parse::<apis::TaskID>()
                                        .map_err(|_| Status::invalid_argument("invalid task id"))?;
                                    registered_tasks.insert(task_id);
                                    flame.forward_task_update(&ssn_id, task_id, &mut registered_tasks, &tx).await?;
                                }
                                None => requests_closed = true,
                            }
                        }
                        task_update = task_updates.recv() => {
                            match task_update {
                                Ok(task_id) => {
                                    if registered_tasks.contains(&task_id) {
                                        flame.forward_task_update(&ssn_id, task_id, &mut registered_tasks, &tx).await?;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    // Reconcile registered tasks after missing updates.
                                    for task_id in registered_tasks.iter().copied().collect::<Vec<_>>() {
                                        flame.forward_task_update(&ssn_id, task_id, &mut registered_tasks, &tx).await?;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    // Send final states for registered tasks before ending the watch.
                                    for task_id in registered_tasks.iter().copied().collect::<Vec<_>>() {
                                        flame.forward_task_update(&ssn_id, task_id, &mut registered_tasks, &tx).await?;
                                    }
                                    return Err(Status::not_found("session task watch closed"));
                                }
                            }
                        }
                    }
                }
            }.await;
            if let Err(status) = result {
                let _ = tx.send(Err(status)).await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn list_tasks(
        &self,
        req: Request<ListTasksRequest>,
    ) -> Result<Response<Self::ListTasksStream>, Status> {
        trace_fn!("Frontend::list_tasks");
        let req = req.into_inner();
        let ssn_id = req
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;
        let task_list = self
            .controller
            .list_tasks(ssn_id)
            .await
            .map_err(Status::from)?;
        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            for task in task_list {
                if tx.send(Ok(Task::from(&task))).await.is_err() {
                    return;
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::ListTasksStream
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

    async fn list_applications(
        &self,
        request: Request<ListApplicationsRequest>,
    ) -> Result<Response<ApplicationList>, Status> {
        trace_fn!("Frontend::list_applications");
        let filter = crate::model::ApplicationFilter::try_from(request.into_inner())
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let app_list = self
            .controller
            .list_applications(Some(&filter))
            .await
            .map_err(Status::from)?;

        let applications = app_list.iter().map(rpc::Application::from).collect();

        Ok(Response::new(ApplicationList { applications }))
    }

    async fn list_executors(
        &self,
        _: tonic::Request<ListExecutorsRequest>,
    ) -> Result<Response<ExecutorList>, Status> {
        trace_fn!("Frontend::list_executors");
        let executor_list = self.controller.list_executors().map_err(Status::from)?;
        let executors = executor_list.iter().map(rpc::Executor::from).collect();
        Ok(Response::new(ExecutorList { executors }))
    }

    async fn list_nodes(
        &self,
        _: tonic::Request<ListNodesRequest>,
    ) -> Result<Response<NodeList>, Status> {
        trace_fn!("Frontend::list_nodes");
        let node_list = self.controller.list_nodes().map_err(Status::from)?;
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

    async fn delete_node(
        &self,
        req: Request<DeleteNodeRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Frontend::delete_node");
        self.controller
            .release_node(&req.into_inner().name)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(rpc::Result {
            return_code: 0,
            message: None,
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

        let explicit = ssn_spec.resreq.map(apis::ResourceRequirement::from);
        let resreq = resolve_session_resreq(explicit, self.cluster_default_resreq.as_ref());

        let attr = SessionAttributes {
            id: ssn_id,
            application: ssn_spec.application,
            common_data: ssn_spec.common_data.map(apis::CommonData::from),
            tokens: ssn_spec.tokens,
            min_instances: ssn_spec.min_instances,
            max_instances: ssn_spec.max_instances,
            batch_size: ssn_spec.batch_size.max(1),
            priority: ssn_spec.priority,
            resreq: Some(resreq),
        };

        tracing::debug!(
            "Creating session with attributes: id={}, application={}, resreq={:?}, min_instances={}, max_instances={:?}, batch_size={}, priority={}",
            attr.id,
            attr.application,
            attr.resreq,
            attr.min_instances,
            attr.max_instances,
            attr.batch_size,
            attr.priority,
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

        let spec = match req.session {
            Some(ssn_spec) => {
                let explicit = ssn_spec.resreq.map(apis::ResourceRequirement::from);
                let resreq = resolve_session_resreq(explicit, self.cluster_default_resreq.as_ref());

                Some(SessionAttributes {
                    id: ssn_id.clone(),
                    application: ssn_spec.application,
                    common_data: ssn_spec.common_data.map(apis::CommonData::from),
                    tokens: ssn_spec.tokens,
                    min_instances: ssn_spec.min_instances,
                    max_instances: ssn_spec.max_instances,
                    batch_size: ssn_spec.batch_size.max(1),
                    priority: ssn_spec.priority,
                    resreq: Some(resreq),
                })
            }
            None => None,
        };

        let ssn = self
            .controller
            .open_session(ssn_id, spec)
            .await
            .map(Session::from)
            .map_err(Status::from)?;

        Ok(Response::new(ssn))
    }

    async fn suspend_session(
        &self,
        req: Request<SuspendSessionRequest>,
    ) -> Result<Response<rpc::Session>, Status> {
        trace_fn!("Frontend::suspend_session");
        let ssn_id = req
            .into_inner()
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let ssn = self
            .controller
            .signal_session(ssn_id, Signal::Suspend)
            .await
            .map(rpc::Session::from)
            .map_err(Status::from)?;

        Ok(Response::new(ssn))
    }

    async fn resume_session(
        &self,
        req: Request<ResumeSessionRequest>,
    ) -> Result<Response<rpc::Session>, Status> {
        trace_fn!("Frontend::resume_session");
        let ssn_id = req
            .into_inner()
            .session_id
            .parse::<apis::SessionID>()
            .map_err(|_| Status::invalid_argument("invalid session id"))?;

        let ssn = self
            .controller
            .resume_session(ssn_id)
            .await
            .map(rpc::Session::from)
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
    async fn list_sessions(
        &self,
        request: Request<ListSessionsRequest>,
    ) -> Result<Response<SessionList>, Status> {
        trace_fn!("Frontend::list_sessions");
        let filter = crate::model::SessionFilter::try_from(request.into_inner())
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let ssn_list = self
            .controller
            .list_sessions(Some(&filter))
            .map_err(Status::from)?;

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
            .create_task(
                ssn_id,
                task_spec.input.map(apis::TaskInput::from),
                Some(apis::TaskOptions {
                    affinity: task_spec
                        .affinity
                        .into_iter()
                        .map(bytes::Bytes::from)
                        .collect(),
                }),
            )
            .await
            .map(Task::from)
            .map_err(Status::from)?;

        Ok(Response::new(task))
    }
    async fn get_task(&self, req: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
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
            .await
            .map(Task::from)
            .map_err(Status::from)?;

        Ok(Response::new(task))
    }
}

#[cfg(test)]
mod tests;

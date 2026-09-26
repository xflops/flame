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
    DeleteSessionRequest, ExecutorList, GetApplicationRequest, GetNodeRequest, GetNodeResponse,
    GetSessionRequest, GetTaskRequest, ListApplicationsRequest, ListExecutorsRequest,
    ListNodesRequest, ListSessionsRequest, ListTasksRequest, NodeList, OpenSessionRequest,
    RegisterApplicationRequest, Session, SessionList, Task, UnregisterApplicationRequest,
    UpdateApplicationRequest, WatchTaskRequest,
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
            .map_err(Status::from)?;
        if task.is_completed() {
            // Terminal updates carry failure details in task events. Load those
            // once, after the state snapshot, instead of on every watch update.
            task = self
                .controller
                .get_task(ssn_id.clone(), task_id)
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
        let task_list = self.controller.list_tasks(ssn_id).map_err(Status::from)?;

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
            min_instances: ssn_spec.min_instances,
            max_instances: ssn_spec.max_instances,
            batch_size: 1,
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
                    min_instances: ssn_spec.min_instances,
                    max_instances: ssn_spec.max_instances,
                    batch_size: 1,
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
            .map(Task::from)
            .map_err(Status::from)?;

        Ok(Response::new(task))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{Stream, StreamExt};

    async fn watch_task_stream<S>(
        controller: crate::controller::ControllerPtr,
        watch_requests: S,
        tx: mpsc::Sender<Result<Task, Status>>,
    ) -> Result<(), Status>
    where
        S: Stream<Item = Result<WatchTaskRequest, Status>> + Unpin + Send + 'static,
    {
        let flame = Flame {
            controller,
            cluster_default_resreq: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        let address = listener.local_addr().unwrap();
        let incoming = futures::stream::unfold(listener, |listener| async move {
            let (socket, _) = listener.accept().await.ok()?;
            Some((Ok::<_, std::io::Error>(socket), listener))
        });
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(rpc::frontend_server::FrontendServer::new(flame))
                .serve_with_incoming(Box::pin(incoming)),
        );
        let result = async {
                let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                    .map_err(|error| Status::internal(error.to_string()))?;
                let channel = endpoint
                    .connect()
                    .await
                    .map_err(|error| Status::internal(error.to_string()))?;
                let mut client = rpc::frontend_client::FrontendClient::new(channel);
                let response = tokio::select! {
                    _ = tx.closed() => return Ok(()),
                    response = client.watch_tasks(watch_requests.map(|watch_request| watch_request.unwrap())) => response?,
                };
                let mut task_updates = response.into_inner();
                loop {
                    tokio::select! {
                        _ = tx.closed() => return Ok(()),
                        task_update = task_updates.message() => match task_update? {
                            Some(task_update) => tx.send(Ok(task_update)).await
                                .map_err(|_| Status::cancelled("task watch stream closed"))?,
                            None => return Ok(()),
                        },
                    }
                }
            }
            .await;
        server.abort();
        result
    }

    async fn watch_test_storage_and_controller(
    ) -> (crate::storage::StoragePtr, crate::controller::ControllerPtr) {
        let config = common::ctx::FlameClusterContext {
            cluster: common::ctx::FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        let controller = crate::controller::new_ptr(storage.clone());
        controller
            .register_application("test-app".to_string(), ApplicationAttributes::default())
            .await
            .unwrap();
        controller
            .create_session(SessionAttributes {
                id: "watch-test-session".to_string(),
                application: "test-app".to_string(),
                resreq: Some(ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        (storage, controller)
    }

    async fn watch_test_controller() -> crate::controller::ControllerPtr {
        let (_, controller) = watch_test_storage_and_controller().await;
        controller
    }

    fn watch_request(task_id: apis::TaskID) -> WatchTaskRequest {
        WatchTaskRequest {
            session_id: "watch-test-session".to_string(),
            task_id: task_id.to_string(),
        }
    }

    type WatchRequest = Result<WatchTaskRequest, Status>;

    fn watch_requests(
        task_id: apis::TaskID,
    ) -> (mpsc::Sender<WatchRequest>, ReceiverStream<WatchRequest>) {
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(Ok(watch_request(task_id))).unwrap();
        (tx, ReceiverStream::new(rx))
    }

    #[tokio::test]
    async fn watch_tasks_omits_event_history_until_terminal_state() {
        let controller = watch_test_controller().await;
        let task = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        assert!(!controller
            .get_task("watch-test-session".to_string(), task.id)
            .unwrap()
            .events
            .is_empty());

        let (_requests_tx, requests) = watch_requests(task.id);
        let (tx, mut rx) = mpsc::channel(1);
        let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
        let reported = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let status = reported.status.unwrap();
        assert_eq!(status.state, rpc::TaskState::Pending as i32);
        assert!(status.events.is_empty());
        drop(rx);
        watcher.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn watch_tasks_includes_failed_task_event_message() {
        use common::apis::{TaskGID, TaskResult, TaskState};

        let (storage, controller) = watch_test_storage_and_controller().await;
        let task = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        let ssn_ptr = storage
            .get_session_ptr("watch-test-session".to_string())
            .unwrap();
        let task_ptr = storage
            .get_task_ptr(TaskGID {
                ssn_id: "watch-test-session".to_string(),
                task_id: task.id,
            })
            .unwrap();
        storage
            .update_task_result(
                ssn_ptr,
                task_ptr,
                TaskResult {
                    state: TaskState::Failed,
                    message: Some("task failed on worker".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let (_requests_tx, requests) = watch_requests(task.id);
        let (tx, mut rx) = mpsc::channel(1);
        let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
        let reported = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let status = reported.status.unwrap();
        assert_eq!(status.state, rpc::TaskState::Failed as i32);
        assert!(status
            .events
            .iter()
            .any(|event| { event.message.as_deref() == Some("task failed on worker") }));
        drop(rx);
        watcher.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn watch_tasks_rejects_unknown_task() {
        let controller = watch_test_controller().await;
        let (_requests_tx, requests) = watch_requests(999);
        let (tx, _rx) = mpsc::channel(1);
        let result = watch_task_stream(controller, requests, tx).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn watch_tasks_rejects_empty_registration_stream() {
        let controller = watch_test_controller().await;
        let (_requests_tx, requests_rx) = mpsc::channel::<Result<WatchTaskRequest, Status>>(1);
        drop(_requests_tx);
        let (tx, _rx) = mpsc::channel(1);
        let result = watch_task_stream(controller, ReceiverStream::new(requests_rx), tx).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn watch_tasks_rejects_registration_after_session_close() {
        let controller = watch_test_controller().await;
        let task = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        controller
            .close_session("watch-test-session".to_string())
            .await
            .unwrap();

        let (_requests_tx, requests) = watch_requests(task.id);
        let (tx, _rx) = mpsc::channel(1);
        let result = watch_task_stream(controller, requests, tx).await;
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(status.message(), "session is closed");
    }

    #[tokio::test]
    async fn watch_tasks_reports_terminal_state_then_closes_on_session_close() {
        let controller = watch_test_controller().await;
        let task = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        let (_requests_tx, requests) = watch_requests(task.id);
        let (tx, mut rx) = mpsc::channel(2);
        let watcher = tokio::spawn(watch_task_stream(controller.clone(), requests, tx));
        rx.recv().await.unwrap().unwrap();

        controller
            .close_session("watch-test-session".to_string())
            .await
            .unwrap();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal.status.unwrap().state,
            rpc::TaskState::Cancelled as i32
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
            .await
            .unwrap()
            .unwrap();
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(status.message(), "session task watch closed");
    }

    #[tokio::test]
    async fn watch_tasks_reports_only_registered_ids_on_one_stream() {
        let controller = watch_test_controller().await;
        let first = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        let second = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        let _unregistered = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();

        let (requests_tx, requests) = watch_requests(first.id);
        let (tx, mut rx) = mpsc::channel(2);
        let watcher = tokio::spawn(watch_task_stream(controller.clone(), requests, tx));
        let initial = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(initial.metadata.unwrap().id, first.id.to_string());
        assert_eq!(
            initial.status.unwrap().state,
            rpc::TaskState::Pending as i32
        );
        // The second task exists in the session but has not been registered.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );

        requests_tx.send(Ok(watch_request(first.id))).await.unwrap();
        let repeated_snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(repeated_snapshot.metadata.unwrap().id, first.id.to_string());
        assert_eq!(
            repeated_snapshot.status.unwrap().state,
            rpc::TaskState::Pending as i32
        );

        requests_tx
            .send(Ok(watch_request(second.id)))
            .await
            .unwrap();
        drop(requests_tx);
        let second_initial = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(second_initial.metadata.unwrap().id, second.id.to_string());
        assert_eq!(
            second_initial.status.unwrap().state,
            rpc::TaskState::Pending as i32
        );

        controller
            .close_session("watch-test-session".to_string())
            .await
            .unwrap();
        let mut completed = HashSet::new();
        for _ in 0..2 {
            let reported = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                reported.status.unwrap().state,
                rpc::TaskState::Cancelled as i32
            );
            completed.insert(reported.metadata.unwrap().id);
        }
        assert_eq!(
            completed,
            HashSet::from([first.id.to_string(), second.id.to_string()])
        );
        let status = watcher.await.unwrap().unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(status.message(), "session task watch closed");
    }

    #[tokio::test]
    async fn watch_tasks_stops_when_response_stream_is_dropped() {
        let controller = watch_test_controller().await;
        let task = controller
            .create_task("watch-test-session".to_string(), None, None)
            .await
            .unwrap();
        let (_requests_tx, requests) = watch_requests(task.id);
        let (tx, rx) = mpsc::channel(1);
        let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
        tokio::task::yield_now().await;
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
            .await
            .expect("watch must stop after response stream closes")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn watch_tasks_stops_before_first_request_when_response_stream_is_dropped() {
        let controller = watch_test_controller().await;
        let (_requests_tx, requests_rx) = mpsc::channel::<Result<WatchTaskRequest, Status>>(1);
        let (tx, rx) = mpsc::channel(1);
        let watcher = tokio::spawn(watch_task_stream(
            controller,
            ReceiverStream::new(requests_rx),
            tx,
        ));
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
            .await
            .expect("watch must stop before its first request when response stream closes")
            .unwrap()
            .unwrap();
    }

    fn rr(cpu: u64, memory: u64, gpu: i32) -> ResourceRequirement {
        ResourceRequirement { cpu, memory, gpu }
    }

    #[test]
    fn explicit_resreq_returned_verbatim() {
        let explicit = rr(2, 2 * 1024 * 1024 * 1024, 1);
        let cluster_default = Some(rr(4, 8 * 1024 * 1024 * 1024, 0));
        let res = resolve_session_resreq(Some(explicit.clone()), cluster_default.as_ref());
        assert_eq!(
            res, explicit,
            "explicit resreq must be returned verbatim and override the cluster default"
        );
    }

    #[test]
    fn cluster_default_used_when_explicit_unset() {
        let cluster_default = rr(4, 8 * 1024 * 1024 * 1024, 2);
        let res = resolve_session_resreq(None, Some(&cluster_default));
        assert_eq!(
            res, cluster_default,
            "should clone the cluster default when no explicit resreq is supplied"
        );
    }

    #[test]
    fn hardcoded_fallback_when_no_cluster_default() {
        let res = resolve_session_resreq(None, None);
        assert_eq!(
            res, DEFAULT_FALLBACK_RESREQ,
            "should yield the hardcoded fallback (cpu=1, mem=1GiB, gpu=0)"
        );
    }
}

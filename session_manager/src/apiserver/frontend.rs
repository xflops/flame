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
    DeleteSessionRequest, ExecutorList, GetApplicationRequest, GetNodeRequest, GetNodeResponse,
    GetSessionRequest, GetTaskRequest, ListApplicationRequest, ListExecutorRequest,
    ListNodesRequest, ListSessionRequest, ListTaskRequest, NodeList, OpenSessionRequest,
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

/// Validates that min_instances and max_instances are batch-aligned when batch_size > 1.
fn validate_batch_alignment(
    batch_size: u32,
    min_instances: u32,
    max_instances: Option<u32>,
) -> Result<(), FlameError> {
    let batch_size = batch_size.max(1);

    if batch_size == 1 {
        return Ok(());
    }

    if min_instances != 0 && !min_instances.is_multiple_of(batch_size) {
        return Err(FlameError::InvalidConfig(format!(
            "min_instances ({}) must be 0 or a multiple of batch_size ({})",
            min_instances, batch_size
        )));
    }

    if let Some(max) = max_instances {
        if !max.is_multiple_of(batch_size) {
            return Err(FlameError::InvalidConfig(format!(
                "max_instances ({}) must be a multiple of batch_size ({})",
                max, batch_size
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
        _: Request<ListApplicationRequest>,
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
        _: tonic::Request<ListExecutorRequest>,
    ) -> Result<Response<ExecutorList>, Status> {
        trace_fn!("Frontend::list_executor");
        let executor_list = self.controller.list_executor().map_err(Status::from)?;
        let executors = executor_list.iter().map(rpc::Executor::from).collect();
        Ok(Response::new(ExecutorList { executors }))
    }

    async fn list_nodes(
        &self,
        _: tonic::Request<ListNodesRequest>,
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

        validate_batch_alignment(
            ssn_spec.batch_size,
            ssn_spec.min_instances,
            ssn_spec.max_instances,
        )
        .map_err(Status::from)?;

        let explicit = ssn_spec.resreq.map(apis::ResourceRequirement::from);
        let resreq = resolve_session_resreq(explicit, self.cluster_default_resreq.as_ref());

        let attr = SessionAttributes {
            id: ssn_id,
            application: ssn_spec.application,
            common_data: ssn_spec.common_data.map(apis::CommonData::from),
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
                validate_batch_alignment(
                    ssn_spec.batch_size,
                    ssn_spec.min_instances,
                    ssn_spec.max_instances,
                )
                .map_err(Status::from)?;

                let explicit = ssn_spec.resreq.map(apis::ResourceRequirement::from);
                let resreq = resolve_session_resreq(explicit, self.cluster_default_resreq.as_ref());

                Some(SessionAttributes {
                    id: ssn_id.clone(),
                    application: ssn_spec.application,
                    common_data: ssn_spec.common_data.map(apis::CommonData::from),
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
        _: Request<ListSessionRequest>,
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
    async fn watch_task(
        &self,
        req: Request<WatchTaskRequest>,
    ) -> Result<Response<Self::WatchTaskStream>, Status> {
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

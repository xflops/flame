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

use async_trait::async_trait;
use chrono::Utc;
use stdng::{logs::TraceFn, trace_fn};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use self::rpc::backend_server::Backend;
use self::rpc::{
    BindExecutorCompletedRequest, BindExecutorRequest, BindExecutorResponse, CompleteTaskRequest,
    LaunchTaskRequest, LaunchTaskResponse, RegisterExecutorRequest, RegisterNodeRequest,
    ReleaseNodeRequest, SyncNodeRequest, SyncNodeResponse, UnbindExecutorCompletedRequest,
    UnbindExecutorRequest, UnregisterExecutorRequest, WatchNodeRequest, WatchNodeResponse,
};
use ::rpc::flame::v1 as rpc;

use crate::apiserver::Flame;
use crate::controller::ControllerPtr;
use crate::model::Executor;
use common::apis::UserIdentity as Identity;
use common::apis::{operation, ExecutorState, FlameResult, Node, Object, Shim, TaskResult};
use common::FlameError;
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};

/// Timeout for heartbeat in seconds. If no heartbeat is received within this
/// duration, the stream is considered stale and will be closed.
const HEARTBEAT_TIMEOUT_SECS: u64 = 15;

impl Flame {
    async fn backend_actor<T>(&self, req: &Request<T>) -> Result<Option<Identity>, Status> {
        let actor = self.security_manager.identify(
            req.extensions().get::<TlsConnectInfo<TcpConnectInfo>>(),
            None,
        )?;
        Self::require_node_actor(self.security.is_some(), &actor)?;
        Ok(actor)
    }

    fn require_node_actor(secure: bool, actor: &Option<Identity>) -> Result<(), Status> {
        if secure && !matches!(actor, Some(Identity::SystemNode(_))) {
            return Err(Status::permission_denied("system node identity required"));
        }
        Ok(())
    }

    async fn authorize(
        &self,
        actor: &Option<Identity>,
        executor_id: &str,
        operation: &str,
    ) -> Result<Executor, Status> {
        let executor = self
            .controller
            .get_executor(executor_id.to_owned())
            .map_err(|_| Status::not_found("executor not found"))?;
        self.security_manager
            .authorize(actor, operation, Object::Node(&executor.node))
            .await?;
        Ok(executor)
    }
}

// ============================================================================
// Helper functions for watch_node stream handling
// ============================================================================

/// Sends an acknowledgement response to the client.
async fn send_ack(tx: &mpsc::Sender<Result<WatchNodeResponse, Status>>) -> bool {
    let ack = WatchNodeResponse {
        response: Some(rpc::watch_node_response::Response::Ack(
            rpc::Acknowledgement {
                timestamp: Utc::now().timestamp(),
            },
        )),
    };
    tx.send(Ok(ack)).await.is_ok()
}

/// Handles a heartbeat request from the client.
/// Updates node status and sends acknowledgement.
async fn handle_heartbeat(
    controller: &ControllerPtr,
    tx: &mpsc::Sender<Result<WatchNodeResponse, Status>>,
    node_name: &str,
    hb: rpc::NodeHeartbeat,
) -> bool {
    tracing::debug!("Received heartbeat from node <{}>", hb.node_name);

    // Update node status if provided
    if let Some(status) = hb.status {
        let node = build_node_from_heartbeat(controller, node_name, status);
        if let Err(e) = controller.update_node(&node).await {
            tracing::warn!("Failed to update node status for <{}>: {}", node_name, e);
        }
    }

    // Send acknowledgement
    send_ack(tx).await
}

/// Builds a Node struct from heartbeat status, preserving existing node info.
fn build_node_from_heartbeat(
    controller: &ControllerPtr,
    node_name: &str,
    status: rpc::NodeStatus,
) -> Node {
    // Fetch existing node to preserve info (labels, taints, etc.)
    let existing_node = controller.get_node(node_name);

    match existing_node {
        Ok(Some(existing)) => {
            // Preserve existing node info, update status fields
            Node {
                name: node_name.to_string(),
                state: rpc::NodeState::try_from(status.state)
                    .unwrap_or(rpc::NodeState::Unknown)
                    .into(),
                capacity: status
                    .capacity
                    .map(|r| r.into())
                    .unwrap_or(existing.capacity),
                allocatable: status
                    .allocatable
                    .map(|r| r.into())
                    .unwrap_or(existing.allocatable),
                info: status.info.map(|i| i.into()).unwrap_or(existing.info),
            }
        }
        _ => {
            // Node not found or error, create with status data
            Node {
                name: node_name.to_string(),
                state: rpc::NodeState::try_from(status.state)
                    .unwrap_or(rpc::NodeState::Unknown)
                    .into(),
                capacity: status.capacity.map(|r| r.into()).unwrap_or_default(),
                allocatable: status.allocatable.map(|r| r.into()).unwrap_or_default(),
                info: status.info.map(|i| i.into()).unwrap_or_default(),
            }
        }
    }
}

// ============================================================================
// Backend trait implementation
// ============================================================================

#[async_trait]
impl Backend for Flame {
    async fn register_node(
        &self,
        req: Request<RegisterNodeRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::register_node");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        let node = Node::from(
            req.node
                .ok_or(FlameError::InvalidConfig("node is required".to_string()))?,
        );
        self.security_manager
            .authorize(&identity, operation::UPDATE, Object::Node(&node.name))
            .await?;

        // Convert reported executors from proto
        let reported_executors: Vec<Executor> =
            req.executors.into_iter().map(Executor::from).collect();

        tracing::info!(
            "Registering node <{}> with {} executors",
            node.name,
            reported_executors.len()
        );

        // Delegate all registration logic to controller
        self.controller
            .register_node(&node, &reported_executors)
            .await?;

        Ok(Response::new(rpc::Result::default()))
    }

    /// Deprecated: Use `watch_node` streaming RPC instead for better efficiency.
    /// This polling-based API is kept for backward compatibility only.
    #[allow(deprecated)]
    async fn sync_node(
        &self,
        req: Request<SyncNodeRequest>,
    ) -> Result<Response<SyncNodeResponse>, Status> {
        trace_fn!("Backend::sync_node");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        let node = Node::from(
            req.node
                .ok_or(FlameError::InvalidConfig("node is required".to_string()))?,
        );
        self.security_manager
            .authorize(&identity, operation::UPDATE, Object::Node(&node.name))
            .await?;
        let executors: Vec<Executor> = req.executors.into_iter().map(rpc::Executor::into).collect();

        let executors = self.controller.sync_node(&node, &executors).await?;

        Ok(Response::new(SyncNodeResponse {
            node: Some(node.into()),
            executors: executors.into_iter().map(rpc::Executor::from).collect(),
        }))
    }

    type WatchNodeStream = ReceiverStream<Result<WatchNodeResponse, Status>>;

    /// WatchNode streaming RPC for node-executor synchronization.
    ///
    /// Flow:
    /// 1. Client must call RegisterNode first to register the node and push initial executors to queue
    /// 2. Client starts WatchNode stream, sends first heartbeat to identify the node
    /// 3. Server gets the queue (already populated by RegisterNode) and starts streaming
    /// 4. Server sends executor updates from the queue as they arrive
    /// 5. Client sends periodic heartbeats with node status
    async fn watch_node(
        &self,
        req: Request<Streaming<WatchNodeRequest>>,
    ) -> Result<Response<Self::WatchNodeStream>, Status> {
        trace_fn!("Backend::watch_node");

        let identity = self.backend_actor(&req).await?;

        let mut in_stream = req.into_inner();
        let (tx, rx) = mpsc::channel(32);

        let controller = self.controller.clone();
        let security_manager = self.security_manager.clone();

        // Clone tx for the queue consumer task
        let tx_for_queue = tx.clone();

        // Spawn a task to handle the incoming stream
        tokio::spawn(async move {
            let mut node_name: Option<String> = None;

            loop {
                let request = match timeout(
                    std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS),
                    in_stream.message(),
                )
                .await
                {
                    Ok(Ok(Some(req))) => req,
                    Ok(Ok(None)) => break, // Stream closed
                    Ok(Err(e)) => {
                        tracing::error!("Stream error: {}", e);
                        break;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Heartbeat timeout for node <{:?}>. Closing stream.",
                            node_name
                        );
                        break;
                    }
                };

                // WatchNodeRequest now only contains heartbeat (no registration)
                let Some(hb) = request.heartbeat else {
                    tracing::warn!("Received empty heartbeat request");
                    continue;
                };

                if node_name
                    .as_deref()
                    .is_some_and(|name| name != hb.node_name)
                {
                    let _ = tx
                        .send(Err(Status::permission_denied(
                            "node changed on watch stream",
                        )))
                        .await;
                    break;
                }

                // First heartbeat: identify the node and set up receiver
                if node_name.is_none() {
                    let name = hb.node_name.clone();
                    if let Err(status) = security_manager
                        .authorize(&identity, operation::UPDATE, Object::Node(&name))
                        .await
                    {
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                    tracing::info!("Node <{}> starting watch stream", name);

                    // Get the channel (already created by register_node)
                    match controller.get_node_channel(&name) {
                        Ok(Some((_sender, receiver))) => {
                            node_name = Some(name.clone());

                            // Spawn a task to receive from connection and send to client
                            let tx_clone = tx_for_queue.clone();
                            let name_clone = name.clone();
                            tokio::spawn(async move {
                                while let Some(executor) = receiver.recv().await {
                                    let response = WatchNodeResponse {
                                        response: Some(
                                            rpc::watch_node_response::Response::Executor(
                                                rpc::Executor::from(&executor),
                                            ),
                                        ),
                                    };
                                    if tx_clone.send(Ok(response)).await.is_err() {
                                        tracing::debug!(
                                            "Client <{}> disconnected, stopping receiver",
                                            name_clone
                                        );
                                        break;
                                    }
                                }
                            });
                        }
                        Ok(None) => {
                            tracing::error!(
                                "No connection for node <{}>. Call RegisterNode first.",
                                name
                            );
                            break;
                        }
                        Err(e) => {
                            tracing::error!("Failed to get channel for node <{}>: {}", name, e);
                            break;
                        }
                    }
                }

                // Handle heartbeat - update node status
                if let Some(ref name) = node_name {
                    if !handle_heartbeat(&controller, &tx, name, hb).await {
                        tracing::warn!("Client disconnected during heartbeat");
                        break;
                    }
                }
            }

            // Cleanup: drain node (state transition + start cleanup timer)
            if let Some(name) = node_name {
                tracing::info!("Node <{}> stream closed, draining", name);

                if let Err(e) = controller.drain_node(&name).await {
                    tracing::warn!("Failed to drain node <{}>: {}", name, e);
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn release_node(
        &self,
        req: Request<ReleaseNodeRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::release_node");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.security_manager
            .authorize(&identity, operation::DELETE, Object::Node(&req.node_name))
            .await?;
        self.controller.release_node(&req.node_name).await?;
        Ok(Response::new(rpc::Result::default()))
    }

    async fn register_executor(
        &self,
        req: Request<RegisterExecutorRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::register_executor");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        let spec = req
            .executor_spec
            .ok_or(FlameError::InvalidConfig("no executor spec".to_string()))?;
        let stored = self
            .authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;

        let shim = Shim::from(spec.shim());
        let now = Utc::now();
        let e = Executor {
            id: req.executor_id,
            node: stored.node,
            resreq: spec.resreq.unwrap_or_default().into(),
            shim,
            application: stored.application,
            task_id: None,
            ssn_id: None,
            attributes: Default::default(),
            creation_time: now,
            latest_updated_timestamp: now,
            state: ExecutorState::Idle,
        };

        self.controller
            .register_executor(&e)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn unregister_executor(
        &self,
        req: Request<UnregisterExecutorRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::unregister_executor");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::DELETE)
            .await?;

        self.controller.unregister_executor(req.executor_id).await?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn bind_executor(
        &self,
        req: Request<BindExecutorRequest>,
    ) -> Result<Response<BindExecutorResponse>, Status> {
        trace_fn!("Backend::bind_executor");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;
        let executor_id = req.executor_id.to_string();

        let ssn = self
            .controller
            .wait_for_session(executor_id.clone())
            .await?;

        // If the session is not found, return.
        let Some(ssn) = ssn else {
            return Ok(Response::new(BindExecutorResponse {
                application: None,
                session: None,
            }));
        };

        let app = self
            .controller
            .get_application(ssn.application.clone())
            .await?;
        let application = Some(rpc::Application::from(&app));
        let session = Some(rpc::Session::from(&ssn));

        tracing::debug!(
            "Bind executor <{}> to Session <{}:{}>",
            executor_id,
            app.name,
            ssn.id,
        );

        Ok(Response::new(BindExecutorResponse {
            application,
            session,
        }))
    }

    async fn bind_executor_completed(
        &self,
        req: Request<BindExecutorCompletedRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::bind_executor_completed");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;

        self.controller
            .bind_executor_completed(
                req.executor_id,
                req.result.map(FlameResult::from),
                req.attributes,
            )
            .await?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn unbind_executor(
        &self,
        req: Request<UnbindExecutorRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::unbind_executor");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;
        self.controller.unbind_executor(req.executor_id).await?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn unbind_executor_completed(
        &self,
        req: Request<UnbindExecutorCompletedRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::unbind_executor_completed");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;
        self.controller
            .unbind_executor_completed(req.executor_id)
            .await?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn launch_task(
        &self,
        req: Request<LaunchTaskRequest>,
    ) -> Result<Response<LaunchTaskResponse>, Status> {
        trace_fn!("Backend::launch_task");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;
        let executor_id = req.executor_id.clone();

        let task = self.controller.launch_task(executor_id).await?;
        if let Some(task) = task {
            return Ok(Response::new(LaunchTaskResponse {
                task: Some(rpc::Task::from(&task)),
            }));
        }

        Ok(Response::new(LaunchTaskResponse { task: None }))
    }

    async fn complete_task(
        &self,
        req: Request<CompleteTaskRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::complete_task");
        let identity = self.backend_actor(&req).await?;
        let req = req.into_inner();
        self.authorize(&identity, &req.executor_id, operation::UPDATE)
            .await?;

        let task_result = req.task_result.ok_or(FlameError::InvalidState(format!(
            "no task result when completing task in {}",
            req.executor_id.clone()
        )))?;

        self.controller
            .complete_task(
                req.executor_id.clone(),
                TaskResult::from(task_result),
                req.attributes,
            )
            .await?;

        Ok(Response::new(rpc::Result::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn secure_backend_rejects_tenant_and_cache_identities() {
        for actor in [
            Some(Identity::TenantUser("root".to_string())),
            Some(Identity::SystemCache("cache-a".to_string())),
            None,
        ] {
            assert_eq!(
                Flame::require_node_actor(true, &actor).unwrap_err().code(),
                tonic::Code::PermissionDenied,
            );
            assert!(Flame::require_node_actor(false, &actor).is_ok());
        }
        assert!(
            Flame::require_node_actor(true, &Some(Identity::SystemNode("node-a".to_string())))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn system_node_controls_only_its_stored_executors() {
        let config = common::ctx::FlameClusterContext {
            cluster: common::ctx::FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = common::storage::new_ptr(&config).await.unwrap();
        let controller = crate::controller::new_ptr(storage.clone());
        let app_name = "test-app".to_string();
        controller
            .register_application(
                app_name.clone(),
                common::apis::ApplicationAttributes::default(),
            )
            .await
            .unwrap();
        let session_id = "test-session".to_string();
        controller
            .create_session(common::apis::SessionAttributes {
                id: session_id.clone(),
                application: app_name,
                resreq: Some(common::apis::ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let executor = storage
            .create_executor("node-a".to_string(), session_id)
            .await
            .unwrap();
        let flame = Flame {
            controller: controller.clone(),
            cluster_default_resreq: None,
            security: Some(Default::default()),
            security_manager: Arc::from(
                common::security::new(Some(&common::ctx::FlameSecurity {
                    trust_domain: "test.local".to_string(),
                    ..Default::default()
                }))
                .with_role(storage.clone())
                .await
                .unwrap(),
            ),
        };
        let owner = Some(Identity::SystemNode("node-a".to_string()));
        let other_node = Some(Identity::SystemNode("node-b".to_string()));
        assert!(flame
            .authorize(&owner, &executor.id, operation::UPDATE)
            .await
            .is_ok());
        assert_eq!(
            flame
                .authorize(&other_node, &executor.id, operation::UPDATE)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::NotFound
        );
        assert!(flame
            .authorize(&owner, &executor.id, operation::DELETE)
            .await
            .is_ok());
    }
}

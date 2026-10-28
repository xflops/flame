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
use crate::model::{Executor, NodeConnectionReceiver, NodeConnectionSender};
use common::apis::{ExecutorState, FlameResult, Node, Shim, TaskResult};
use common::FlameError;

/// Timeout for heartbeat in seconds. If no heartbeat is received within this
/// duration, the stream is considered stale and will be closed.
const HEARTBEAT_TIMEOUT_SECS: u64 = 15;

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
        let existing = controller.get_node(node_name).ok().flatten();
        let node = build_node_from_heartbeat(existing, node_name, status);
        if let Err(e) = controller.update_node(&node).await {
            tracing::warn!("Failed to update node status for <{}>: {}", node_name, e);
        }
    }

    // Send acknowledgement
    send_ack(tx).await
}

/// Builds a Node struct from heartbeat status, preserving existing node info.
fn build_node_from_heartbeat(
    existing_node: Option<Node>,
    node_name: &str,
    status: rpc::NodeStatus,
) -> Node {
    // The existing node preserves what a heartbeat does not carry (info, shims, ...).
    match existing_node {
        Some(existing) => {
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
                // Reported at registration, not in a heartbeat.
                shims: existing.shims,
            }
        }
        None => {
            // Node not found, create with status data
            Node {
                name: node_name.to_string(),
                state: rpc::NodeState::try_from(status.state)
                    .unwrap_or(rpc::NodeState::Unknown)
                    .into(),
                capacity: status.capacity.map(|r| r.into()).unwrap_or_default(),
                allocatable: status.allocatable.map(|r| r.into()).unwrap_or_default(),
                info: status.info.map(|i| i.into()).unwrap_or_default(),
                shims: Vec::new(),
            }
        }
    }
}

// ============================================================================
// Backend trait implementation
// ============================================================================

/// Forwards the executor notifications queued for `node` to one watch stream.
///
/// The queue belongs to the node's connection and outlives the stream: when the executor
/// manager reconnects, the next stream reads the same queue. The consumer of a dead stream
/// must therefore stop as soon as its stream closes. If it stayed parked in `recv`, it would
/// keep the queue's lock, take the next notification and fail to deliver it, and the executor
/// that notification announces would stay `Void` forever.
async fn forward_executors(
    node: String,
    sender: NodeConnectionSender,
    receiver: NodeConnectionReceiver,
    tx: mpsc::Sender<Result<WatchNodeResponse, Status>>,
) {
    loop {
        // `biased`: a closed stream wins over a ready notification, which then stays queued
        // for the next stream. Dropping the pending `recv` releases the queue's lock.
        let executor = tokio::select! {
            biased;
            _ = tx.closed() => {
                tracing::info!("Watch stream of node <{node}> closed, stopping its executor forwarder");
                return;
            }
            executor = receiver.recv() => match executor {
                Some(executor) => executor,
                None => return,
            },
        };

        let response = WatchNodeResponse {
            response: Some(rpc::watch_node_response::Response::Executor(
                rpc::Executor::from(&executor),
            )),
        };
        if tx.send(Ok(response)).await.is_err() {
            // The stream died after the executor was taken from the queue: queue it again
            // for the next stream of the node instead of dropping it.
            tracing::warn!(
                "Watch stream of node <{node}> closed while sending executor <{}>, queueing it again",
                executor.id
            );
            if let Err(e) = sender.send(executor).await {
                tracing::warn!("Failed to queue the executor of node <{node}> again: {e}");
            }
            return;
        }
    }
}

#[async_trait]
impl Backend for Flame {
    async fn register_node(
        &self,
        req: Request<RegisterNodeRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::register_node");
        let req = req.into_inner();
        let node = Node::from(
            req.node
                .ok_or(FlameError::InvalidConfig("node is required".to_string()))?,
        );

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
        let req = req.into_inner();
        let node = Node::from(
            req.node
                .ok_or(FlameError::InvalidConfig("node is required".to_string()))?,
        );
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

        let mut in_stream = req.into_inner();
        let (tx, rx) = mpsc::channel(32);

        let controller = self.controller.clone();

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

                let name = hb.node_name.clone();

                // First heartbeat: identify the node and set up receiver
                if node_name.is_none() {
                    tracing::info!("Node <{}> starting watch stream", name);

                    // Get the channel (already created by register_node)
                    match controller.get_node_channel(&name) {
                        Ok(Some((sender, receiver))) => {
                            node_name = Some(name.clone());

                            // Spawn a task to receive from connection and send to client
                            tokio::spawn(forward_executors(
                                name.clone(),
                                sender,
                                receiver,
                                tx_for_queue.clone(),
                            ));
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
        let req = req.into_inner();
        self.controller.release_node(&req.node_name).await?;
        Ok(Response::new(rpc::Result::default()))
    }

    async fn register_executor(
        &self,
        req: Request<RegisterExecutorRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::register_executor");
        let req = req.into_inner();
        let spec = req
            .executor_spec
            .ok_or(FlameError::InvalidConfig("no executor spec".to_string()))?;

        let shim = Shim::from(spec.shim());
        let now = Utc::now();
        let e = Executor {
            id: req.executor_id,
            node: spec.node,
            resreq: spec.resreq.unwrap_or_default().into(),
            shim,
            application: spec.application,
            task_id: None,
            signal: None,
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
        let req = req.into_inner();

        self.controller.unregister_executor(req.executor_id).await?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn bind_executor(
        &self,
        req: Request<BindExecutorRequest>,
    ) -> Result<Response<BindExecutorResponse>, Status> {
        trace_fn!("Backend::bind_executor");
        let req = req.into_inner();
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
        let mut session = rpc::Session::from(&ssn);
        if let Some(spec) = session.spec.as_mut() {
            spec.tokens = ssn.tokens.clone();
        }
        let session = Some(session);

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
        let req = req.into_inner();

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
        let req = req.into_inner();
        self.controller.unbind_executor(req.executor_id).await?;

        Ok(Response::new(rpc::Result::default()))
    }

    async fn unbind_executor_completed(
        &self,
        req: Request<UnbindExecutorCompletedRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Backend::unbind_executor_completed");
        let req = req.into_inner();
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
        let req = req.into_inner();
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
        let req = req.into_inner();

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
    use std::time::Duration;

    use common::apis::ResourceRequirement;
    use tokio::task::JoinHandle;
    use tokio::time::sleep;

    use super::*;
    use crate::model::NodeConnection;

    type Stream = (
        mpsc::Sender<Result<WatchNodeResponse, Status>>,
        mpsc::Receiver<Result<WatchNodeResponse, Status>>,
    );

    fn heartbeat_status() -> rpc::NodeStatus {
        rpc::NodeStatus {
            state: rpc::NodeState::Ready as i32,
            capacity: Some(
                ResourceRequirement {
                    cpu: 8,
                    memory: 1024,
                    gpu: 1,
                }
                .into(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn heartbeat_keeps_the_shims_the_node_registered_with() {
        let existing = Node {
            name: "node-1".to_string(),
            shims: vec![Shim::Cri, Shim::Host],
            ..Default::default()
        };

        let node = build_node_from_heartbeat(Some(existing), "node-1", heartbeat_status());

        assert_eq!(node.shims, [Shim::Cri, Shim::Host]);
        // The heartbeat updates what it carries.
        assert_eq!(node.capacity.cpu, 8);
        assert_eq!(node.capacity.gpu, 1);
    }

    #[test]
    fn heartbeat_of_an_unknown_node_has_no_shims() {
        let node = build_node_from_heartbeat(None, "node-1", heartbeat_status());
        assert!(node.shims.is_empty());
    }

    fn executor(id: &str) -> Executor {
        Executor {
            id: id.to_string(),
            node: "node-1".to_string(),
            resreq: ResourceRequirement::default(),
            shim: Shim::Host,
            application: String::new(),
            task_id: None,
            ssn_id: None,
            attributes: Default::default(),
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::Void,
            signal: None,
        }
    }

    fn forwarder(
        connection: &NodeConnection,
        tx: &mpsc::Sender<Result<WatchNodeResponse, Status>>,
    ) -> JoinHandle<()> {
        tokio::spawn(forward_executors(
            "node-1".to_string(),
            connection.sender(),
            connection.receiver(),
            tx.clone(),
        ))
    }

    async fn next_executor(rx: &mut mpsc::Receiver<Result<WatchNodeResponse, Status>>) -> String {
        let response = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the notification reaches the new stream")
            .expect("the stream is open")
            .expect("a response, not an error");
        match response.response {
            Some(rpc::watch_node_response::Response::Executor(executor)) => {
                executor.metadata.expect("metadata").id
            }
            other => panic!("expected an executor, got {other:?}"),
        }
    }

    /// Issue #615: the consumer of the old stream must not take, and lose, the first
    /// notification sent after the executor manager reconnects.
    #[tokio::test]
    async fn notification_after_a_reconnect_reaches_the_new_stream() {
        let connection = NodeConnection::new("node-1".to_string());

        // The first stream: its consumer parks waiting for a notification.
        let (old_tx, old_rx): Stream = mpsc::channel(32);
        let old = forwarder(&connection, &old_tx);
        sleep(Duration::from_millis(50)).await;
        // The executor manager goes away and comes back with a second stream.
        drop(old_rx);
        let (new_tx, mut new_rx): Stream = mpsc::channel(32);
        let _new = forwarder(&connection, &new_tx);

        connection.sender().send(executor("e1")).await.unwrap();

        assert_eq!(next_executor(&mut new_rx).await, "e1");
        timeout(Duration::from_secs(2), old)
            .await
            .expect("the consumer of the closed stream stops")
            .unwrap();
    }

    #[tokio::test]
    async fn closed_stream_leaves_queued_notifications_for_the_next_one() {
        let connection = NodeConnection::new("node-1".to_string());
        connection.sender().send(executor("e1")).await.unwrap();

        // A consumer that starts on an already closed stream must not take the notification.
        let (dead_tx, dead_rx): Stream = mpsc::channel(32);
        drop(dead_rx);
        timeout(Duration::from_secs(2), forwarder(&connection, &dead_tx))
            .await
            .expect("the consumer of the closed stream stops")
            .unwrap();

        let (tx, mut rx): Stream = mpsc::channel(32);
        let _live = forwarder(&connection, &tx);
        assert_eq!(next_executor(&mut rx).await, "e1");
    }

    #[tokio::test]
    async fn executor_taken_just_before_the_stream_dies_is_queued_again() {
        let connection = NodeConnection::new("node-1".to_string());

        // A stream whose buffer is full: the forwarder takes the executor, then waits to send it.
        let (tx, rx): Stream = mpsc::channel(1);
        tx.send(Ok(WatchNodeResponse::default())).await.unwrap();
        let _old = forwarder(&connection, &tx);
        connection.sender().send(executor("e1")).await.unwrap();
        sleep(Duration::from_millis(100)).await;
        drop(rx);

        let (new_tx, mut new_rx): Stream = mpsc::channel(32);
        let _new = forwarder(&connection, &new_tx);
        assert_eq!(next_executor(&mut new_rx).await, "e1");
    }
}

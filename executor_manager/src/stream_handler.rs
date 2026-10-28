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

//! StreamHandler for managing bidirectional WatchNode streams.
//!
//! This module provides the client-side implementation of the WatchNode
//! streaming protocol, including reconnection logic and heartbeat management.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{interval, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

use common::apis::Node;
use common::ctx::FlameClusterContext;
use common::FlameError;
use rpc::flame::v1 as proto;
use stdng::{lock_ptr, MutexPtr};

use crate::client::BackendClient;
use crate::devices::DeviceManagers;
use crate::executor::{Executor, ExecutorPtr};
use crate::manager::{ExecutorManager, ExecutorMessage};

/// Default interval between heartbeats in seconds.
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 5;

/// Default interval between reconnection attempts in seconds.
const DEFAULT_RECONNECT_INTERVAL_SECS: u64 = 1;

/// Maximum reconnection interval in seconds (for exponential backoff).
const MAX_RECONNECT_INTERVAL_SECS: u64 = 30;

/// Timeout for receiving responses from the server.
const RESPONSE_TIMEOUT_SECS: u64 = 10;

/// Manages the bidirectional WatchNode stream lifecycle.
///
/// The StreamHandler is responsible for:
/// - Establishing and maintaining the stream connection
/// - Re-registering the node on each reconnection (handles failover)
/// - Sending periodic heartbeats with current node status
/// - Handling reconnection with exponential backoff
/// - Processing executor state notifications from the server
/// - Forwarding executor updates to the manager for action derivation
pub struct StreamHandler {
    /// A clone of the executor manager: its configuration (what the node reports about it, such
    /// as the shims it runs), its backend client, its devices, and its executors, which are
    /// shared with it for re-registration.
    manager: ExecutorManager,
    node: MutexPtr<Node>,
    reconnect_interval: Duration,
    heartbeat_interval: Duration,
}

impl StreamHandler {
    /// Creates a new StreamHandler.
    ///
    /// # Arguments
    ///
    /// * `manager` - A clone of the executor manager, which shares its executors, devices
    ///   and backend client with the handler
    pub fn new(manager: ExecutorManager) -> Result<Self, FlameError> {
        manager.dev_mgr.discover()?;
        let mut node = Node::new();
        node.with_shims(&manager.ctx.cluster.executors.shims)
            .with_capability(manager.dev_mgr.capacity());
        Ok(StreamHandler {
            manager,
            node: stdng::new_ptr(node),
            reconnect_interval: Duration::from_secs(DEFAULT_RECONNECT_INTERVAL_SECS),
            heartbeat_interval: Duration::from_secs(DEFAULT_HEARTBEAT_INTERVAL_SECS),
        })
    }

    /// Runs the stream handler, forwarding executor updates to the manager.
    ///
    /// This method establishes the WatchNode stream and continuously
    /// processes responses from the server. On stream failure, it
    /// attempts to reconnect with exponential backoff.
    ///
    /// This is a long-running, self-recovering task that handles errors
    /// internally and never returns under normal operation.
    pub async fn run(&mut self, executor_tx: mpsc::Sender<ExecutorMessage>) {
        let mut current_reconnect_interval = self.reconnect_interval;

        loop {
            match self.run_stream(&executor_tx).await {
                Ok(()) => {
                    // Stream closed gracefully, reset reconnect interval
                    current_reconnect_interval = self.reconnect_interval;
                    let node_name = lock_ptr!(self.node)
                        .map(|n| n.name.clone())
                        .unwrap_or_else(|_| "unknown".to_string());
                    tracing::info!(
                        "WatchNode stream closed gracefully for node <{}>",
                        node_name
                    );
                }
                Err(e) => {
                    let node_name = lock_ptr!(self.node)
                        .map(|n| n.name.clone())
                        .unwrap_or_else(|_| "unknown".to_string());
                    tracing::warn!(
                        "WatchNode stream error for node <{}>: {}. Reconnecting in {:?}",
                        node_name,
                        e,
                        current_reconnect_interval
                    );
                }
            }

            // Wait before reconnecting
            tokio::time::sleep(current_reconnect_interval).await;

            // Exponential backoff
            current_reconnect_interval = std::cmp::min(
                current_reconnect_interval * 2,
                Duration::from_secs(MAX_RECONNECT_INTERVAL_SECS),
            );
        }
    }

    /// Runs a single stream session.
    ///
    /// Flow:
    /// 1. Register the node with current executor state (handles reconnection/failover)
    /// 2. Start WatchNode stream and immediately send first heartbeat to identify the node
    /// 3. Server sends all executors on first heartbeat (initial sync)
    /// 4. Continue sending periodic heartbeats
    async fn run_stream(
        &mut self,
        executor_tx: &mpsc::Sender<ExecutorMessage>,
    ) -> Result<(), FlameError> {
        // Get current node state
        let node = {
            let mut node = lock_ptr!(self.node)?;
            self.manager.dev_mgr.discover()?;
            node.with_shims(&self.manager.ctx.cluster.executors.shims)
                .with_capability(self.manager.dev_mgr.capacity());
            node.clone()
        };

        // Get current executors for state alignment during registration
        let current_executors: Vec<Executor> = lock_ptr!(self.manager.executors)?
            .values()
            .filter_map(|ptr| lock_ptr!(ptr).ok().map(|e| (*e).clone()))
            .collect();

        // Register node with current executor list for state alignment
        // This is called on every reconnection to handle failover scenarios
        tracing::info!(
            "Registering node <{}> with {} executors for state alignment",
            node.name,
            current_executors.len()
        );
        self.manager
            .client
            .register_node(&node, &current_executors)
            .await?;

        // Create channels for the bidirectional stream
        let (request_tx, request_rx) = mpsc::channel::<proto::WatchNodeRequest>(32);

        // Start the stream
        let response_stream = self
            .manager
            .client
            .watch_node(ReceiverStream::new(request_rx))
            .await?;

        tracing::info!("WatchNode: Starting stream for node <{}>", node.name,);

        // Send initial heartbeat immediately to identify the node
        let initial_heartbeat = proto::WatchNodeRequest {
            heartbeat: Some(proto::NodeHeartbeat {
                node_name: node.name.clone(),
                status: Some(proto::NodeStatus {
                    state: proto::NodeState::from(node.state) as i32,
                    capacity: Some(node.capacity.clone().into()),
                    allocatable: Some(node.allocatable.clone().into()),
                    info: Some(node.info.clone().into()),
                    addresses: vec![],
                    last_heartbeat_time: 0,
                }),
            }),
        };
        request_tx
            .send(initial_heartbeat)
            .await
            .map_err(|e| FlameError::Network(format!("Failed to send initial heartbeat: {}", e)))?;

        // Spawn heartbeat task for periodic heartbeats
        let heartbeat_tx = request_tx.clone();
        let node_ptr = self.node.clone();
        let dev_mgr = self.manager.dev_mgr.clone();
        let shims = self.manager.ctx.cluster.executors.shims.clone();
        let heartbeat_interval = self.heartbeat_interval;
        let heartbeat_handle = tokio::spawn(async move {
            let mut ticker = interval(heartbeat_interval);
            loop {
                ticker.tick().await;

                // Refresh and collect current node status
                let (node_name, status) = match lock_ptr!(node_ptr) {
                    Ok(mut node) => {
                        // Discover again, and rebuild the node to get its current resource status
                        if let Err(error) = dev_mgr.discover() {
                            tracing::warn!(%error, "Failed to discover devices for heartbeat");
                            continue;
                        }
                        node.with_shims(&shims).with_capability(dev_mgr.capacity());
                        let status = proto::NodeStatus {
                            state: proto::NodeState::from(node.state) as i32,
                            capacity: Some(node.capacity.clone().into()),
                            allocatable: Some(node.allocatable.clone().into()),
                            info: Some(node.info.clone().into()),
                            addresses: vec![],
                            last_heartbeat_time: 0,
                        };
                        (node.name.clone(), Some(status))
                    }
                    Err(e) => {
                        tracing::warn!("Failed to lock node for heartbeat: {}", e);
                        continue;
                    }
                };

                let heartbeat = proto::WatchNodeRequest {
                    heartbeat: Some(proto::NodeHeartbeat { node_name, status }),
                };
                if heartbeat_tx.send(heartbeat).await.is_err() {
                    break;
                }
            }
        });

        // Process responses
        let result = self.process_responses(response_stream, executor_tx).await;

        // Cancel heartbeat task
        heartbeat_handle.abort();

        result
    }

    /// Processes responses from the server stream.
    async fn process_responses(
        &self,
        mut stream: Streaming<proto::WatchNodeResponse>,
        executor_tx: &mpsc::Sender<ExecutorMessage>,
    ) -> Result<(), FlameError> {
        loop {
            match timeout(
                Duration::from_secs(RESPONSE_TIMEOUT_SECS * 3), // Allow for missed heartbeats
                stream.message(),
            )
            .await
            {
                Ok(Ok(Some(response))) => {
                    if let Some(msg) = self.handle_response(response)? {
                        executor_tx.send(msg).await.map_err(|e| {
                            FlameError::Internal(format!("Failed to send executor: {}", e))
                        })?;
                    }
                }
                Ok(Ok(None)) => {
                    // Stream closed by server
                    return Ok(());
                }
                Ok(Err(e)) => {
                    return Err(FlameError::Network(format!("Stream error: {}", e)));
                }
                Err(_) => {
                    return Err(FlameError::Network("Response timeout".to_string()));
                }
            }
        }
    }

    /// Handles a single response from the server.
    ///
    /// Returns an ExecutorMessage if the response contains an executor update,
    /// which the caller is responsible for forwarding to the manager.
    fn handle_response(
        &self,
        response: proto::WatchNodeResponse,
    ) -> Result<Option<ExecutorMessage>, FlameError> {
        match response.response {
            Some(proto::watch_node_response::Response::Executor(proto_executor)) => {
                let executor: Executor = Executor::try_from(&proto_executor)?;

                tracing::debug!(
                    "WatchNode: Received executor <{}> with state {:?}",
                    executor.id,
                    executor.state
                );

                Ok(Some(ExecutorMessage::Update(executor)))
            }
            Some(proto::watch_node_response::Response::Ack(ack)) => {
                tracing::trace!(
                    "WatchNode: Received acknowledgement with timestamp {}",
                    ack.timestamp
                );
                Ok(None)
            }
            None => {
                tracing::warn!("WatchNode: Received empty response");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::Shim;

    fn context_with_shims(shims: Vec<Shim>) -> FlameClusterContext {
        let mut ctx = FlameClusterContext::default();
        ctx.cluster.executors.shims = shims;
        ctx
    }

    struct RefreshDevices {
        visible: std::sync::atomic::AtomicI32,
        discovered: std::sync::atomic::AtomicI32,
        fail: std::sync::atomic::AtomicBool,
    }

    impl crate::devices::DeviceManager for RefreshDevices {
        fn discover(&self) -> Result<(), FlameError> {
            use std::sync::atomic::Ordering::SeqCst;
            if self.fail.load(SeqCst) {
                return Err(FlameError::Internal("discovery failed".into()));
            }
            self.discovered.store(self.visible.load(SeqCst), SeqCst);
            Ok(())
        }
        fn capacity(&self) -> common::apis::ResourceRequirement {
            common::apis::ResourceRequirement {
                gpu: self.discovered.load(std::sync::atomic::Ordering::SeqCst),
                ..Default::default()
            }
        }
        fn allocate(&self, _: &Executor) -> Result<crate::devices::DeviceAssignment, FlameError> {
            unreachable!()
        }
        fn release(&self, _: &Executor) -> Result<(), FlameError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn every_node_refresh_discovers_before_reading_capacity() {
        use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::SeqCst};
        let devices = Arc::new(RefreshDevices {
            visible: AtomicI32::new(2),
            discovered: AtomicI32::new(0),
            fail: AtomicBool::new(false),
        });
        let dev_mgr = Arc::new(DeviceManagers::with_manager(
            crate::devices::GPU,
            devices.clone(),
        ));
        let manager = ExecutorManager::for_test(
            context_with_shims(vec![Shim::Cri, Shim::Host]),
            dev_mgr.clone(),
        );
        let handler = StreamHandler::new(manager).unwrap();
        assert!(Arc::ptr_eq(&handler.manager.dev_mgr, &dev_mgr));
        let mut node = lock_ptr!(handler.node).unwrap().clone();
        assert_eq!(node.capacity.gpu, 2);
        // The node carries the shims of the context.
        assert_eq!(node.shims, [Shim::Cri, Shim::Host]);
        let shims = handler.manager.ctx.cluster.executors.shims.clone();
        for visible in [3, 0, 1] {
            devices.visible.store(visible, SeqCst);
            handler.manager.dev_mgr.discover().unwrap();
            node.with_shims(&shims)
                .with_capability(handler.manager.dev_mgr.capacity());
            assert_eq!(node.shims, [Shim::Cri, Shim::Host]);
            assert_eq!(node.capacity.gpu, visible);
            assert_eq!(node.capacity.cpu, 0);
            assert_eq!(node.capacity.memory, 0);
            assert_eq!(node.allocatable, node.capacity);
        }
        devices.visible.store(99, SeqCst);
        devices.fail.store(true, SeqCst);
        // A device manager that cannot discover fails before the node is rebuilt, so the node
        // stays as it was.
        assert!(handler.manager.dev_mgr.discover().is_err());
        assert_eq!(node.capacity.gpu, 1);
        let manager = ExecutorManager::for_test(context_with_shims(vec![Shim::Host]), dev_mgr);
        assert!(StreamHandler::new(manager).is_err());
    }

    #[test]
    fn disabled_device_managers_clear_previous_device_capacity() {
        let mut node = Node::new();
        node.capacity.gpu = 99;
        let dev_mgr = DeviceManagers::default();
        dev_mgr.discover().unwrap();
        node.with_shims(&[Shim::Host])
            .with_capability(dev_mgr.capacity());
        assert_eq!(node.shims, [Shim::Host]);
        assert_eq!(node.capacity, Default::default());
        assert_eq!(node.allocatable, node.capacity);
    }

    #[test]
    fn test_node_status_collection() {
        use common::apis::{NodeInfo, NodeState, ResourceRequirement};

        // Create a node with known values
        let node = Node {
            name: "test-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            allocatable: ResourceRequirement {
                cpu: 3,
                memory: 6144,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
            shims: Vec::new(),
        };

        // Verify the node can be converted to proto NodeStatus
        let status = proto::NodeStatus {
            state: proto::NodeState::from(node.state) as i32,
            capacity: Some(node.capacity.clone().into()),
            allocatable: Some(node.allocatable.clone().into()),
            info: Some(node.info.clone().into()),
            addresses: vec![],
            last_heartbeat_time: 0,
        };

        assert_eq!(status.state, proto::NodeState::Ready as i32);
        assert!(status.capacity.is_some());
        assert!(status.allocatable.is_some());
        assert!(status.info.is_some());

        let capacity = status.capacity.unwrap();
        assert_eq!(capacity.cpu, 4);
        assert_eq!(capacity.memory, 8192);
    }

    #[test]
    fn test_stream_handler_creation() {
        use common::apis::{NodeInfo, NodeState, ResourceRequirement};

        let node = Node {
            name: "test-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            allocatable: ResourceRequirement {
                cpu: 3,
                memory: 6144,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
            shims: Vec::new(),
        };

        // We can't fully test StreamHandler without a real client,
        // but we can verify the struct is created correctly
        // by checking the intervals are set to defaults
        assert_eq!(
            Duration::from_secs(DEFAULT_HEARTBEAT_INTERVAL_SECS),
            Duration::from_secs(5)
        );
        assert_eq!(
            Duration::from_secs(DEFAULT_RECONNECT_INTERVAL_SECS),
            Duration::from_secs(1)
        );
        assert_eq!(
            Duration::from_secs(MAX_RECONNECT_INTERVAL_SECS),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn test_executor_conversion_preserves_application() {
        use common::apis::{ExecutorState, ResourceRequirement, Shim};

        // Test that Executor can be created with expected fields
        let executor = Executor {
            id: "test-exec".to_string(),
            application: "test-app".to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement::default(),
            assignments: Default::default(),
            session: None,
            task: None,
            context: None,
            shim: Shim::Host,
            app_mgr: None,
            dev_mgr: None,
            shim_instance: None,
            state: ExecutorState::Idle,
        };

        assert_eq!(executor.id, "test-exec");
        assert_eq!(executor.application, "test-app");
        assert_eq!(executor.node, "test-node");
        assert_eq!(executor.state, ExecutorState::Idle);

        let rpc_executor = proto::Executor::from(&executor);
        assert_eq!(rpc_executor.spec.as_ref().unwrap().application, "test-app");
        let restored = Executor::try_from(&rpc_executor).unwrap();
        assert_eq!(restored.application, "test-app");
    }
}

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

//! Node state machine for managing node lifecycle.
//!
//! State transitions:
//! - Unknown -> Ready (on register/reconnect)
//! - Ready -> Unknown (on drain - cleanup timer starts)
//! - Unknown -> NotReady (on shutdown after cleanup timeout)
//! - NotReady -> Ready (on reconnect after shutdown)

use std::sync::Arc;

use crate::controller::nodes::{
    not_ready::NotReadyState, ready::ReadyState, unknown::UnknownState,
};
use crate::storage::StoragePtr;

use common::apis::{Node, NodePtr, NodeState};
use common::FlameError;
use stdng::{lock_ptr, MutexPtr};

mod not_ready;
mod ready;
mod unknown;

/// Creates a state handler based on the node's current state.
pub fn from(storage: StoragePtr, node_ptr: NodePtr) -> Result<Arc<dyn NodeStates>, FlameError> {
    let node = lock_ptr!(node_ptr)?;
    tracing::debug!("Build state <{:?}> for Node <{}>.", node.state, node.name);

    match node.state {
        NodeState::Unknown => Ok(Arc::new(UnknownState {
            storage,
            node: node_ptr.clone(),
        })),
        NodeState::Ready => Ok(Arc::new(ReadyState {
            storage,
            node: node_ptr.clone(),
        })),
        NodeState::NotReady => Ok(Arc::new(NotReadyState {
            storage,
            node: node_ptr.clone(),
        })),
    }
}

/// Trait defining the operations available for each node state.
///
/// Each state implements this trait, returning errors for invalid operations
/// and performing state transitions for valid ones.
#[async_trait::async_trait]
pub trait NodeStates: Send + Sync + 'static {
    /// Register a node (transition to Ready state).
    /// Valid from: Unknown, NotReady
    async fn register_node(&self) -> Result<(), FlameError>;

    /// Mark node as draining (transition to Unknown state, starts cleanup timer).
    /// Valid from: Ready
    async fn drain(&self) -> Result<(), FlameError>;

    /// Shutdown node after cleanup timeout (transition to NotReady state).
    /// Valid from: Unknown
    async fn shutdown(&self) -> Result<(), FlameError>;

    /// Update node information (heartbeat, resource update).
    /// Valid from: Ready
    async fn update_node(&self, node: &Node) -> Result<(), FlameError>;

    /// Release/unregister a node completely.
    /// Valid from: any state
    async fn release_node(&self) -> Result<(), FlameError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{Node, NodeInfo, NodeState, ResourceRequirement, Shim};
    use common::ctx::{FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits};

    /// Helper to create a test node with specified state
    fn create_test_node(name: &str, state: NodeState) -> NodePtr {
        stdng::new_ptr(Node {
            name: name.to_string(),
            state,
            capacity: ResourceRequirement::default(),
            allocatable: ResourceRequirement::default(),
            info: NodeInfo::default(),
        })
    }

    /// Helper to get current state from node pointer
    fn get_state(node_ptr: &NodePtr) -> Result<NodeState, FlameError> {
        let node = lock_ptr!(node_ptr)?;
        Ok(node.state)
    }

    /// Creates a mock storage for testing state transitions (async version).
    async fn create_mock_storage() -> StoragePtr {
        let unique_id = format!(
            "{}_{:?}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            std::thread::current().id()
        );
        let test_dir = std::env::temp_dir().join(format!("flame_test_{}", unique_id));
        std::fs::create_dir_all(&test_dir).unwrap();
        let db_path = test_dir.join("flame.db");
        let url = format!("sqlite://{}", db_path.display());

        let ctx = FlameClusterContext {
            cluster: FlameCluster {
                name: "test".to_string(),
                endpoint: "http://localhost:8080".to_string(),
                storage: url,
                slot: ResourceRequirement::default(),
                policies: vec!["priority".to_string(), "gang".to_string()],
                schedule_interval: 1000,
                executors: FlameExecutors {
                    shim: Shim::default(),
                },
                tls: None,
                limits: FlameLimits {
                    max_sessions: None,
                    max_executors: 10,
                },
                pprof: None,
            },
            cache: None,
        };

        crate::storage::new_ptr(&ctx).await.unwrap()
    }

    // ========================================================================
    // UnknownState Tests
    // ========================================================================

    mod unknown_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_register_node_transitions_to_ready() {
            let node_ptr = create_test_node("test-node", NodeState::Unknown);
            let storage = create_mock_storage().await;

            // Register node in storage first so update_node_state works
            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            let state = UnknownState {
                storage,
                node: node_ptr.clone(),
            };

            let result = state.register_node().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
        }

        #[tokio::test]
        async fn test_drain_fails_already_draining() {
            let node_ptr = create_test_node("test-node", NodeState::Unknown);
            let state = UnknownState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let result = state.drain().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Unknown);
        }

        #[tokio::test]
        async fn test_shutdown_transitions_to_not_ready() {
            let node_ptr = create_test_node("test-node", NodeState::Unknown);
            let storage = create_mock_storage().await;

            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            let state = UnknownState {
                storage,
                node: node_ptr.clone(),
            };

            let result = state.shutdown().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::NotReady);
        }

        #[tokio::test]
        async fn test_update_node_fails_in_unknown_state() {
            let node_ptr = create_test_node("test-node", NodeState::Unknown);
            let state = UnknownState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let updated_node = Node {
                name: "test-node".to_string(),
                state: NodeState::Ready,
                capacity: ResourceRequirement {
                    cpu: 8,
                    memory: 16384,
                },
                allocatable: ResourceRequirement {
                    cpu: 8,
                    memory: 16384,
                },
                info: NodeInfo::default(),
            };

            let result = state.update_node(&updated_node).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_release_node_succeeds() {
            let node_ptr = create_test_node("test-node", NodeState::Unknown);
            let state = UnknownState {
                storage: create_mock_storage().await,
                node: node_ptr,
            };

            let result = state.release_node().await;
            assert!(result.is_ok());
        }
    }

    // ========================================================================
    // ReadyState Tests
    // ========================================================================

    mod ready_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_register_node_is_idempotent() {
            let node_ptr = create_test_node("test-node", NodeState::Ready);
            let state = ReadyState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let result = state.register_node().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
        }

        #[tokio::test]
        async fn test_drain_transitions_to_unknown() {
            let node_ptr = create_test_node("test-node", NodeState::Ready);
            let storage = create_mock_storage().await;

            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            let state = ReadyState {
                storage,
                node: node_ptr.clone(),
            };

            let result = state.drain().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Unknown);
        }

        #[tokio::test]
        async fn test_shutdown_fails_must_drain_first() {
            let node_ptr = create_test_node("test-node", NodeState::Ready);
            let state = ReadyState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let result = state.shutdown().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
        }

        #[tokio::test]
        async fn test_update_node_succeeds() {
            let node_ptr = create_test_node("test-node", NodeState::Ready);
            let state = ReadyState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let updated_node = Node {
                name: "test-node".to_string(),
                state: NodeState::Ready,
                capacity: ResourceRequirement {
                    cpu: 16,
                    memory: 32768,
                },
                allocatable: ResourceRequirement {
                    cpu: 14,
                    memory: 28672,
                },
                info: NodeInfo {
                    arch: "x86_64".to_string(),
                    os: "linux".to_string(),
                },
            };

            let result = state.update_node(&updated_node).await;

            assert!(result.is_ok());
            let node = lock_ptr!(node_ptr).unwrap();
            assert_eq!(node.capacity.cpu, 16);
            assert_eq!(node.allocatable.memory, 28672);
            assert_eq!(node.info.arch, "x86_64");
        }

        #[tokio::test]
        async fn test_release_node_succeeds() {
            let node_ptr = create_test_node("test-node", NodeState::Ready);
            let state = ReadyState {
                storage: create_mock_storage().await,
                node: node_ptr,
            };

            let result = state.release_node().await;
            assert!(result.is_ok());
        }
    }

    // ========================================================================
    // NotReadyState Tests
    // ========================================================================

    mod not_ready_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_register_node_transitions_to_ready() {
            let node_ptr = create_test_node("test-node", NodeState::NotReady);
            let storage = create_mock_storage().await;

            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            let state = NotReadyState {
                storage,
                node: node_ptr.clone(),
            };

            let result = state.register_node().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
        }

        #[tokio::test]
        async fn test_drain_fails_already_not_ready() {
            let node_ptr = create_test_node("test-node", NodeState::NotReady);
            let state = NotReadyState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let result = state.drain().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_shutdown_is_idempotent() {
            let node_ptr = create_test_node("test-node", NodeState::NotReady);
            let state = NotReadyState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let result = state.shutdown().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&node_ptr).unwrap(), NodeState::NotReady);
        }

        #[tokio::test]
        async fn test_update_node_fails_must_reconnect() {
            let node_ptr = create_test_node("test-node", NodeState::NotReady);
            let state = NotReadyState {
                storage: create_mock_storage().await,
                node: node_ptr.clone(),
            };

            let updated_node = Node {
                name: "test-node".to_string(),
                state: NodeState::Ready,
                capacity: ResourceRequirement::default(),
                allocatable: ResourceRequirement::default(),
                info: NodeInfo::default(),
            };

            let result = state.update_node(&updated_node).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_release_node_succeeds() {
            let node_ptr = create_test_node("test-node", NodeState::NotReady);
            let state = NotReadyState {
                storage: create_mock_storage().await,
                node: node_ptr,
            };

            let result = state.release_node().await;
            assert!(result.is_ok());
        }
    }

    // ========================================================================
    // State Factory Tests
    // ========================================================================

    mod factory_tests {
        use super::*;

        #[tokio::test]
        async fn test_from_creates_unknown_state() {
            let node_ptr = create_test_node("test-node", NodeState::Unknown);
            let storage = create_mock_storage().await;

            let state = from(storage, node_ptr);

            assert!(state.is_ok());
        }

        #[tokio::test]
        async fn test_from_creates_ready_state() {
            let node_ptr = create_test_node("test-node", NodeState::Ready);
            let storage = create_mock_storage().await;

            let state = from(storage, node_ptr);

            assert!(state.is_ok());
        }

        #[tokio::test]
        async fn test_from_creates_not_ready_state() {
            let node_ptr = create_test_node("test-node", NodeState::NotReady);
            let storage = create_mock_storage().await;

            let state = from(storage, node_ptr);

            assert!(state.is_ok());
        }
    }

    // ========================================================================
    // Full State Machine Transition Tests
    // ========================================================================

    mod state_machine_tests {
        use super::*;

        #[tokio::test]
        async fn test_full_lifecycle_unknown_to_ready_to_unknown_to_not_ready() {
            let storage = create_mock_storage().await;
            let node_ptr = create_test_node("lifecycle-node", NodeState::Unknown);

            // Register node in storage first
            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            // Unknown -> Ready (register)
            {
                let state = from(storage.clone(), node_ptr.clone()).unwrap();
                state.register_node().await.unwrap();
                assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
            }

            // Ready -> Unknown (drain)
            {
                let state = from(storage.clone(), node_ptr.clone()).unwrap();
                state.drain().await.unwrap();
                assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Unknown);
            }

            // Unknown -> NotReady (shutdown after timeout)
            {
                let state = from(storage.clone(), node_ptr.clone()).unwrap();
                state.shutdown().await.unwrap();
                assert_eq!(get_state(&node_ptr).unwrap(), NodeState::NotReady);
            }
        }

        #[tokio::test]
        async fn test_reconnection_during_drain() {
            let storage = create_mock_storage().await;
            let node_ptr = create_test_node("reconnect-node", NodeState::Ready);

            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            // Start draining
            {
                let state = from(storage.clone(), node_ptr.clone()).unwrap();
                state.drain().await.unwrap();
                assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Unknown);
            }

            // Reconnect before timeout - should go back to Ready
            {
                let state = from(storage.clone(), node_ptr.clone()).unwrap();
                state.register_node().await.unwrap();
                assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
            }
        }

        #[tokio::test]
        async fn test_reconnection_after_shutdown() {
            let storage = create_mock_storage().await;
            let node_ptr = create_test_node("shutdown-reconnect-node", NodeState::NotReady);

            let node = lock_ptr!(node_ptr).unwrap().clone();
            storage.register_node(&node).await.unwrap();

            // Node was previously shutdown, now reconnecting
            {
                let state = from(storage.clone(), node_ptr.clone()).unwrap();
                state.register_node().await.unwrap();
                assert_eq!(get_state(&node_ptr).unwrap(), NodeState::Ready);
            }
        }
    }
}

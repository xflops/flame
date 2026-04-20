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

//! Tests for node and executor persistence logic.
//!
//! These tests verify the database persistence for:
//! 1. Node CRUD operations
//! 2. Executor CRUD operations
//! 3. Executor state updates

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use common::apis::{ExecutorState, Node, NodeInfo, NodeState, ResourceRequirement, Shim};
    use common::FlameError;

    use crate::model::Executor;
    use crate::storage::engine::Engine;
    use crate::storage::engine::SqliteEngine;

    /// Test node CRUD operations.
    #[test]
    fn test_node_crud() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_node_crud");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        // Create a node
        let node = Node {
            name: "test-node-1".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 6,
                memory: 12288,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };

        let created = tokio_test::block_on(storage.create_node(&node))?;
        assert_eq!(created.name, "test-node-1");
        assert_eq!(created.state, NodeState::Ready);
        assert_eq!(created.capacity.cpu, 8);

        // Get the node
        let retrieved = tokio_test::block_on(storage.get_node("test-node-1"))?;
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.name, "test-node-1");
        assert_eq!(retrieved.info.arch, "x86_64");

        // Update the node
        let updated_node = Node {
            name: "test-node-1".to_string(),
            state: NodeState::NotReady,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };

        let updated = tokio_test::block_on(storage.update_node(&updated_node))?;
        assert_eq!(updated.state, NodeState::NotReady);
        assert_eq!(updated.allocatable.cpu, 4);

        // Find all nodes
        let nodes = tokio_test::block_on(storage.find_nodes())?;
        assert_eq!(nodes.len(), 1);

        // Delete the node
        tokio_test::block_on(storage.delete_node("test-node-1"))?;

        // Verify deletion
        let deleted = tokio_test::block_on(storage.get_node("test-node-1"))?;
        assert!(deleted.is_none());

        Ok(())
    }

    /// Test executor CRUD operations.
    #[test]
    fn test_executor_crud() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_executor_crud");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        // First create a node (required for foreign key)
        let node = Node {
            name: "test-node-exec".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };
        tokio_test::block_on(storage.create_node(&node))?;

        // Create an executor
        let executor = Executor {
            id: "exec-1".to_string(),
            node: "test-node-exec".to_string(),
            resreq: ResourceRequirement {
                cpu: 2,
                memory: 4096,
            },
            slots: 2,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Void,
        };

        let created = tokio_test::block_on(storage.create_executor(&executor))?;
        assert_eq!(created.id, "exec-1");
        assert_eq!(created.state, ExecutorState::Void);

        // Get the executor
        let retrieved = tokio_test::block_on(storage.get_executor(&"exec-1".to_string()))?;
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.node, "test-node-exec");
        assert_eq!(retrieved.slots, 2);

        // Update executor state
        let updated = tokio_test::block_on(
            storage.update_executor_state(&"exec-1".to_string(), ExecutorState::Idle),
        )?;
        assert_eq!(updated.state, ExecutorState::Idle);

        // Find executors by node
        let executors = tokio_test::block_on(storage.find_executors(Some("test-node-exec")))?;
        assert_eq!(executors.len(), 1);

        // Find all executors
        let all_executors = tokio_test::block_on(storage.find_executors(None))?;
        assert_eq!(all_executors.len(), 1);

        // Delete the executor
        tokio_test::block_on(storage.delete_executor(&"exec-1".to_string()))?;

        // Verify deletion
        let deleted = tokio_test::block_on(storage.get_executor(&"exec-1".to_string()))?;
        assert!(deleted.is_none());

        Ok(())
    }

    /// Test that deleting a node cascades to executors.
    #[test]
    fn test_node_delete_cascades_to_executors() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_node_cascade");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        // Create a node
        let node = Node {
            name: "cascade-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };
        tokio_test::block_on(storage.create_node(&node))?;

        // Create executors on the node
        for i in 1..=3 {
            let executor = Executor {
                id: format!("cascade-exec-{}", i),
                node: "cascade-node".to_string(),
                resreq: ResourceRequirement {
                    cpu: 2,
                    memory: 4096,
                },
                slots: 2,
                shim: Shim::Host,
                task_id: None,
                ssn_id: None,
                creation_time: Utc::now(),
                state: ExecutorState::Idle,
            };
            tokio_test::block_on(storage.create_executor(&executor))?;
        }

        // Verify executors exist
        let executors = tokio_test::block_on(storage.find_executors(Some("cascade-node")))?;
        assert_eq!(executors.len(), 3);

        // Delete the node (should cascade to executors)
        tokio_test::block_on(storage.delete_node("cascade-node"))?;

        // Verify executors are deleted
        let executors = tokio_test::block_on(storage.find_executors(None))?;
        assert_eq!(executors.len(), 0);

        Ok(())
    }

    /// Test executor state transitions.
    #[test]
    fn test_executor_state_transitions() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_executor_states");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        // Create a node
        let node = Node {
            name: "state-node".to_string(),
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
        tokio_test::block_on(storage.create_node(&node))?;

        // Create an executor
        let executor = Executor {
            id: "state-exec".to_string(),
            node: "state-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 2,
                memory: 4096,
            },
            slots: 2,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Void,
        };
        tokio_test::block_on(storage.create_executor(&executor))?;

        // Test state transitions
        let states = vec![
            ExecutorState::Idle,
            ExecutorState::Binding,
            ExecutorState::Bound,
            ExecutorState::Unbinding,
            ExecutorState::Releasing,
            ExecutorState::Released,
        ];

        for state in states {
            let updated = tokio_test::block_on(
                storage.update_executor_state(&"state-exec".to_string(), state),
            )?;
            assert_eq!(updated.state, state);
        }

        Ok(())
    }
}

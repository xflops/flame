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

use crate::apis::{ExecutorState, Node, NodeInfo, NodeState, ResourceRequirement, Shim};
use crate::FlameError;
use chrono::Utc;

use crate::apis::Executor;
use crate::storage::engine::connect;

/// Detailed reports must survive the engine/cache boundary without new persistence columns.
#[tokio::test]
async fn registration_preserves_live_inventory_for_all_storage_engines() {
    use crate::apis::NodeNetworkInterface;
    use crate::ctx::{FlameCluster, FlameClusterContext};
    let dir = tempfile::tempdir().unwrap();
    for url in [
        "none".to_string(),
        format!("fs://{}", dir.path().join("inventory-fs").display()),
    ] {
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: url,
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        let mut node = Node {
            name: "inventory-worker".into(),
            info: NodeInfo {
                arch: "x86_64".into(),
                os: "linux".into(),
                kernel_version: "6.8.0".into(),
                network_interfaces: vec![NodeNetworkInterface {
                    name: "eth0".into(),
                    addresses: vec!["192.168.3.149/24".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        storage.register_node(&node).await.unwrap();
        assert_eq!(
            storage.get_node(&node.name).unwrap().unwrap().info,
            node.info
        );
        node.info.network_interfaces[0].addresses = vec!["192.168.3.150/24".into()];
        storage.register_node(&node).await.unwrap();
        assert_eq!(
            storage.get_node(&node.name).unwrap().unwrap().info,
            node.info
        );
    }
}

#[tokio::test]
async fn release_node_enforces_safety_without_a_controller() {
    use crate::apis::{ApplicationAttributes, SessionAttributes};
    use crate::ctx::{FlameCluster, FlameClusterContext};

    let dir = tempfile::tempdir().unwrap();
    for url in [
        "none".to_string(),
        format!("fs://{}", dir.path().join("fs").display()),
    ] {
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: url,
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        assert!(matches!(
            storage.release_node(" ").await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(matches!(
            storage.release_node("missing").await,
            Err(FlameError::NotFound(_))
        ));
        let node = Node {
            name: "worker".into(),
            state: NodeState::Ready,
            ..Default::default()
        };
        storage.register_node(&node).await.unwrap();
        assert!(matches!(
            storage.release_node("worker").await,
            Err(FlameError::InvalidState(_))
        ));
        assert_eq!(
            storage.get_node("worker").unwrap().unwrap().state,
            NodeState::Ready
        );

        storage
            .register_application("app".into(), ApplicationAttributes::default())
            .await
            .unwrap();
        let session = storage
            .create_session(SessionAttributes {
                id: format!("release-storage-{}", uuid::Uuid::new_v4()),
                application: "app".into(),
                resreq: Some(ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        storage
            .update_node_state("worker", NodeState::Unknown)
            .await
            .unwrap();
        let mut executor = storage
            .create_executor("worker".into(), session.id)
            .await
            .unwrap();
        for state in [
            ExecutorState::Void,
            ExecutorState::Releasing,
            ExecutorState::Released,
        ] {
            executor.state = state;
            storage.update_executor(&executor).await.unwrap();
            assert!(matches!(
                storage.release_node("worker").await,
                Err(FlameError::InvalidState(_))
            ));
            assert!(storage.get_node("worker").unwrap().is_some());
            assert!(storage.get_executor_ptr(executor.id.clone()).is_ok());
        }
        storage.delete_executor(executor.id).await.unwrap();
        storage.release_node("worker").await.unwrap();
        assert!(storage.get_node("worker").unwrap().is_none());
        storage
            .register_node(&Node {
                state: NodeState::NotReady,
                ..node
            })
            .await
            .unwrap();
        storage.release_node("worker").await.unwrap();
        assert!(storage.get_node("worker").unwrap().is_none());
    }
}

#[tokio::test]
async fn failed_node_deletion_preserves_cached_node_and_allows_retry() {
    use crate::ctx::{FlameCluster, FlameClusterContext};
    let dir = tempfile::tempdir().unwrap();
    let context = FlameClusterContext {
        cluster: FlameCluster {
            storage: format!("fs://{}", dir.path().display()),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = crate::storage::new_ptr(&context).await.unwrap();
    storage
        .register_node(&Node {
            name: "worker".into(),
            state: NodeState::Unknown,
            ..Default::default()
        })
        .await
        .unwrap();
    let original = storage.get_node_ptr("worker").unwrap();
    let path = dir.path().join("nodes/worker");
    let backup = dir.path().join("worker-backup");
    // Replacing the directory with a file makes remove_dir_all fail while
    // retaining the actual persisted node for a successful retry.
    std::fs::rename(&path, &backup).unwrap();
    std::fs::write(&path, b"blocked").unwrap();
    assert!(storage.release_node("worker").await.is_err());
    let cached = storage.get_node_ptr("worker").unwrap();
    assert!(std::sync::Arc::ptr_eq(&cached, &original));
    assert!(backup.join("metadata").is_file());
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    storage.release_node("worker").await.unwrap();
    assert!(storage.get_node("worker").unwrap().is_none());
    assert!(!path.exists());
}

/// Test node CRUD operations.
#[test]
fn test_node_crud() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    // Create a node
    let node = Node {
        name: "test-node-1".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 6,
            memory: 12288,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
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
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 4,
            memory: 8192,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
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
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    // First create a node (required for executor ownership)
    let node = Node {
        name: "test-node-exec".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
    };
    tokio_test::block_on(storage.create_node(&node))?;

    // Create an executor
    let executor = Executor {
        id: "exec-1".to_string(),
        node: "test-node-exec".to_string(),
        resreq: ResourceRequirement {
            cpu: 2,
            memory: 4096,
            gpu: 0,
        },
        shim: Shim::Host,
        application: "test-app".to_string(),
        task_id: None,
        ssn_id: None,
        signal: None,
        attributes: Default::default(),
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state: ExecutorState::Void,
    };

    let created = tokio_test::block_on(storage.create_executor(&executor))?;
    assert_eq!(created.id, "exec-1");
    assert_eq!(created.state, ExecutorState::Void);
    assert_eq!(created.application, "test-app");

    // Get the executor
    let retrieved = tokio_test::block_on(storage.get_executor(&"exec-1".to_string()))?;
    assert!(retrieved.is_some());
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.node, "test-node-exec");
    assert_eq!(retrieved.application, "test-app");

    // Update executor state
    let updated = tokio_test::block_on(
        storage.update_executor_state(&"exec-1".to_string(), ExecutorState::Idle),
    )?;
    assert_eq!(updated.state, ExecutorState::Idle);
    assert_eq!(updated.application, "test-app");

    // Find executors by node
    let executors = tokio_test::block_on(storage.find_executors(Some("test-node-exec")))?;
    assert_eq!(executors.len(), 1);
    assert_eq!(executors[0].application, "test-app");

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
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    // Create a node
    let node = Node {
        name: "cascade-node".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
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
                gpu: 0,
            },
            shim: Shim::Host,
            application: "test-app".to_string(),
            task_id: None,
            ssn_id: None,
            signal: None,
            attributes: Default::default(),
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
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
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    // Create a node
    let node = Node {
        name: "state-node".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        info: NodeInfo::default(),
        shims: Vec::new(),
    };
    tokio_test::block_on(storage.create_node(&node))?;

    // Create an executor
    let executor = Executor {
        id: "state-exec".to_string(),
        node: "state-node".to_string(),
        resreq: ResourceRequirement {
            cpu: 2,
            memory: 4096,
            gpu: 0,
        },
        shim: Shim::Host,
        application: "test-app".to_string(),
        task_id: None,
        ssn_id: None,
        signal: None,
        attributes: Default::default(),
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
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
        let updated =
            tokio_test::block_on(storage.update_executor_state(&"state-exec".to_string(), state))?;
        assert_eq!(updated.state, state);
    }

    Ok(())
}

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
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 14,
            memory: 28672,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: vec![common::apis::Shim::Host],
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

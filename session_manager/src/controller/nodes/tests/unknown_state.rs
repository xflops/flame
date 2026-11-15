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
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        info: NodeInfo::default(),
        shims: vec![common::apis::Shim::Host],
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

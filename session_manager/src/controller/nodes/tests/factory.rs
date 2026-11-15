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

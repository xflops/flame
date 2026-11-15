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

use super::*;

#[tokio::test]
async fn test_drain_node_transitions_to_unknown() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let node = create_test_node("drain-test-node");

    // Connect node
    controller.register_node(&node, &[]).await.unwrap();

    // Drain node
    let result = controller.drain_node("drain-test-node").await;

    assert!(result.is_ok());

    // Verify node state is Unknown (draining)
    let stored_node = storage.get_node("drain-test-node").unwrap().unwrap();
    assert_eq!(stored_node.state, NodeState::Unknown);
}

#[tokio::test]
async fn test_drain_nonexistent_node_succeeds() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);

    // Draining a node that doesn't exist should not error
    let result = controller.drain_node("nonexistent-node").await;

    assert!(result.is_ok());
}

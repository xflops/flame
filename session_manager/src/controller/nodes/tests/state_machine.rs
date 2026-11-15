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

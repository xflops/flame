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
use std::collections::HashSet;
use std::time::Duration;

#[tokio::test]
async fn test_update_node_does_not_transition_state() {
    // This tests that update_node (used for heartbeat) does NOT transition state
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let node = create_test_node("heartbeat-node");

    // First register the node (sets it to Unknown state in storage)
    storage.register_node(&node).await.unwrap();

    // Now call update_node (heartbeat case)
    let result = controller.update_node(&node).await;

    assert!(result.is_ok());

    // Verify node state was NOT changed to Ready
    // The node should remain in whatever state it was in storage
    let stored_node = storage.get_node("heartbeat-node").unwrap();
    assert!(stored_node.is_some());
    // Note: The initial state is Unknown, and without a stream connection,
    // it should NOT transition to Ready
}

#[tokio::test]
async fn test_register_node_with_stream_transitions_to_ready() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let node = create_test_node("stream-node");

    // Register node
    let result = controller.register_node(&node, &[]).await;

    assert!(result.is_ok());

    // Verify node state WAS changed to Ready (via on_connected callback)
    let stored_node = storage.get_node("stream-node").unwrap();
    assert!(stored_node.is_some());
    assert_eq!(stored_node.unwrap().state, NodeState::Ready);
}

#[tokio::test]
async fn test_register_node_fresh_connection_succeeds() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let node = create_test_node("fresh-node");

    // Fresh connection with no reported executors
    let result = controller.register_node(&node, &[]).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn missing_persisted_executor_is_sent_for_targeted_release() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());
    let node = create_test_node("recovery-node");
    storage.register_node(&node).await.unwrap();
    controller
        .register_application("test-app".to_string(), create_test_application())
        .await
        .unwrap();
    let session = controller
        .create_session(create_test_session_attr("recovery-session"))
        .await
        .unwrap();
    let executor = storage
        .create_executor(node.name.clone(), session.id)
        .await
        .unwrap();

    controller.register_node(&node, &[]).await.unwrap();

    let (_, receiver) = controller.get_node_channel(&node.name).unwrap().unwrap();
    let cleanup = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleanup.id, executor.id);
    assert_eq!(cleanup.state, ExecutorState::Releasing);
    assert_eq!(
        controller.get_executor(executor.id).unwrap().state,
        ExecutorState::Releasing
    );
}

#[tokio::test]
async fn cleanup_ack_fails_interrupted_task_without_retry() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());
    let node = create_test_node("terminal-recovery-node");
    storage.register_node(&node).await.unwrap();
    controller
        .register_application("test-app".to_string(), create_test_application())
        .await
        .unwrap();
    let session = controller
        .create_session(create_test_session_attr("terminal-recovery-session"))
        .await
        .unwrap();
    let task = controller
        .create_task(session.id.clone(), None, None)
        .await
        .unwrap();
    let session_ptr = storage.get_session_ptr(session.id.clone()).unwrap();
    let task_ptr = storage
        .get_task_ptr(TaskGID {
            ssn_id: session.id.clone(),
            task_id: task.id,
        })
        .unwrap();
    storage
        .update_task_state(session_ptr, task_ptr, TaskState::Running, None)
        .await
        .unwrap();
    let mut executor = storage
        .create_executor(node.name, session.id.clone())
        .await
        .unwrap();
    executor.state = ExecutorState::Releasing;
    executor.ssn_id = Some(session.id.clone());
    executor.task_id = Some(task.id);
    storage.update_executor(&executor).await.unwrap();

    controller
        .unregister_executor(executor.id.clone())
        .await
        .unwrap();

    assert_eq!(
        controller
            .get_task(session.id, task.id)
            .await
            .unwrap()
            .state,
        TaskState::Failed
    );
    assert!(matches!(
        controller.get_executor(executor.id).unwrap_err(),
        FlameError::NotFound(_)
    ));
}

#[tokio::test]
async fn test_register_node_replaces_existing_connection() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let node = create_test_node("replace-node");

    // First connection
    controller.register_node(&node, &[]).await.unwrap();

    // Second connection (replaces first)
    let result = controller.register_node(&node, &[]).await;

    assert!(result.is_ok());
    // Node should still be Ready
    let stored_node = storage.get_node("replace-node").unwrap().unwrap();
    assert_eq!(stored_node.state, NodeState::Ready);
}

#[tokio::test]
async fn test_register_node_preserves_persisted_executor_application() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    let executor_id = create_binding_executor(&controller, "reconnect-application").await;
    let mut reported = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(reported.application, "test-app");
    reported.application = "other-app".to_string();

    controller
        .register_node(&create_test_node("bind-node"), &[reported])
        .await
        .unwrap();

    assert_eq!(
        controller.get_executor(executor_id).unwrap().application,
        "test-app"
    );
}

#[tokio::test]
async fn test_register_node_rejects_a_new_node_that_reports_executors() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());
    let node = create_test_node("new-busy-node");
    let reported = Executor {
        id: "running-executor".to_string(),
        node: "new-busy-node".to_string(),
        application: "test-app".to_string(),
        state: ExecutorState::Bound,
        ..Default::default()
    };

    let result = controller.register_node(&node, &[reported]).await;

    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    // Nothing was recorded for the rejected node.
    assert!(storage.get_node("new-busy-node").unwrap().is_none());
    assert!(matches!(
        controller.get_executor("running-executor".to_string()),
        Err(FlameError::NotFound(_))
    ));

    // After the admin clears it, it joins with no executors.
    controller.register_node(&node, &[]).await.unwrap();
    assert!(storage.get_node("new-busy-node").unwrap().is_some());
}

#[tokio::test]
async fn test_register_node_rejects_executors_unknown_to_a_known_node() {
    let controller = new_ptr(create_test_storage().await);
    let known_id = create_binding_executor(&controller, "known-node-unknown-executor").await;
    let known = controller.get_executor(known_id.clone()).unwrap();
    let node = create_test_node("bind-node");
    let unknown = Executor {
        id: "unknown-executor".to_string(),
        node: "bind-node".to_string(),
        application: "test-app".to_string(),
        state: ExecutorState::Bound,
        ..Default::default()
    };

    let result = controller.register_node(&node, &[known, unknown]).await;

    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    // The rejection changes nothing: the known executor is untouched.
    assert_eq!(
        controller.get_executor(known_id).unwrap().state,
        ExecutorState::Binding
    );
    assert!(matches!(
        controller.get_executor("unknown-executor".to_string()),
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn test_register_node_reconnect_during_drain() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let node = create_test_node("drain-reconnect-node");

    // Initial connection
    controller.register_node(&node, &[]).await.unwrap();

    // Start draining
    controller.drain_node("drain-reconnect-node").await.unwrap();

    // Verify node is in Unknown state (draining)
    let stored_node = storage.get_node("drain-reconnect-node").unwrap().unwrap();
    assert_eq!(stored_node.state, NodeState::Unknown);

    // Reconnect before timeout
    let result = controller.register_node(&node, &[]).await;

    assert!(result.is_ok());
    // Node should be back to Ready
    let stored_node = storage.get_node("drain-reconnect-node").unwrap().unwrap();
    assert_eq!(stored_node.state, NodeState::Ready);
}

#[tokio::test]
async fn test_register_node_updates_node_info() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());

    let mut node = create_test_node("update-node");

    // Initial registration
    controller.register_node(&node, &[]).await.unwrap();

    // Update node info
    node.capacity = ResourceRequirement {
        cpu: 16,
        memory: 32768,
        gpu: 0,
    };
    node.allocatable = ResourceRequirement {
        cpu: 14,
        memory: 28672,
        gpu: 0,
    };

    controller.register_node(&node, &[]).await.unwrap();

    // Verify info was updated
    let stored_node = storage.get_node("update-node").unwrap().unwrap();
    assert_eq!(stored_node.capacity.cpu, 16);
    assert_eq!(stored_node.allocatable.memory, 28672);
}

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
use uuid::Uuid;

async fn persistent_lifecycle_executor() -> (tempfile::TempDir, ControllerPtr, Executor) {
    let directory = tempfile::TempDir::new().unwrap();
    let url = format!("fs://{}", directory.path().display());
    let ctx = FlameClusterContext {
        cluster: FlameCluster {
            storage: url.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    let controller = new_ptr(crate::storage::new_ptr(&ctx).await.unwrap());
    let session_id = "persist-before-assignment".to_string();
    controller
        .register_application("test-app".to_string(), create_test_application())
        .await
        .unwrap();
    controller
        .storage()
        .register_node(&create_test_node("persist-binding-node"))
        .await
        .unwrap();
    controller
        .create_session(create_test_session_attr(&session_id))
        .await
        .unwrap();
    let executor = controller
        .create_executor("persist-binding-node".to_string(), session_id.clone())
        .await
        .unwrap();
    (directory, controller, executor)
}

/// Force persistence to fail before publication, then retry after repairing the path.
async fn assert_transition_is_staged<F, Fut>(
    directory: &std::path::Path,
    executor: &ExecutorPtr,
    mut transition: F,
    expected: ExecutorState,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), FlameError>>,
{
    let before = lock_ptr!(executor).unwrap().clone();
    let parent = directory.join("nodes").join(&before.node).join("executors");
    let saved = parent.with_extension("saved");
    std::fs::rename(&parent, &saved).unwrap();
    std::fs::write(&parent, b"block executor persistence").unwrap();
    let failed = transition().await;
    std::fs::remove_file(&parent).unwrap();
    std::fs::rename(&saved, &parent).unwrap();
    assert!(matches!(failed, Err(FlameError::Storage(_))));
    {
        let current = lock_ptr!(executor).unwrap();
        assert_eq!(current.state, before.state);
        assert_eq!(current.ssn_id, before.ssn_id);
        assert_eq!(current.task_id, before.task_id);
        assert_eq!(current.signal, before.signal);
    }
    transition().await.unwrap();
    assert_eq!(lock_ptr!(executor).unwrap().state, expected);
    let metadata = parent.join(&before.id).join("metadata");
    if expected == ExecutorState::Released {
        assert!(!metadata.exists());
    } else {
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(metadata).unwrap()).unwrap();
        assert_eq!(persisted["state"], i32::from(expected));
    }
}

#[tokio::test]
async fn lifecycle_transitions_are_visible_only_after_persistence() {
    let (directory, controller, executor) = persistent_lifecycle_executor().await;
    let shared = controller
        .storage()
        .get_executor_ptr(executor.id.clone())
        .unwrap();
    assert_eq!(lock_ptr!(shared).unwrap().state, ExecutorState::Void);
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.register_executor(&executor),
        ExecutorState::Idle,
    )
    .await;
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.bind_session(executor.id.clone(), "persist-before-assignment".to_string()),
        ExecutorState::Binding,
    )
    .await;
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.bind_executor_completed(executor.id.clone(), None, None),
        ExecutorState::Bound,
    )
    .await;
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.unbind_executor(executor.id.clone()),
        ExecutorState::Unbinding,
    )
    .await;
    controller
        .interrupt_executor(executor.id.clone())
        .await
        .unwrap();
    assert_eq!(lock_ptr!(shared).unwrap().signal, Some(Signal::Interrupt));
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.unbind_executor_completed(executor.id.clone()),
        ExecutorState::Idle,
    )
    .await;
    assert_eq!(lock_ptr!(shared).unwrap().signal, None);
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.release_executor(executor.id.clone()),
        ExecutorState::Releasing,
    )
    .await;
    assert_transition_is_staged(
        directory.path(),
        &shared,
        || controller.unregister_executor(executor.id.clone()),
        ExecutorState::Released,
    )
    .await;
    assert!(matches!(
        controller.get_executor(executor.id),
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn assignment_is_visible_only_after_binding_is_persisted() {
    use tokio::time::{timeout, Duration};

    let (directory, controller, executor) = persistent_lifecycle_executor().await;
    let session_id = "persist-before-assignment".to_string();
    controller.register_executor(&executor).await.unwrap();
    let parent = directory
        .path()
        .join("nodes/persist-binding-node/executors");
    let saved = parent.with_extension("saved");
    std::fs::rename(&parent, &saved).unwrap();
    std::fs::write(&parent, b"block binding persistence").unwrap();
    let failed = controller
        .bind_session(executor.id.clone(), session_id.clone())
        .await;
    std::fs::remove_file(&parent).unwrap();
    std::fs::rename(&saved, &parent).unwrap();
    assert!(matches!(failed, Err(FlameError::Storage(_))));
    let current = controller.get_executor(executor.id.clone()).unwrap();
    assert_eq!(current.state, ExecutorState::Idle);
    assert_eq!(current.ssn_id, None);
    let waiting = controller.wait_for_session(executor.id.clone());
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());
    controller
        .bind_session(executor.id.clone(), session_id.clone())
        .await
        .unwrap();
    let assigned = timeout(Duration::from_secs(1), waiting)
        .await
        .expect("persisted assignment should wake the executor")
        .unwrap()
        .unwrap();
    assert_eq!(assigned.id, session_id);
    controller
        .bind_executor_completed(executor.id.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(
        controller.get_executor(executor.id).unwrap().state,
        ExecutorState::Bound
    );
}

#[tokio::test]
async fn returns_none_when_assigned_session_is_closed() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    let ssn_id = format!("closed-session-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    controller.close_session(ssn_id.clone()).await.unwrap();

    let session = controller
        .wait_for_session(executor_id.clone())
        .await
        .unwrap();
    assert!(session.is_none());

    let executor = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(executor.state, ExecutorState::Binding);
    assert_eq!(executor.ssn_id, Some(ssn_id));

    controller
        .unregister_executor(executor_id.clone())
        .await
        .unwrap();

    assert!(matches!(
        controller.get_executor(executor_id).unwrap_err(),
        FlameError::NotFound(_)
    ));
}

#[tokio::test]
async fn returns_none_when_assigned_session_is_missing() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    let ssn_id = format!("missing-session-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    controller.close_session(ssn_id.clone()).await.unwrap();
    controller.delete_session(ssn_id.clone()).await.unwrap();

    let session = controller
        .wait_for_session(executor_id.clone())
        .await
        .unwrap();
    assert!(session.is_none());

    let executor = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(executor.state, ExecutorState::Binding);
    assert_eq!(executor.ssn_id, Some(ssn_id));

    controller
        .unregister_executor(executor_id.clone())
        .await
        .unwrap();

    assert!(matches!(
        controller.get_executor(executor_id).unwrap_err(),
        FlameError::NotFound(_)
    ));
}

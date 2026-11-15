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

async fn fail_binding(controller: &ControllerPtr, executor_id: &str) -> Result<(), FlameError> {
    controller
        .bind_executor_completed(
            executor_id.to_string(),
            Some(FlameResult {
                return_code: -1,
                message: Some("on_session_enter failed: boom".to_string()),
            }),
            None,
        )
        .await
}

#[tokio::test]
async fn records_failure_event_and_moves_executor_to_unbinding() {
    let storage = create_test_storage_with_retry_limits(2).await;
    let controller = new_ptr(storage.clone());
    let ssn_id = format!("bind-failed-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    fail_binding(&controller, &executor_id).await.unwrap();

    let executor = controller.get_executor(executor_id).unwrap();
    assert_eq!(executor.state, ExecutorState::Unbinding);
    assert_eq!(executor.ssn_id, None);
    assert_eq!(executor.task_id, None);

    let session = controller.get_session(ssn_id).unwrap();
    assert_eq!(session.retry_count, 1);
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.code == SESSION_BIND_FAILED)
            .count(),
        1
    );
    assert!(session.events.iter().any(|event| event
        .message
        .as_deref()
        .is_some_and(|message| message.contains("on_session_enter failed: boom"))));
}

#[tokio::test]
async fn failed_bind_after_session_close_moves_executor_to_unbinding_without_events() {
    let storage = create_test_storage_with_retry_limits(1).await;
    let controller = new_ptr(storage);
    let ssn_id = format!("closed-bind-failed-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;
    controller
        .record_event(
            EventOwner::session(ssn_id.clone()),
            Event {
                code: SESSION_BIND_FAILED,
                message: Some("earlier binding failure".to_string()),
                creation_time: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        controller.get_session(ssn_id.clone()).unwrap().events.len(),
        1
    );

    controller.close_session(ssn_id.clone()).await.unwrap();
    assert_eq!(
        controller.get_executor(executor_id.clone()).unwrap().state,
        ExecutorState::Binding
    );

    // The worker can finish its failed bind after the session has closed.
    // Both failure and retry-limit events must be ignored without
    // preventing the executor from completing its unbinding handshake.
    fail_binding(&controller, &executor_id).await.unwrap();

    let executor = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(executor.state, ExecutorState::Unbinding);
    assert_eq!(executor.ssn_id, None);
    assert_eq!(executor.task_id, None);
    let session = controller.get_session(ssn_id.clone()).unwrap();
    assert_eq!(session.status.state, SessionState::Closed);
    assert!(session.events.is_empty());

    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    assert_eq!(
        controller.get_executor(executor_id).unwrap().state,
        ExecutorState::Idle
    );
    assert!(controller.get_session(ssn_id).unwrap().events.is_empty());
}

#[tokio::test]
async fn records_retry_limit_once_and_allows_count_to_exceed_limit() {
    let storage = create_test_storage_with_retry_limits(2).await;
    let controller = new_ptr(storage.clone());
    let ssn_id = format!("retry-limit-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    fail_binding(&controller, &executor_id).await.unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    controller
        .bind_session(executor_id.clone(), ssn_id.clone())
        .await
        .unwrap();

    fail_binding(&controller, &executor_id).await.unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    controller
        .bind_session(executor_id.clone(), ssn_id.clone())
        .await
        .unwrap();

    fail_binding(&controller, &executor_id).await.unwrap();

    let session = controller.get_session(ssn_id).unwrap();
    assert_eq!(session.retry_count, 3);
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.code == SESSION_BIND_FAILED)
            .count(),
        3
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.code == SESSION_RETRY_LIMIT_REACHED)
            .count(),
        1
    );
}

#[tokio::test]
async fn bind_success_keeps_retry_count() {
    let storage = create_test_storage_with_retry_limits(2).await;
    let controller = new_ptr(storage.clone());
    let ssn_id = format!("retry-keep-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    fail_binding(&controller, &executor_id).await.unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    controller
        .bind_session(executor_id.clone(), ssn_id.clone())
        .await
        .unwrap();

    controller
        .bind_executor_completed(
            executor_id.clone(),
            Some(FlameResult {
                return_code: 0,
                message: None,
            }),
            None,
        )
        .await
        .unwrap();

    let session = controller.get_session(ssn_id).unwrap();
    assert_eq!(session.retry_count, 1);
    assert_eq!(
        controller.get_executor(executor_id).unwrap().state,
        ExecutorState::Bound
    );
}

#[tokio::test]
async fn bind_success_requires_attached_session() {
    let storage = create_test_storage_with_retry_limits(2).await;
    let controller = new_ptr(storage.clone());
    let ssn_id = format!("missing-session-{}", Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    {
        let executor = storage.get_executor_ptr(executor_id.clone()).unwrap();
        let mut executor = lock_ptr!(executor).unwrap();
        executor.ssn_id = None;
    }

    let err = controller
        .bind_executor_completed(
            executor_id.clone(),
            Some(FlameResult {
                return_code: 0,
                message: None,
            }),
            None,
        )
        .await
        .unwrap_err();

    assert!(matches!(err, FlameError::InvalidState(_)));
    assert_eq!(
        controller.get_executor(executor_id).unwrap().state,
        ExecutorState::Binding
    );
}

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
async fn test_register_executor_transitions_to_idle() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let storage = create_mock_storage().await;

    let state = VoidState {
        storage,
        executor: exe_ptr.clone(),
    };

    let result = state.register_executor().await;

    assert!(result.is_ok());
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Idle);
}

#[tokio::test]
async fn test_release_executor_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let result = state.release_executor().await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Void);
}

#[tokio::test]
async fn test_unregister_executor_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let result = state.unregister_executor().await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

#[tokio::test]
async fn test_bind_session_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let ssn_ptr = new_ptr(common::apis::Session::default());
    let result = state.bind_session(ssn_ptr).await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

#[tokio::test]
async fn test_bind_session_completed_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let result = state.bind_session_completed(None).await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

#[tokio::test]
async fn test_unbind_executor_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let result = state.unbind_executor().await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

#[tokio::test]
async fn test_unbind_executor_completed_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let result = state.unbind_executor_completed().await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

#[tokio::test]
async fn test_launch_task_fails() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let state = VoidState {
        storage: create_mock_storage().await,
        executor: exe_ptr.clone(),
    };

    let ssn_ptr = new_ptr(common::apis::Session::default());
    let task_ptr = new_ptr(common::apis::Task::default());
    let result = state.launch_task(ssn_ptr, task_ptr).await;

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

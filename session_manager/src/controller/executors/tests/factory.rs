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
async fn test_from_creates_void_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_from_creates_idle_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_from_creates_binding_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_from_creates_bound_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_from_creates_unbinding_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_from_creates_releasing_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_from_returns_error_for_unknown_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Unknown);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

#[tokio::test]
async fn test_from_returns_error_for_released_state() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Released);
    let storage = create_mock_storage().await;

    let result = from(storage, exe_ptr);

    assert!(result.is_err());
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
}

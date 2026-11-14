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
async fn test_full_bind_unbind_lifecycle() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let storage = create_mock_storage().await;

    let void_state = VoidState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    void_state.register_executor().await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Idle);

    let idle_state = IdleState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    let (_, ssn_ptr) = create_stored_test_session(&storage).await;
    idle_state.bind_session(ssn_ptr).await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Binding);

    let binding_state = BindingState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    binding_state.bind_session_completed(None).await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Bound);

    let bound_state = BoundState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    bound_state.unbind_executor().await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Unbinding);

    let unbinding_state = UnbindingState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    unbinding_state.unbind_executor_completed().await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Idle);
}

#[tokio::test]
async fn test_full_release_lifecycle() {
    let exe_ptr = create_test_executor("exe-1", ExecutorState::Void);
    let storage = create_mock_storage().await;

    let void_state = VoidState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    void_state.register_executor().await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Idle);

    let idle_state = IdleState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    idle_state.release_executor().await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Releasing);

    let releasing_state = ReleasingState {
        storage: storage.clone(),
        executor: exe_ptr.clone(),
    };
    releasing_state.unregister_executor().await.unwrap();
    assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Released);
}

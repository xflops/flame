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

use std::sync::Arc;

use crate::controller::executors::{
    binding::BindingState, bound::BoundState, idle::IdleState, releasing::ReleasingState,
    unbinding::UnbindingState, void::VoidState,
};
use crate::storage::StoragePtr;

use crate::model::ExecutorPtr;
use common::apis::{ExecutorID, ExecutorState, SessionPtr, Task, TaskOutput, TaskPtr, TaskResult};
use common::FlameError;
use stdng::{lock_ptr, new_ptr, MutexPtr};

mod binding;
mod bound;
mod idle;
mod releasing;
mod unbinding;
mod void;

pub fn from(storage: StoragePtr, exe_ptr: ExecutorPtr) -> Result<Arc<dyn States>, FlameError> {
    let exe = lock_ptr!(exe_ptr)?;
    tracing::debug!("Build state <{}> for Executor <{}>.", exe.state, exe.id);

    match exe.state {
        ExecutorState::Void => Ok(Arc::new(VoidState {
            storage,
            executor: exe_ptr.clone(),
        })),
        ExecutorState::Idle => Ok(Arc::new(IdleState {
            storage,
            executor: exe_ptr.clone(),
        })),
        ExecutorState::Binding => Ok(Arc::new(BindingState {
            storage,
            executor: exe_ptr.clone(),
        })),
        ExecutorState::Bound => Ok(Arc::new(BoundState {
            storage,
            executor: exe_ptr.clone(),
        })),
        ExecutorState::Unbinding => Ok(Arc::new(UnbindingState {
            storage,
            executor: exe_ptr.clone(),
        })),
        ExecutorState::Releasing => Ok(Arc::new(ReleasingState {
            storage,
            executor: exe_ptr.clone(),
        })),
        _ => Err(FlameError::InvalidState("Executor is unknown".to_string())),
    }
}

#[async_trait::async_trait]
pub trait States: Send + Sync + 'static {
    async fn register_executor(&self) -> Result<(), FlameError>;
    async fn release_executor(&self) -> Result<(), FlameError>;
    async fn unregister_executor(&self) -> Result<(), FlameError>;

    async fn bind_session(&self, ssn: SessionPtr) -> Result<(), FlameError>;
    async fn bind_session_completed(&self) -> Result<(), FlameError>;

    async fn unbind_executor(&self) -> Result<(), FlameError>;
    async fn unbind_executor_completed(&self) -> Result<(), FlameError>;

    async fn launch_task(&self, ssn: SessionPtr) -> Result<Option<Task>, FlameError>;
    async fn complete_task(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Executor;
    use chrono::Utc;
    use common::apis::{ResourceRequirement, Shim};
    use common::ctx::{FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits};

    fn create_test_executor(id: &str, state: ExecutorState) -> ExecutorPtr {
        new_ptr(Executor {
            id: id.to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement::default(),
            slots: 1,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state,
        })
    }

    fn get_state(exe_ptr: &ExecutorPtr) -> Result<ExecutorState, FlameError> {
        let exe = lock_ptr!(exe_ptr)?;
        Ok(exe.state)
    }

    async fn create_mock_storage() -> StoragePtr {
        let unique_id = format!(
            "{}_{:?}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            std::thread::current().id()
        );
        let test_dir = std::env::temp_dir().join(format!("flame_exec_test_{}", unique_id));
        std::fs::create_dir_all(&test_dir).unwrap();
        let db_path = test_dir.join("flame.db");
        let url = format!("sqlite://{}", db_path.display());

        let ctx = FlameClusterContext {
            cluster: FlameCluster {
                name: "test".to_string(),
                endpoint: "http://localhost:8080".to_string(),
                storage: url,
                slot: ResourceRequirement::default(),
                policy: "fifo".to_string(),
                schedule_interval: 1000,
                executors: FlameExecutors {
                    shim: Shim::default(),
                },
                tls: None,
                limits: FlameLimits {
                    max_sessions: None,
                    max_executors: 10,
                },
            },
            cache: None,
        };

        crate::storage::new_ptr(&ctx).await.unwrap()
    }

    mod void_state_tests {
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

            let result = state.bind_session_completed().await;

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
            let result = state.launch_task(ssn_ptr).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }
    }

    mod idle_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_register_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let state = IdleState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.register_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Idle);
        }

        #[tokio::test]
        async fn test_bind_session_transitions_to_binding() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let storage = create_mock_storage().await;

            let state = IdleState {
                storage,
                executor: exe_ptr.clone(),
            };

            let ssn = common::apis::Session {
                id: "ssn-1".to_string(),
                ..Default::default()
            };
            let ssn_ptr = new_ptr(ssn);

            let result = state.bind_session(ssn_ptr).await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Binding);

            let exe = lock_ptr!(exe_ptr).unwrap();
            assert_eq!(exe.ssn_id, Some("ssn-1".to_string()));
        }

        #[tokio::test]
        async fn test_release_executor_transitions_to_releasing() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let storage = create_mock_storage().await;

            let state = IdleState {
                storage,
                executor: exe_ptr.clone(),
            };

            let result = state.release_executor().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Releasing);
        }

        #[tokio::test]
        async fn test_unregister_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let state = IdleState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unregister_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_bind_session_completed_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let state = IdleState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.bind_session_completed().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unbind_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let state = IdleState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_launch_task_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Idle);
            let state = IdleState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let ssn_ptr = new_ptr(common::apis::Session::default());
            let result = state.launch_task(ssn_ptr).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }
    }

    mod binding_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_bind_session_completed_transitions_to_bound() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let storage = create_mock_storage().await;

            let state = BindingState {
                storage,
                executor: exe_ptr.clone(),
            };

            let result = state.bind_session_completed().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Bound);
        }

        #[tokio::test]
        async fn test_bind_session_is_idempotent() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let storage = create_mock_storage().await;

            let state = BindingState {
                storage,
                executor: exe_ptr.clone(),
            };

            let ssn = common::apis::Session {
                id: "ssn-2".to_string(),
                ..Default::default()
            };
            let ssn_ptr = new_ptr(ssn);

            let result = state.bind_session(ssn_ptr).await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Binding);
        }

        #[tokio::test]
        async fn test_register_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let state = BindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.register_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_release_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let state = BindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.release_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unregister_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let state = BindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unregister_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unbind_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let state = BindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_launch_task_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Binding);
            let state = BindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let ssn_ptr = new_ptr(common::apis::Session::default());
            let result = state.launch_task(ssn_ptr).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }
    }

    mod bound_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_unbind_executor_transitions_to_unbinding() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let storage = create_mock_storage().await;

            let state = BoundState {
                storage,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Unbinding);
        }

        #[tokio::test]
        async fn test_register_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let state = BoundState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.register_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_release_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let state = BoundState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.release_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unregister_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let state = BoundState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unregister_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_bind_session_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let state = BoundState {
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
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let state = BoundState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.bind_session_completed().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unbind_executor_completed_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Bound);
            let state = BoundState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor_completed().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }
    }

    mod unbinding_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_unbind_executor_completed_transitions_to_idle() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let storage = create_mock_storage().await;

            {
                let mut exe = lock_ptr!(exe_ptr).unwrap();
                exe.ssn_id = Some("ssn-1".to_string());
                exe.task_id = Some(1);
            }

            let state = UnbindingState {
                storage,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor_completed().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Idle);

            let exe = lock_ptr!(exe_ptr).unwrap();
            assert!(exe.ssn_id.is_none());
            assert!(exe.task_id.is_none());
        }

        #[tokio::test]
        async fn test_unbind_executor_is_idempotent() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let storage = create_mock_storage().await;

            let state = UnbindingState {
                storage,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Unbinding);
        }

        #[tokio::test]
        async fn test_register_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let state = UnbindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.register_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_release_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let state = UnbindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.release_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unregister_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let state = UnbindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unregister_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_bind_session_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let state = UnbindingState {
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
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let state = UnbindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.bind_session_completed().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_launch_task_returns_none() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Unbinding);
            let state = UnbindingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let ssn_ptr = new_ptr(common::apis::Session::default());
            let result = state.launch_task(ssn_ptr).await;

            assert!(result.is_ok());
            assert!(result.unwrap().is_none());
        }
    }

    mod releasing_state_tests {
        use super::*;

        #[tokio::test]
        async fn test_unregister_executor_transitions_to_released() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let storage = create_mock_storage().await;

            let state = ReleasingState {
                storage,
                executor: exe_ptr.clone(),
            };

            let result = state.unregister_executor().await;

            assert!(result.is_ok());
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Released);
        }

        #[tokio::test]
        async fn test_register_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.register_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_release_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.release_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_bind_session_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
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
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.bind_session_completed().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unbind_executor_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_unbind_executor_completed_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let result = state.unbind_executor_completed().await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_launch_task_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let ssn_ptr = new_ptr(common::apis::Session::default());
            let result = state.launch_task(ssn_ptr).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }

        #[tokio::test]
        async fn test_complete_task_fails() {
            let exe_ptr = create_test_executor("exe-1", ExecutorState::Releasing);
            let state = ReleasingState {
                storage: create_mock_storage().await,
                executor: exe_ptr.clone(),
            };

            let ssn_ptr = new_ptr(common::apis::Session::default());
            let task_ptr = new_ptr(common::apis::Task::default());
            let task_result = TaskResult::default();

            let result = state.complete_task(ssn_ptr, task_ptr, task_result).await;

            assert!(result.is_err());
            assert!(matches!(result, Err(FlameError::InvalidState(_))));
        }
    }

    mod factory_tests {
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
    }

    mod lifecycle_tests {
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
            let ssn = common::apis::Session {
                id: "ssn-1".to_string(),
                ..Default::default()
            };
            let ssn_ptr = new_ptr(ssn);
            idle_state.bind_session(ssn_ptr.clone()).await.unwrap();
            assert_eq!(get_state(&exe_ptr).unwrap(), ExecutorState::Binding);

            let binding_state = BindingState {
                storage: storage.clone(),
                executor: exe_ptr.clone(),
            };
            binding_state.bind_session_completed().await.unwrap();
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
    }
}

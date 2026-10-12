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

use async_trait::async_trait;
use stdng::{lock_ptr, logs::TraceFn, trace_fn};

use crate::client::BackendClient;
use crate::executor::{Executor, ExecutorPtr};

use common::apis::{Event, EventOwner, ExecutorState};
use common::FlameError;

mod binding;
mod bound;
mod idle;
mod releasing;
mod unbinding;
mod unknown;
mod void;

pub fn from(client: BackendClient, shared: ExecutorPtr) -> Result<Box<dyn State>, FlameError> {
    let e = lock_ptr!(shared)?.clone();
    tracing::debug!("Build state <{}> for Executor <{}>.", e.state, e.id);

    let state: Box<dyn State> = match e.state {
        ExecutorState::Void => Box::new(void::VoidState {
            client,
            executor: e,
        }),
        ExecutorState::Idle => Box::new(idle::IdleState {
            client,
            executor: e,
        }),
        ExecutorState::Binding => Box::new(binding::BindingState {
            client,
            executor: e,
        }),
        ExecutorState::Bound => Box::new(bound::BoundState {
            client,
            executor: e,
        }),
        ExecutorState::Unbinding => Box::new(unbinding::UnbindingState {
            client,
            executor: e,
        }),
        ExecutorState::Releasing => Box::new(releasing::ReleasingState {
            client,
            executor: e,
        }),
        _ => Box::new(unknown::UnknownState { executor: e }),
    };
    Ok(state)
}

#[async_trait]
pub trait State: Send + Sync {
    async fn execute(&mut self) -> Result<Executor, FlameError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;
    use common::apis::{ResourceRequirement, Shim};

    fn create_test_executor(id: &str, state: ExecutorState) -> Executor {
        Executor {
            id: id.to_string(),
            application: "test-app".to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement::default(),
            assignments: Default::default(),
            shim: Shim::Host,
            session: None,
            task: None,
            context: None,
            app_mgr: None,
            dev_mgr: None,
            shim_instance: None,
            state,
        }
    }

    #[tokio::test]
    async fn missing_service_reports_error_without_changing_executor() {
        for state in [ExecutorState::Bound, ExecutorState::Unbinding] {
            let executor = create_test_executor("missing-service", state);
            let shared = stdng::new_ptr(executor);
            let mut handler = from(BackendClient::default(), shared.clone()).unwrap();
            let error = handler
                .execute()
                .await
                .err()
                .expect("missing service error");
            assert!(error.to_string().contains("no shim instance"));
            let executor = lock_ptr!(shared).unwrap();
            assert_eq!(executor.state, state);
            assert!(executor.shim_instance.is_none());
            assert!(executor.task.is_none());
        }
    }

    mod factory_tests {
        use super::*;

        #[tokio::test]
        async fn test_from_void_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Void);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }

        #[tokio::test]
        async fn test_from_idle_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Idle);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }

        #[tokio::test]
        async fn test_from_binding_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Binding);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }

        #[tokio::test]
        async fn test_from_bound_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Bound);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }

        #[tokio::test]
        async fn test_from_unbinding_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Unbinding);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }

        #[tokio::test]
        async fn test_from_releasing_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Releasing);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }

        #[tokio::test]
        async fn test_from_unknown_state() {
            let exe = create_test_executor("exe-1", ExecutorState::Unknown);
            let client = BackendClient::default();
            let shared = stdng::new_ptr(exe.clone());
            let _state = from(client, shared).unwrap();
        }
    }
}

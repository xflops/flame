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

use async_trait::async_trait;
use stdng::{logs::TraceFn, trace_fn};

use crate::client::BackendClient;
use crate::executor::Executor;
use crate::states::State;
use common::apis::ExecutorState;
use common::FlameError;

#[derive(Clone)]
pub struct ReleasingState {
    pub client: BackendClient,
    pub executor: Executor,
}

#[async_trait]
impl State for ReleasingState {
    async fn execute(&mut self) -> Result<Executor, FlameError> {
        trace_fn!("ReleasingState::execute");

        self.client
            .unregister_executor(&self.executor.clone())
            .await?;

        self.release_completed();

        Ok(self.executor.clone())
    }
}

impl ReleasingState {
    fn release_completed(&mut self) {
        // The executor and its retained service instance share one lifetime.
        // Drop the last executor-owned shim reference only after Session
        // Manager has accepted the unregister operation.
        self.executor.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shims::{SessionEnterResponse, Shim as ShimService, ShimPtr, TaskInvokeResponse};
    use common::apis::{ResourceRequirement, SessionContext, Shim, TaskContext};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct TestShim;

    #[async_trait]
    impl ShimService for TestShim {
        async fn on_session_enter(
            &mut self,
            _ctx: &SessionContext,
        ) -> Result<SessionEnterResponse, FlameError> {
            unreachable!()
        }

        async fn on_task_invoke(
            &mut self,
            _ctx: &TaskContext,
        ) -> Result<TaskInvokeResponse, FlameError> {
            unreachable!()
        }

        async fn on_session_leave(&mut self) -> Result<(), FlameError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn release_clears_retained_instance() {
        let shim: ShimPtr = Arc::new(Mutex::new(TestShim));
        let executor = Executor {
            id: "executor-1".to_string(),
            application: "test-app".to_string(),
            resreq: ResourceRequirement::default(),
            node: "node-1".to_string(),
            shim: Shim::Host,
            session: None,
            task: None,
            context: None,
            shim_instance: Some(shim),
            state: ExecutorState::Releasing,
        };
        let mut state = ReleasingState {
            client: BackendClient::default(),
            executor,
        };

        state.release_completed();

        assert_eq!(state.executor.state, ExecutorState::Released);
        assert!(state.executor.shim_instance.is_none());
    }
}

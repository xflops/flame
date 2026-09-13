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
use stdng::{logs::TraceFn, trace_fn};

use crate::client::BackendClient;
use crate::executor::Executor;
use crate::states::State;
use common::apis::ExecutorState;
use common::FlameError;

#[derive(Clone)]
pub struct UnbindingState {
    pub client: BackendClient,
    pub executor: Executor,
}

#[async_trait]
impl State for UnbindingState {
    async fn execute(&mut self) -> Result<Executor, FlameError> {
        trace_fn!("UnbindingState::execute");

        self.client.unbind_executor(&self.executor.clone()).await?;
        if let Some(shim_ptr) = self.executor.shim_instance.clone() {
            let mut shim = shim_ptr.lock().await;
            shim.on_session_leave().await?;
        } else {
            tracing::debug!(
                "Executor <{}> has no shim instance during unbinding; skip on_session_leave",
                self.executor.id
            );
        }

        self.client
            .unbind_executor_completed(&self.executor.clone())
            .await?;

        self.unbind_completed();

        Ok(self.executor.clone())
    }
}

impl UnbindingState {
    fn unbind_completed(&mut self) {
        self.executor.task = None;
        self.executor.session = None;

        // After unbound from session, the executor is idle now.
        self.executor.state = ExecutorState::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shims::{SessionEnterResponse, Shim, ShimPtr, TaskInvokeResponse};
    use common::apis::{ResourceRequirement, SessionContext, Shim as ShimType, TaskContext};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct TestShim;

    #[async_trait]
    impl Shim for TestShim {
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
            Ok(())
        }
    }

    #[tokio::test]
    async fn successful_unbind_retains_instance() {
        let shim: ShimPtr = Arc::new(Mutex::new(TestShim));
        let executor = Executor {
            id: "executor-1".to_string(),
            application: "test-app".to_string(),
            resreq: ResourceRequirement::default(),
            node: "node-1".to_string(),
            shim: ShimType::Host,
            session: None,
            task: None,
            context: None,
            shim_instance: Some(shim.clone()),
            state: ExecutorState::Unbinding,
        };
        let mut state = UnbindingState {
            client: BackendClient::default(),
            executor,
        };

        state.unbind_completed();

        assert_eq!(state.executor.state, ExecutorState::Idle);
        assert!(Arc::ptr_eq(
            state.executor.shim_instance.as_ref().unwrap(),
            &shim
        ));
    }
}

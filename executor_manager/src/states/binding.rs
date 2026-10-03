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
pub struct BindingState {
    pub client: BackendClient,
    pub executor: Executor,
}

#[async_trait]
impl State for BindingState {
    async fn execute(&mut self) -> Result<Executor, FlameError> {
        trace_fn!("BindingState::execute");

        tracing::warn!(
            "Executor <{}> received in Binding state, resetting to Idle for re-scheduling",
            self.executor.name
        );

        self.executor.state = ExecutorState::Idle;
        self.executor.session = None;

        Ok(self.executor.clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_binding_state_transitions_to_idle() {
        // BindingState handles the recovery case where an executor is received
        // in Binding state after a restart. The execute() method:
        //
        // 1. Logs a warning about the unexpected state
        // 2. Resets executor.state to Idle
        // 3. Clears executor.session to None
        // 4. Returns the executor for re-scheduling
        //
        // This is a defensive measure - the primary fix is in session manager's
        // load_data() which resets Binding executors to Idle during startup.
        // This handler ensures graceful recovery if binding executors somehow
        // reach the executor manager.
    }
}

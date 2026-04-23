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

use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use crate::controller::executors::States;
use crate::model::ExecutorPtr;
use crate::storage::StoragePtr;
use common::apis::{ExecutorState, SessionPtr, Task, TaskPtr, TaskResult};
use common::FlameError;

pub struct BindingState {
    pub storage: StoragePtr,
    pub executor: ExecutorPtr,
}

#[async_trait::async_trait]
impl States for BindingState {
    async fn register_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::register_executor");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }

    async fn release_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::release_executor");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }

    async fn unregister_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::unregister_executor");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }

    async fn bind_session(&self, ssn_ptr: SessionPtr) -> Result<(), FlameError> {
        trace_fn!("BindingState::bind_session");

        let ssn_id = {
            let ssn = lock_ptr!(ssn_ptr)?;
            ssn.id.clone()
        };

        let mut e = lock_ptr!(self.executor)?;
        e.ssn_id = Some(ssn_id);
        e.state = ExecutorState::Binding;

        Ok(())
    }

    async fn bind_session_completed(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::bind_session");

        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Bound;

        Ok(())
    }

    async fn unbind_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::unbind_executor");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }

    async fn unbind_executor_completed(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::unbind_executor_completed");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }

    async fn launch_task(
        &self,
        _ssn: SessionPtr,
        _task: TaskPtr,
    ) -> Result<Option<Task>, FlameError> {
        trace_fn!("BindingState::launch_task");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }

    async fn complete_task(
        &self,
        _ssn: SessionPtr,
        _task: TaskPtr,
        _: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("BindingState::complete_task");

        Err(FlameError::InvalidState("Executor is binding".to_string()))
    }
}

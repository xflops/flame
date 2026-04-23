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

use crate::controller::executors::States;
use crate::storage::StoragePtr;
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use crate::model::ExecutorPtr;
use common::apis::{ExecutorState, SessionPtr, Task, TaskPtr, TaskResult, TaskState};
use common::FlameError;

pub struct UnbindingState {
    pub storage: StoragePtr,
    pub executor: ExecutorPtr,
}

#[async_trait::async_trait]
impl States for UnbindingState {
    async fn register_executor(&self) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::register_executor");

        Err(FlameError::InvalidState(
            "Executor is unbinding".to_string(),
        ))
    }

    async fn release_executor(&self) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::release_executor");

        Err(FlameError::InvalidState(
            "Executor is unbinding".to_string(),
        ))
    }

    async fn unregister_executor(&self) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::unregister_executor");

        Err(FlameError::InvalidState(
            "Executor is unbinding".to_string(),
        ))
    }

    async fn bind_session(&self, _ssn_ptr: SessionPtr) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::bind_session");

        Err(FlameError::InvalidState(
            "Executor is unbinding".to_string(),
        ))
    }

    async fn bind_session_completed(&self) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::bind_session_completed");

        Err(FlameError::InvalidState(
            "Executor is unbinding".to_string(),
        ))
    }

    async fn unbind_executor(&self) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::unbind_session");

        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Unbinding;

        Ok(())
    }

    async fn unbind_executor_completed(&self) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::unbind_session_completed");

        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Idle;
        e.ssn_id = None;
        e.task_id = None;

        Ok(())
    }

    async fn launch_task(
        &self,
        _ssn: SessionPtr,
        _task: TaskPtr,
    ) -> Result<Option<Task>, FlameError> {
        trace_fn!("UnbindingState::launch_task");

        Ok(None)
    }

    async fn complete_task(
        &self,
        ssn_ptr: SessionPtr,
        task_ptr: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("UnbindingState::complete_task");

        self.storage
            .update_task_result(ssn_ptr, task_ptr, task_result)
            .await?;

        {
            let mut e = lock_ptr!(self.executor)?;
            e.task_id = None;
            e.ssn_id = None;
        };

        Ok(())
    }
}

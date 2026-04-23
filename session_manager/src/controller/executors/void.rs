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
use common::apis::{ExecutorState, SessionPtr, Task, TaskPtr, TaskResult};
use common::FlameError;

pub struct VoidState {
    pub storage: StoragePtr,
    pub executor: ExecutorPtr,
}

#[async_trait::async_trait]
impl States for VoidState {
    async fn register_executor(&self) -> Result<(), FlameError> {
        trace_fn!("VoidState::register_executor");
        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Idle;

        Ok(())
    }

    async fn release_executor(&self) -> Result<(), FlameError> {
        trace_fn!("VoidState::release_executor");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn unregister_executor(&self) -> Result<(), FlameError> {
        trace_fn!("VoidState::unregister_executor");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn bind_session(&self, _ssn_ptr: SessionPtr) -> Result<(), FlameError> {
        trace_fn!("VoidState::bind_session");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn bind_session_completed(&self) -> Result<(), FlameError> {
        trace_fn!("VoidState::bind_session_completed");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn unbind_executor(&self) -> Result<(), FlameError> {
        trace_fn!("VoidState::unbind_executor");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn unbind_executor_completed(&self) -> Result<(), FlameError> {
        trace_fn!("VoidState::unbind_executor_completed");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn launch_task(
        &self,
        _ssn: SessionPtr,
        _task: TaskPtr,
    ) -> Result<Option<Task>, FlameError> {
        trace_fn!("VoidState::launch_task");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }

    async fn complete_task(
        &self,
        ssn_ptr: SessionPtr,
        task_ptr: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("VoidState::complete_task");

        Err(FlameError::InvalidState("Executor is void".to_string()))
    }
}

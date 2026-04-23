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

use stdng::{lock_ptr, logs::TraceFn, trace_fn};

use crate::model::ExecutorPtr;
use common::apis::{ExecutorState, SessionPtr, Task, TaskPtr, TaskResult, TaskState};
use common::FlameError;

use crate::controller::executors::States;
use crate::storage::StoragePtr;

pub struct BoundState {
    pub storage: StoragePtr,
    pub executor: ExecutorPtr,
}

#[async_trait::async_trait]
impl States for BoundState {
    async fn register_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BoundState::register_executor");

        Err(FlameError::InvalidState("Executor is bound".to_string()))
    }

    async fn release_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BoundState::release_executor");

        Err(FlameError::InvalidState("Executor is bound".to_string()))
    }

    async fn unregister_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BoundState::unregister_executor");

        Err(FlameError::InvalidState("Executor is bound".to_string()))
    }

    async fn bind_session(&self, _ssn_ptr: SessionPtr) -> Result<(), FlameError> {
        trace_fn!("BoundState::bind_session");

        Err(FlameError::InvalidState("Executor is bound".to_string()))
    }

    async fn bind_session_completed(&self) -> Result<(), FlameError> {
        trace_fn!("BoundState::bind_session_completed");

        Err(FlameError::InvalidState("Executor is bound".to_string()))
    }

    async fn unbind_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BoundState::unbind_session");

        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Unbinding;

        Ok(())
    }

    async fn unbind_executor_completed(&self) -> Result<(), FlameError> {
        trace_fn!("BoundState::unbind_executor_completed");

        Err(FlameError::InvalidState("Executor is bound".to_string()))
    }

    async fn launch_task(
        &self,
        ssn_ptr: SessionPtr,
        task_ptr: TaskPtr,
    ) -> Result<Option<Task>, FlameError> {
        trace_fn!("BoundState::launch_task");

        let (ssn_id, task_id) = {
            let task = lock_ptr!(task_ptr)?;
            (task.ssn_id.clone(), task.id)
        };

        let host = {
            let e = lock_ptr!(self.executor)?;
            e.node.clone()
        };

        tracing::debug!("Launching task <{}/{}> on host <{}>", ssn_id, task_id, host);

        let msg = format!("Running task on host <{}>.", host);
        self.storage
            .update_task_state(ssn_ptr, task_ptr.clone(), TaskState::Running, Some(msg))
            .await?;

        {
            let mut e = lock_ptr!(self.executor)?;
            e.task_id = Some(task_id);
            e.ssn_id = Some(ssn_id);
        };

        let task = lock_ptr!(task_ptr)?;
        Ok(Some((*task).clone()))
    }

    async fn complete_task(
        &self,
        ssn_ptr: SessionPtr,
        task_ptr: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("BoundState::complete_task");

        self.storage
            .update_task_result(ssn_ptr, task_ptr, task_result)
            .await?;

        {
            let mut e = lock_ptr!(self.executor)?;
            e.task_id = None;
        };

        Ok(())
    }
}

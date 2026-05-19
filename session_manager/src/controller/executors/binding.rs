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

use chrono::Utc;
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use crate::controller::executors::States;
use crate::model::ExecutorPtr;
use crate::storage::StoragePtr;
use common::apis::{
    Event, EventOwner, ExecutorState, FlameResult, SessionPtr, Task, TaskPtr, TaskResult,
    BIND_RESULT_OK, SESSION_BIND_FAILED, SESSION_RETRY_LIMIT_REACHED,
};
use common::FlameError;

pub struct BindingState {
    pub storage: StoragePtr,
    pub executor: ExecutorPtr,
}

impl BindingState {
    async fn bind_session_success(&self) -> Result<(), FlameError> {
        let ssn_id = {
            let e = lock_ptr!(self.executor)?;
            e.ssn_id.clone().ok_or_else(|| {
                FlameError::InvalidState(format!(
                    "Executor <{}> has no bound session on successful bind completion",
                    e.id
                ))
            })?
        };
        self.storage.get_session_ptr(ssn_id)?;

        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Bound;

        Ok(())
    }

    async fn bind_session_failed(&self, result: &FlameResult) -> Result<(), FlameError> {
        let executor = { lock_ptr!(self.executor)?.clone() };

        self.record_bind_failure(&executor, result).await?;
        self.increment_session_retry_count(&executor).await?;

        let mut e = lock_ptr!(self.executor)?;
        e.state = ExecutorState::Unbinding;
        e.ssn_id = None;
        e.task_id = None;

        Ok(())
    }

    async fn increment_session_retry_count(
        &self,
        executor: &crate::model::Executor,
    ) -> Result<(), FlameError> {
        let ssn_id = executor.ssn_id.clone().ok_or_else(|| {
            FlameError::InvalidState(format!("Executor <{}> has no bound session", executor.id))
        })?;
        let retry_limit = self.storage.session_retry_limits();
        let ssn_ptr = self.storage.get_session_ptr(ssn_id.clone())?;
        let (retry_count, crossed_retry_limit) = {
            let mut ssn = lock_ptr!(ssn_ptr)?;
            let previous_retry_count = ssn.retry_count;
            ssn.retry_count = ssn.retry_count.saturating_add(1);
            (
                ssn.retry_count,
                previous_retry_count < retry_limit && ssn.retry_count >= retry_limit,
            )
        };

        if crossed_retry_limit {
            self.record_retry_limit_reached(&ssn_id, retry_count, retry_limit)
                .await?;
        }

        Ok(())
    }

    async fn record_bind_failure(
        &self,
        executor: &crate::model::Executor,
        result: &FlameResult,
    ) -> Result<(), FlameError> {
        let ssn_id = executor.ssn_id.clone().ok_or_else(|| {
            FlameError::InvalidState(format!("Executor <{}> has no bound session", executor.id))
        })?;
        let detail = result.message.clone().unwrap_or_default();

        self.storage
            .record_event(
                EventOwner::session(ssn_id),
                Event {
                    code: SESSION_BIND_FAILED,
                    message: Some(format!(
                        "Executor <{}> on node <{}> failed to bind session with return_code <{}>: {}",
                        executor.id,
                        executor.node,
                        result.return_code,
                        detail
                    )),
                    creation_time: Utc::now(),
                },
            )
            .await
    }

    async fn record_retry_limit_reached(
        &self,
        ssn_id: &str,
        retry_count: u32,
        retry_limit: u32,
    ) -> Result<(), FlameError> {
        self.storage
            .record_event(
                EventOwner::session(ssn_id.to_string()),
                Event {
                    code: SESSION_RETRY_LIMIT_REACHED,
                    message: Some(format!(
                        "Session retry limit reached after {}/{} failed attempts; new executor assignment is paused for this session; existing executors are unchanged.",
                        retry_count, retry_limit
                    )),
                    creation_time: Utc::now(),
                },
            )
            .await
    }
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

        let mut e = lock_ptr!(self.executor)?;
        tracing::debug!(
            "Executor <{}> unregistering from binding state, moving to released",
            e.id
        );
        e.state = ExecutorState::Released;
        e.ssn_id = None;
        e.task_id = None;

        Ok(())
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

    async fn bind_session_completed(&self, result: Option<FlameResult>) -> Result<(), FlameError> {
        trace_fn!("BindingState::bind_session_completed");

        match result.as_ref() {
            Some(result) if result.return_code != BIND_RESULT_OK => {
                self.bind_session_failed(result).await?
            }
            _ => self.bind_session_success().await?,
        }

        Ok(())
    }

    async fn unbind_executor(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::unbind_executor");

        let mut e = lock_ptr!(self.executor)?;
        tracing::debug!(
            "Executor <{}> unbinding from binding state, session=<{:?}>",
            e.id,
            e.ssn_id
        );
        e.state = ExecutorState::Unbinding;

        Ok(())
    }

    async fn unbind_executor_completed(&self) -> Result<(), FlameError> {
        trace_fn!("BindingState::unbind_executor_completed");

        let mut e = lock_ptr!(self.executor)?;
        tracing::debug!(
            "Executor <{}> unbind completed from binding state, returning to idle",
            e.id
        );
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

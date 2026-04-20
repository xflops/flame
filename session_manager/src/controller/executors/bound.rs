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

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use chrono::{DateTime, Duration, Utc};
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use crate::model::ExecutorPtr;
use common::apis::{
    ExecutorState, SessionPtr, SessionState, Task, TaskOutput, TaskPtr, TaskResult, TaskState,
};
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

    async fn launch_task(&self, ssn_ptr: SessionPtr) -> Result<Option<Task>, FlameError> {
        trace_fn!("BoundState::launch_task");

        tracing::debug!("Launching task for session");

        let app_name = {
            let ssn = lock_ptr!(ssn_ptr)?;
            tracing::debug!("Got session <{}>", ssn.id);
            ssn.application.clone()
        };

        tracing::debug!("Getting application <{}>", app_name);

        let app_ptr = self.storage.get_application(app_name.clone()).await?;

        tracing::debug!(
            "Got application <{}>, delay release: {:?}",
            app_name,
            app_ptr.delay_release
        );

        let task_ptr = WaitForTaskFuture::new(&ssn_ptr, app_ptr.delay_release).await?;
        tracing::debug!("Got task!");

        let (exec_id, host) = {
            let e = lock_ptr!(self.executor)?;
            (e.id.clone(), e.node.clone())
        };

        tracing::debug!("Got executor <{}>, host <{}>", exec_id, host);

        let task_ptr = {
            match task_ptr {
                Some(task_ptr) => {
                    let msg = format!("Running task on host <{}>.", host.clone());
                    self.storage
                        .update_task_state(
                            ssn_ptr.clone(),
                            task_ptr.clone(),
                            TaskState::Running,
                            Some(msg),
                        )
                        .await?;
                    Some(task_ptr)
                }
                None => None,
            }
        };

        // No pending task, return.
        if task_ptr.is_none() {
            tracing::debug!("No pending task, return.");
            return Ok(None);
        }

        // let task_ptr = task_ptr.unwrap();
        let (ssn_id, task_id) = {
            let task_ptr = task_ptr.clone().unwrap();
            let task = lock_ptr!(task_ptr)?;
            (task.ssn_id.clone(), task.id)
        };

        tracing::debug!(
            "Launching task <{}/{}> on host <{}> by executor {}",
            ssn_id.clone(),
            task_id.clone(),
            host.clone(),
            exec_id
        );

        {
            let mut e = lock_ptr!(self.executor)?;
            e.task_id = Some(task_id);
            e.ssn_id = Some(ssn_id);
        };

        let task_ptr = task_ptr.unwrap();
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

struct WaitForTaskFuture {
    ssn: SessionPtr,
    delay_release: Duration,
    start_time: DateTime<Utc>,
}

impl WaitForTaskFuture {
    pub fn new(ssn: &SessionPtr, delay_release: Duration) -> Self {
        Self {
            ssn: ssn.clone(),
            delay_release,
            start_time: Utc::now(),
        }
    }
}

impl Future for WaitForTaskFuture {
    type Output = Result<Option<TaskPtr>, FlameError>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut ssn = lock_ptr!(self.ssn)?;

        match ssn.pop_pending_task() {
            None => {
                let now = Utc::now();
                let duration = now.signed_duration_since(self.start_time);
                if duration.num_seconds() > self.delay_release.num_seconds()
                    || ssn.status.state == SessionState::Closed
                {
                    Poll::Ready(Ok(None))
                } else {
                    ctx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Some(task_ptr) => Poll::Ready(Ok(Some(task_ptr))),
        }
    }
}

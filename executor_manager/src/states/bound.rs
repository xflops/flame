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
use common::apis::{ExecutorState, TaskResult, TaskState};
use common::FlameError;

#[derive(Clone)]
pub struct BoundState {
    pub client: BackendClient,
    pub executor: Executor,
}

#[async_trait]
impl State for BoundState {
    async fn execute(&mut self) -> Result<Executor, FlameError> {
        trace_fn!("BoundState::execute");

        let task = self.client.launch_task(&self.executor.clone()).await?;
        self.executor.task = task.clone();

        match task {
            Some(task_ctx) => {
                let shim_ptr =
                    &mut self
                        .executor
                        .shim_instance
                        .clone()
                        .ok_or(FlameError::InvalidState(
                            "no shim instance in bound state".to_string(),
                        ))?;
                let task_result = {
                    let mut shim = shim_ptr.lock().await;
                    match shim.on_task_invoke(&task_ctx).await {
                        Ok(task_result) => task_result,
                        Err(e) => {
                            tracing::error!(
                                "Task <{}/{}> failed during shim invocation on executor <{}>: {}",
                                task_ctx.session_id,
                                task_ctx.task_id,
                                self.executor.id,
                                e
                            );
                            TaskResult {
                                state: TaskState::Failed,
                                output: None,
                                message: Some(e.to_string()),
                            }
                        }
                    }
                };

                self.client
                    .complete_task(&self.executor.clone(), &task_result)
                    .await?;

                let (ssn_id, task_id) = {
                    let task = &self.executor.task.clone().unwrap();
                    (task.session_id.clone(), task.task_id.clone())
                };
                tracing::debug!("Complete task <{ssn_id}/{task_id}>")
            }
            None => {
                self.executor.state = ExecutorState::Unbinding;
            }
        }

        self.executor.task = None;

        Ok(self.executor.clone())
    }
}

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

use std::sync::Arc;

use async_trait::async_trait;
use stdng::{logs::TraceFn, trace_fn};

use crate::appmgr::ApplicationManager;
use crate::client::BackendClient;
use crate::executor::Executor;
use crate::shims;
use crate::states::State;
use common::apis::{
    ExecutorState, FlameResult, BIND_RESULT_APPLICATION_INSTALL_FAILED, BIND_RESULT_OK,
    BIND_RESULT_ON_SESSION_ENTER_FAILED, BIND_RESULT_SHIM_CREATE_FAILED,
};
use common::FlameError;

pub struct IdleState {
    pub client: BackendClient,
    pub executor: Executor,
    pub app_manager: Arc<ApplicationManager>,
}

#[async_trait]
impl State for IdleState {
    async fn execute(&mut self) -> Result<Executor, FlameError> {
        trace_fn!("IdleState::execute");

        let ssn = self.client.bind_executor(&self.executor.clone()).await?;

        let Some(ssn) = ssn else {
            tracing::debug!(
                "Executor <{}> is idle but no session is found, start to release.",
                &self.executor.id.clone()
            );

            self.executor.session = None;
            self.executor.state = ExecutorState::Releasing;
            return Ok(self.executor.clone());
        };

        tracing::debug!(
            "Try to bind to session <{}> which is one of application <{:?}>.",
            &ssn.session_id.clone(),
            &ssn.application.clone()
        );

        let executor_shim = self.executor.shim;
        let app_shim = ssn.application.shim;

        if executor_shim != app_shim {
            tracing::error!(
                "Shim mismatch: executor <{}> supports {:?}, but application <{}> requires {:?}. \
                This should not happen if the scheduler is working correctly.",
                self.executor.id,
                executor_shim,
                ssn.application.name,
                app_shim
            );
            return Err(FlameError::InvalidConfig(format!(
                "Shim mismatch: executor supports {:?}, application requires {:?}",
                executor_shim, app_shim
            )));
        }

        tracing::debug!(
            "Shim validation passed: executor <{}> and application <{}> both use {:?} shim.",
            self.executor.id,
            ssn.application.name,
            executor_shim
        );

        tracing::debug!(
            "Try to bind Executor <{}> to <{}>.",
            &self.executor.id.clone(),
            &ssn.session_id.clone()
        );

        let env_vars = match self.app_manager.install(&ssn.application).await {
            Ok(env_vars) => env_vars,
            Err(e) => {
                self.bind_executor_failed(
                    BIND_RESULT_APPLICATION_INSTALL_FAILED,
                    format!("application installation failed: {e}"),
                    &ssn,
                    None,
                )
                .await?;
                return Ok(self.executor.clone());
            }
        };

        let shim_ptr = match shims::new(&self.executor.clone(), &ssn.application, &env_vars).await {
            Ok(shim_ptr) => shim_ptr,
            Err(e) => {
                self.bind_executor_failed(
                    BIND_RESULT_SHIM_CREATE_FAILED,
                    format!("shim creation failed: {e}"),
                    &ssn,
                    None,
                )
                .await?;
                return Ok(self.executor.clone());
            }
        };

        let enter_result = {
            let mut shim = shim_ptr.lock().await;
            shim.on_session_enter(&ssn).await
        };
        if let Err(e) = enter_result {
            self.bind_executor_failed(
                BIND_RESULT_ON_SESSION_ENTER_FAILED,
                format!("on_session_enter failed: {e}"),
                &ssn,
                Some(shim_ptr.clone()),
            )
            .await?;
            return Ok(self.executor.clone());
        }

        self.client
            .bind_executor_completed(
                &self.executor.clone(),
                Some(FlameResult {
                    return_code: BIND_RESULT_OK,
                    message: None,
                }),
            )
            .await?;

        self.executor.shim_instance = Some(shim_ptr.clone());
        self.executor.session = Some(ssn.clone());
        self.executor.state = ExecutorState::Bound;

        tracing::debug!(
            "Executor <{}> was bound to <{}>.",
            &self.executor.id.clone(),
            &ssn.session_id.clone()
        );

        Ok(self.executor.clone())
    }
}

impl IdleState {
    async fn bind_executor_failed(
        &mut self,
        return_code: i32,
        message: String,
        ssn: &common::apis::SessionContext,
        shim_ptr: Option<shims::ShimPtr>,
    ) -> Result<(), FlameError> {
        tracing::warn!(
            "Executor <{}> failed to bind session <{}>: {}",
            self.executor.id,
            ssn.session_id,
            message
        );
        self.client
            .bind_executor_completed(
                &self.executor.clone(),
                Some(FlameResult {
                    return_code,
                    message: Some(message),
                }),
            )
            .await?;
        self.executor.session = Some(ssn.clone());
        self.executor.shim_instance = shim_ptr;
        self.executor.state = ExecutorState::Unbinding;
        Ok(())
    }
}

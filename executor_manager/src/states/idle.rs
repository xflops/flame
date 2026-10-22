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
use stdng::{async_ptr, await_ptr, lock_ptr, logs::TraceFn, trace_fn};

use crate::appmgr::ApplicationManager;
use crate::client::BackendClient;
use crate::executor::Executor;
use crate::shims;
use crate::states::State;
use common::apis::{
    ExecutorState, FlameResult, BIND_RESULT_OK, BIND_RESULT_ON_SESSION_ENTER_FAILED,
    BIND_RESULT_SHIM_CREATE_FAILED,
};
use common::FlameError;

pub struct IdleState {
    pub client: BackendClient,
    pub executor: Executor,
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

        let shim_ptr = match self.executor.shim_instance.clone() {
            Some(shim_ptr) => {
                tracing::debug!(
                    "Reuse retained instance for Executor <{}> and application <{}>.",
                    self.executor.id,
                    ssn.application.name
                );
                shim_ptr
            }
            None => {
                let app_mgr = self.executor.app_mgr.clone().ok_or_else(|| {
                    FlameError::InvalidState(format!(
                        "executor <{}> has no application manager",
                        self.executor.id
                    ))
                })?;
                let shim_ptr = match shims::new_ptr(&self.executor, Some(&ssn.application)) {
                    Ok(shim_ptr) => shim_ptr,
                    Err(e) => {
                        self.bind_executor_failed(
                            BIND_RESULT_SHIM_CREATE_FAILED,
                            format!("shim creation failed: {e}"),
                            &ssn,
                        )
                        .await?;
                        return Ok(self.executor.clone());
                    }
                };

                if let Err(error) = self.create_instance(shim_ptr.clone(), app_mgr).await {
                    if self.executor.state == ExecutorState::Releasing {
                        tracing::error!(executor_id = %self.executor.id, %error,
                            "Service startup cleanup failed; transitioning to Releasing");
                        return Ok(self.executor.clone());
                    }
                    self.bind_executor_failed(
                        BIND_RESULT_SHIM_CREATE_FAILED,
                        format!("shim service creation failed: {error}"),
                        &ssn,
                    )
                    .await?;
                    return Ok(self.executor.clone());
                }

                shim_ptr
            }
        };

        let enter_result = {
            let mut shim = await_ptr!(shim_ptr);
            shim.on_session_enter(&ssn).await
        };
        let response = match enter_result {
            Ok(response) => response,
            Err(e) => {
                self.bind_executor_failed(
                    BIND_RESULT_ON_SESSION_ENTER_FAILED,
                    format!("on_session_enter failed: {e}"),
                    &ssn,
                )
                .await?;
                return Ok(self.executor.clone());
            }
        };

        self.executor.session = Some(ssn.clone());
        self.executor.state = ExecutorState::Unbinding;
        let completed = self
            .client
            .bind_executor_completed(
                &self.executor.clone(),
                Some(FlameResult {
                    return_code: BIND_RESULT_OK,
                    message: None,
                }),
                response.executor_attributes.map(|attributes| {
                    lock_ptr!(attributes)
                        .expect("executor attributes mutex poisoned")
                        .clone()
                }),
            )
            .await;
        completed?;

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
    async fn create_instance(
        &mut self,
        shim: shims::ShimPtr,
        app_mgr: Arc<ApplicationManager>,
    ) -> Result<(), FlameError> {
        self.executor.shim_instance = Some(shim.clone());
        let result = {
            let mut service = await_ptr!(shim);
            service.create_instance(app_mgr).await
        };
        if result.is_err() {
            let cleanup = await_ptr!(shim).destroy_instance().await;
            if let Err(error) = cleanup {
                // Preserve the tracked shim and allocation. The executor loop
                // retries destruction through Releasing, rather than rebinding
                // an instance whose startup or cleanup did not finish.
                self.executor.state = ExecutorState::Releasing;
                return Err(error);
            }
            self.executor.shim_instance = None;
        }
        result
    }

    async fn bind_executor_failed(
        &mut self,
        return_code: i32,
        message: String,
        ssn: &common::apis::SessionContext,
    ) -> Result<(), FlameError> {
        tracing::warn!(
            "Executor <{}> failed to bind session <{}>: {}",
            self.executor.id,
            ssn.session_id,
            message
        );
        self.executor.session = Some(ssn.clone());
        self.executor.state = ExecutorState::Unbinding;
        let completed = self
            .client
            .bind_executor_completed(
                &self.executor.clone(),
                Some(FlameResult {
                    return_code,
                    message: Some(message),
                }),
                None,
            )
            .await;
        completed?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::gpu::GpuManager;
    use crate::devices::DeviceManager;
    use crate::shims::{SessionEnterResponse, Shim, TaskInvokeResponse};
    use common::apis::{ApplicationContext, ResourceRequirement, SessionContext, TaskContext};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountDestroy {
        destroyed: Arc<AtomicUsize>,
        fail_start: bool,
        fail_cleanup: bool,
    }

    #[async_trait]
    impl Shim for CountDestroy {
        async fn create_instance(&mut self, _: Arc<ApplicationManager>) -> Result<(), FlameError> {
            if self.fail_start {
                Err(FlameError::Internal("startup failed".into()))
            } else {
                Ok(())
            }
        }
        async fn on_session_enter(
            &mut self,
            _: &SessionContext,
        ) -> Result<SessionEnterResponse, FlameError> {
            unreachable!()
        }
        async fn on_task_invoke(
            &mut self,
            _: &TaskContext,
        ) -> Result<TaskInvokeResponse, FlameError> {
            unreachable!()
        }
        async fn on_session_leave(&mut self) -> Result<(), FlameError> {
            unreachable!()
        }
        async fn destroy_instance(&mut self) -> Result<(), FlameError> {
            self.destroyed.fetch_add(1, Ordering::SeqCst);
            if self.fail_cleanup {
                self.fail_cleanup = false;
                return Err(FlameError::Internal("cleanup failed".into()));
            }
            Ok(())
        }
    }

    fn fixture(
        fail_start: bool,
        fail_cleanup: bool,
    ) -> (
        IdleState,
        shims::ShimPtr,
        Arc<AtomicUsize>,
        tempfile::TempDir,
        Arc<crate::devices::DeviceManagers>,
    ) {
        let workdir = tempfile::tempdir().unwrap();
        let allocator = Arc::new(crate::devices::DeviceManagers::with_gpu(
            GpuManager::new()
                .with_workdir(workdir.path())
                .with_inventory(vec!["GPU-a".to_string()])
                .unwrap(),
        ));
        let request = ResourceRequirement {
            gpu: 1,
            ..Default::default()
        };
        let destroyed = Arc::new(AtomicUsize::new(0));
        let executor = Executor {
            id: "executor-1".to_string(),
            application: "test-app".to_string(),
            resreq: ResourceRequirement {
                gpu: 1,
                ..Default::default()
            },
            assignments: allocator
                .allocate(&{
                    let mut metadata = crate::devices::test_executor(request.clone());
                    metadata.id = "executor-1".into();
                    metadata
                })
                .unwrap(),
            node: "test-node".to_string(),
            shim: common::apis::Shim::Host,
            session: None,
            task: None,
            context: None,
            app_mgr: Some(Arc::new(ApplicationManager::new().unwrap())),
            dev_mgr: None,

            shim_instance: None,
            state: ExecutorState::Idle,
        };
        let state = IdleState {
            client: BackendClient::default(),
            executor,
        };
        let shim: shims::ShimPtr = async_ptr(CountDestroy {
            destroyed: destroyed.clone(),
            fail_start,
            fail_cleanup,
        });
        (state, shim, destroyed, workdir, allocator)
    }

    #[tokio::test]
    async fn successful_startup_retains_instance_and_executor_devices() {
        let (mut state, shim, destroyed, _workdir, allocator) = fixture(false, false);
        let app_mgr = state.executor.app_mgr.clone().unwrap();
        state.create_instance(shim.clone(), app_mgr).await.unwrap();
        assert!(Arc::ptr_eq(
            state.executor.shim_instance.as_ref().unwrap(),
            &shim
        ));
        assert_eq!(destroyed.load(Ordering::SeqCst), 0);
        assert!(allocator
            .allocate(&crate::devices::test_executor(
                state.executor.resreq.clone()
            ))
            .is_err());
    }

    #[tokio::test]
    async fn failed_bind_report_retains_recorded_service_and_executor_devices() {
        let (mut state, shim, destroyed, _workdir, allocator) = fixture(false, false);
        let request = state.executor.resreq.clone();
        let app_mgr = state.executor.app_mgr.clone().unwrap();
        state.create_instance(shim.clone(), app_mgr).await.unwrap();
        let session = SessionContext {
            session_id: "test-session".to_string(),
            application: ApplicationContext {
                name: "test-app".to_string(),
                shim: common::apis::Shim::Host,
                image: None,
                command: None,
                arguments: vec![],
                working_directory: None,
                environments: Default::default(),
                url: None,
                installer: None,
            },
            common_data: None,
            tokens: Default::default(),
            batch_size: 1,
        };
        assert!(state
            .bind_executor_failed(
                BIND_RESULT_ON_SESSION_ENTER_FAILED,
                "enter failed".to_string(),
                &session
            )
            .await
            .is_err());
        assert_eq!(destroyed.load(Ordering::SeqCst), 0);
        assert_eq!(state.executor.state, ExecutorState::Unbinding);
        assert_eq!(
            state.executor.session.as_ref().unwrap().session_id,
            session.session_id
        );
        assert!(Arc::ptr_eq(
            state.executor.shim_instance.as_ref().unwrap(),
            &shim
        ));
        await_ptr!(shim).destroy_instance().await.unwrap();
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_err());
        state.executor.release().unwrap();
        assert!(allocator
            .allocate(&crate::devices::test_executor(request.clone()))
            .is_err());
        allocator.release(&state.executor).unwrap();
        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_ok());
    }
    #[tokio::test]
    async fn failed_startup_clears_instance_after_cleanup() {
        let (mut state, shim, destroyed, _workdir, allocator) = fixture(true, false);
        let app_mgr = state.executor.app_mgr.clone().unwrap();
        assert!(state.create_instance(shim, app_mgr).await.is_err());
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
        assert!(state.executor.shim_instance.is_none());
        assert!(allocator
            .allocate(&crate::devices::test_executor(
                (state.executor.resreq).clone()
            ))
            .is_err());
    }

    #[tokio::test]
    async fn failed_startup_cleanup_retains_local_instance_and_devices() {
        let (mut state, shim, destroyed, _workdir, allocator) = fixture(true, true);
        let app_mgr = state.executor.app_mgr.clone().unwrap();
        let error = state
            .create_instance(shim.clone(), app_mgr)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cleanup failed"));
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
        assert_eq!(state.executor.state, ExecutorState::Releasing);
        assert!(Arc::ptr_eq(
            state.executor.shim_instance.as_ref().unwrap(),
            &shim
        ));
        assert!(allocator
            .allocate(&crate::devices::test_executor(
                (state.executor.resreq).clone()
            ))
            .is_err());
    }
}

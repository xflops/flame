/*
Copyright 2025 The Flame Authors.
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
use crate::shims;
use crate::states::State;
use common::apis::{ExecutorState, Shim};
use common::FlameError;

#[derive(Clone)]
pub struct ReleasingState {
    pub client: BackendClient,
    pub executor: Executor,
}

#[async_trait]
impl State for ReleasingState {
    async fn execute(&mut self) -> Result<Executor, FlameError> {
        trace_fn!("ReleasingState::execute");

        let shim_ptr = match self.executor.shim_instance.clone() {
            Some(shim_ptr) => Some(shim_ptr),
            None if self.executor.shim == Shim::Cri => Some(shims::new_ptr(&self.executor, None)?),
            None => None,
        };
        if let Some(shim_ptr) = shim_ptr {
            shim_ptr.lock().await.destroy_instance().await?;
            self.executor.shim_instance = None;
        }

        // Free device claims before Session Manager frees node capacity, so a
        // replacement executor never sees them still held.
        if let Some(dev_mgr) = &self.executor.dev_mgr {
            dev_mgr.release(&self.executor)?;
        }

        self.client
            .unregister_executor(&self.executor.clone())
            .await?;

        self.release_completed()?;

        Ok(self.executor.clone())
    }
}

impl ReleasingState {
    fn release_completed(&mut self) -> Result<(), FlameError> {
        // The executor and its retained service instance share one lifetime.
        // Drop the last executor-owned shim reference only after Session
        // Manager has accepted the unregister operation.
        self.executor.release()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appmgr::ApplicationManager;
    use crate::devices::DeviceManager;
    use crate::shims::{SessionEnterResponse, Shim as ShimService, ShimPtr, TaskInvokeResponse};
    use common::apis::{ResourceRequirement, SessionContext, Shim, TaskContext};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct TestShim;

    #[async_trait]
    impl ShimService for TestShim {
        async fn create_instance(
            &mut self,
            _app_manager: Arc<ApplicationManager>,
        ) -> Result<(), FlameError> {
            unreachable!()
        }

        async fn on_session_enter(
            &mut self,
            _ctx: &SessionContext,
        ) -> Result<SessionEnterResponse, FlameError> {
            unreachable!()
        }

        async fn on_task_invoke(
            &mut self,
            _ctx: &TaskContext,
        ) -> Result<TaskInvokeResponse, FlameError> {
            unreachable!()
        }

        async fn on_session_leave(&mut self) -> Result<(), FlameError> {
            unreachable!()
        }

        async fn destroy_instance(&mut self) -> Result<(), FlameError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn releasing_frees_device_claim_before_unregistering() {
        let gpu_workdir = tempfile::tempdir().unwrap();
        let manager = Arc::new(crate::devices::DeviceManagers::with_gpu(
            crate::devices::gpu::GpuManager::new()
                .with_workdir(gpu_workdir.path())
                .with_inventory(vec!["GPU-a".into()])
                .unwrap(),
        ));
        let mut executor = crate::devices::test_executor(ResourceRequirement {
            gpu: 1,
            ..Default::default()
        });
        executor.assignments = manager.allocate(&executor).unwrap();
        executor.dev_mgr = Some(manager.clone());
        executor.shim_instance = Some(Arc::new(Mutex::new(TestShim)));
        executor.state = ExecutorState::Releasing;
        let saved = gpu_workdir
            .path()
            .join(crate::devices::GPU)
            .join(format!("{}.json", executor.id));
        assert!(saved.exists());
        let mut state = ReleasingState {
            client: BackendClient::default(),
            executor,
        };
        // There is no backend connection, so unregister fails. The claim is
        // already free, so a replacement can be allocated once Session Manager
        // frees the node capacity.
        assert!(state.execute().await.is_err());
        assert!(state.executor.shim_instance.is_none());
        assert_eq!(state.executor.state, ExecutorState::Releasing);
        assert!(!saved.exists());
        let competitor = crate::devices::test_executor(ResourceRequirement {
            gpu: 1,
            ..Default::default()
        });
        assert!(manager.allocate(&competitor).is_ok());
        // Retried releasing stays idempotent.
        assert!(state.execute().await.is_err());
        assert!(manager.allocate(&competitor).is_ok());
    }

    #[tokio::test]
    async fn release_clears_retained_instance() {
        let gpu_workdir = tempfile::tempdir().unwrap();
        let allocator = Arc::new(crate::devices::DeviceManagers::with_gpu(
            crate::devices::gpu::GpuManager::new()
                .with_workdir(gpu_workdir.path())
                .with_inventory(vec!["GPU-a".to_string()])
                .unwrap(),
        ));
        let request = ResourceRequirement {
            gpu: 1,
            ..Default::default()
        };
        let shim: ShimPtr = Arc::new(Mutex::new(TestShim));
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
            node: "node-1".to_string(),
            shim: Shim::Host,
            session: None,
            task: None,
            context: None,
            app_mgr: None,
            dev_mgr: None,
            shim_instance: Some(shim),
            state: ExecutorState::Releasing,
        };
        let mut state = ReleasingState {
            client: BackendClient::default(),
            executor,
        };

        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_err());
        state
            .executor
            .shim_instance
            .as_ref()
            .unwrap()
            .lock()
            .await
            .destroy_instance()
            .await
            .unwrap();
        state.release_completed().unwrap();

        assert_eq!(state.executor.state, ExecutorState::Released);
        assert!(state.executor.shim_instance.is_none());
        assert!(state.executor.assignments.is_empty());
        assert!(allocator
            .allocate(&crate::devices::test_executor(request.clone()))
            .is_err());
        allocator.release(&state.executor).unwrap();
        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_ok());
    }
}

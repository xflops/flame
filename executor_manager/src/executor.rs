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

use std::convert::TryFrom;
use std::sync::Arc;
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use crate::appmgr::ApplicationManager;
use crate::client::BackendClient;
use crate::devices::{DeviceAssignments, DeviceManagers};
use crate::shims::ShimPtr;
use ::rpc::flame::v1::{self as rpc, ExecutorSpec, ExecutorStatus, Metadata};

use crate::states;
use common::apis::{ExecutorState, ResourceRequirement, SessionContext, Shim, TaskContext};
use common::{ctx::FlameClusterContext, FlameError};

#[derive(Clone)]
pub struct Executor {
    pub id: String,
    pub application: String,
    pub resreq: ResourceRequirement,
    /// FEM-local device assignment metadata; never serialized to Session Manager.
    pub assignments: DeviceAssignments,
    /// Shared FEM-local application manager, attached before worker startup.
    /// RPC-decoded executor metadata has no application manager pointer.
    pub app_mgr: Option<Arc<ApplicationManager>>,
    /// Shared FEM device managers, attached with `app_mgr`. Releasing frees the
    /// executor's claims through it before telling Session Manager it is gone.
    pub dev_mgr: Option<Arc<DeviceManagers>>,
    pub node: String,
    /// Supported shim type from executor-manager config.
    /// This indicates what type of shim this executor supports (Host or Wasm).
    pub shim: Shim,

    pub session: Option<SessionContext>,
    pub task: Option<TaskContext>,
    pub context: Option<FlameClusterContext>,

    /// The shim instance used for task execution.
    /// This holds the actual shim implementation pointer, created when
    /// the executor binds to a session.
    pub shim_instance: Option<ShimPtr>,

    pub state: ExecutorState,
}

pub type ExecutorPtr = MutexPtr<Executor>;

impl TryFrom<&rpc::Executor> for Executor {
    type Error = FlameError;

    fn try_from(e: &rpc::Executor) -> Result<Self, Self::Error> {
        // Validate presence of required proto fields
        let spec = e
            .spec
            .as_ref()
            .ok_or_else(|| FlameError::Internal("missing spec in executor response".to_string()))?;
        let status = e.status.as_ref().ok_or_else(|| {
            FlameError::Internal("missing status in executor response".to_string())
        })?;
        let metadata = e.metadata.as_ref().ok_or_else(|| {
            FlameError::Internal("missing metadata in executor response".to_string())
        })?;

        // Validate and convert state with a clear error on failure
        let state = rpc::ExecutorState::try_from(status.state)
            .map_err(|_| FlameError::Internal("invalid executor state".to_string()))?
            .into();

        // Validate nested fields
        let resreq = *spec
            .resreq
            .as_ref()
            .ok_or_else(|| FlameError::Internal("missing resreq in executor spec".to_string()))?;

        Ok(Executor {
            id: metadata.id.clone(),
            application: spec.application.clone(),
            resreq: resreq.into(),
            assignments: Default::default(),
            node: spec.node.clone(),
            shim: Shim::from(spec.shim()), // Get shim from spec
            session: None,
            task: None,
            context: None,
            app_mgr: None,
            dev_mgr: None,
            shim_instance: None,
            state,
        })
    }
}

impl From<Executor> for rpc::Executor {
    fn from(e: Executor) -> Self {
        rpc::Executor::from(&e)
    }
}

impl From<&Executor> for rpc::Executor {
    fn from(e: &Executor) -> Self {
        let metadata = Some(Metadata {
            id: e.id.clone(),
            name: e.id.clone(),
        });

        let spec = Some(ExecutorSpec {
            resreq: Some(e.resreq.clone().into()),
            node: e.node.clone(),
            shim: rpc::Shim::from(e.shim).into(), // Include shim in spec
            application: e.application.clone(),
        });

        let status = Some(ExecutorStatus {
            state: rpc::ExecutorState::from(e.state).into(),
            session_id: e.session.clone().map(|s| s.session_id),
        });

        rpc::Executor {
            metadata,
            spec,
            status,
        }
    }
}

impl Executor {
    pub(crate) fn release(&mut self) -> Result<(), FlameError> {
        self.assignments.clear();
        self.shim_instance = None;
        self.session = None;
        self.task = None;
        self.state = ExecutorState::Released;
        Ok(())
    }

    pub fn update(&mut self, next: &Executor) {
        tracing::debug!(
            "Update executor <{}> from <{}> to <{}>",
            self.id,
            self.state,
            next.state
        );
        self.application = next.application.clone();
        self.state = next.state;
        self.shim_instance = next.shim_instance.clone();
        self.assignments = next.assignments.clone();
        self.session = next.session.clone();
        self.task = next.task.clone();
    }
}

pub fn start(client: BackendClient, executor: ExecutorPtr) -> Result<(), FlameError> {
    tokio::task::spawn(async move {
        loop {
            let exec = match lock_ptr!(executor) {
                Ok(exec) => exec.clone(),
                Err(error) => {
                    tracing::error!("Failed to lock executor: {error}");
                    return;
                }
            };
            if exec.state == ExecutorState::Released {
                tracing::info!(executor_id = %exec.id, "Executor is released, exit");
                return;
            }
            let mut state = match states::from(client.clone(), executor.clone()) {
                Ok(state) => state,
                Err(error) => {
                    tracing::error!(executor_id = %exec.id, %error, "Failed to build executor state");
                    return;
                }
            };
            match state.execute().await {
                Ok(next) => {
                    let mut shared = match lock_ptr!(executor) {
                        Ok(shared) => shared,
                        Err(error) => {
                            tracing::error!(executor_id = %exec.id, %error, "Failed to update executor state");
                            return;
                        }
                    };
                    if shared.state != ExecutorState::Released {
                        shared.update(&next);
                    }
                }
                Err(error) => {
                    tracing::error!(executor_id = %exec.id, node = %exec.node, state = %exec.state,
                        %error, "Failed to execute executor state");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::gpu::GpuManager;
    use crate::devices::DeviceManager;
    use crate::shims::{SessionEnterResponse, Shim as ShimService, TaskInvokeResponse};
    use async_trait::async_trait;
    use stdng::{async_ptr, await_ptr};
    use tokio::sync::Notify;

    struct DelayedDestroy {
        fail_first: bool,
        entered: Arc<Notify>,
        finish: Arc<Notify>,
    }

    #[async_trait]
    impl ShimService for DelayedDestroy {
        async fn create_instance(&mut self, _: Arc<ApplicationManager>) -> Result<(), FlameError> {
            unreachable!()
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
            self.entered.notify_one();
            self.finish.notified().await;
            if self.fail_first {
                self.fail_first = false;
                Err(FlameError::Internal("cleanup failed".into()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn direct_service_cleanup_keeps_allocation_until_explicit_release() {
        let gpu_workdir = tempfile::tempdir().unwrap();
        let allocator = Arc::new(crate::devices::DeviceManagers::with_gpu(
            GpuManager::new()
                .with_workdir(gpu_workdir.path())
                .with_inventory(vec!["GPU-a".to_string()])
                .unwrap(),
        ));
        let request = ResourceRequirement {
            gpu: 1,
            ..Default::default()
        };
        let entered = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let mut executor = Executor {
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
            shim: Shim::Host,
            session: None,
            task: None,
            context: None,
            app_mgr: None,
            dev_mgr: None,
            shim_instance: Some(async_ptr(DelayedDestroy {
                fail_first: false,
                entered: entered.clone(),
                finish: finish.clone(),
            })),
            state: ExecutorState::Bound,
        };
        let shim = executor.shim_instance.clone().unwrap();
        let cleanup = tokio::spawn(async move { await_ptr!(shim).destroy_instance().await });
        entered.notified().await;
        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_err());
        finish.notify_one();
        cleanup.await.unwrap().unwrap();
        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_err());
        executor.release().unwrap();
        assert!(allocator
            .allocate(&crate::devices::test_executor(request.clone()))
            .is_err());
        allocator.release(&executor).unwrap();
        assert!(allocator
            .allocate(&crate::devices::test_executor((request).clone()))
            .is_ok());
    }
    #[tokio::test]
    async fn released_executor_exits_without_running_a_state() {
        let mut metadata = crate::devices::test_executor(ResourceRequirement::default());
        metadata.state = ExecutorState::Released;
        let executor = stdng::new_ptr(metadata);
        start(BackendClient::default(), executor.clone()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while Arc::strong_count(&executor) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(lock_ptr!(executor).unwrap().state, ExecutorState::Released);
    }

    #[tokio::test]
    async fn releasing_executor_requires_backend_acknowledgement() {
        let workdir = tempfile::tempdir().unwrap();
        let devices = Arc::new(crate::devices::DeviceManagers::with_gpu(
            GpuManager::new()
                .with_workdir(workdir.path())
                .with_inventory(vec!["GPU-a".into()])
                .unwrap(),
        ));
        let mut metadata = crate::devices::test_executor(ResourceRequirement {
            gpu: 1,
            ..Default::default()
        });
        metadata.assignments = devices.allocate(&metadata).unwrap();

        metadata.state = ExecutorState::Releasing;
        let entered = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        metadata.shim_instance = Some(async_ptr(DelayedDestroy {
            fail_first: false,
            entered: entered.clone(),
            finish: finish.clone(),
        }));
        let executor = stdng::new_ptr(metadata);
        start(BackendClient::default(), executor.clone()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert_eq!(lock_ptr!(executor).unwrap().state, ExecutorState::Releasing);
        let mut competing = crate::devices::test_executor(ResourceRequirement {
            gpu: 1,
            ..Default::default()
        });
        competing.id = "competing".into();
        assert!(devices.allocate(&competing).is_err());
        finish.notify_one();
        // The failed backend unregister leaves both the persisted ownership and
        // local Releasing state intact; only FEM later frees the physical claim.
        assert!(workdir
            .path()
            .join(crate::devices::GPU)
            .join(format!("{}.json", lock_ptr!(executor).unwrap().id))
            .exists());
        assert!(devices.allocate(&competing).is_err());
        assert_eq!(lock_ptr!(executor).unwrap().state, ExecutorState::Releasing);
        lock_ptr!(executor).unwrap().state = ExecutorState::Released;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while Arc::strong_count(&executor) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(devices.allocate(&competing).is_err());
        devices.release(&lock_ptr!(executor).unwrap()).unwrap();
        assert!(devices.allocate(&competing).is_ok());
    }
}

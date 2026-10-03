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
use std::sync::{Arc, Mutex};
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};
use tokio::task::JoinHandle;

use crate::appmgr::ApplicationManager;
use crate::client::BackendClient;
use crate::shims::ShimPtr;
use ::rpc::flame::v1::{self as rpc, ExecutorSpec, ExecutorStatus, Metadata};

use crate::states;
use common::apis::{ExecutorState, ResourceRequirement, SessionContext, Shim, TaskContext};
use common::{ctx::FlameClusterContext, FlameError};

#[derive(Clone)]
pub struct Executor {
    pub id: String,
    pub name: String,
    pub application: String,
    pub workspace: String,
    pub resreq: ResourceRequirement,
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

pub type ExecutorPtr = Arc<Mutex<Executor>>;

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
            name: metadata.name.clone(),
            application: spec.application.clone(),
            workspace: metadata.workspace.clone().ok_or_else(|| {
                FlameError::Internal("missing workspace in executor metadata".to_string())
            })?,
            resreq: resreq.into(),
            node: spec.node.clone(),
            shim: Shim::from(spec.shim()), // Get shim from spec
            session: None,
            task: None,
            context: None,
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
            name: e.name.clone(),
            workspace: Some(e.workspace.clone()),
        });

        let spec = Some(ExecutorSpec {
            resreq: Some(e.resreq.clone().into()),
            node: e.node.clone(),
            shim: rpc::Shim::from(e.shim).into(), // Include shim in spec
            application: e.application.clone(),
        });

        let status = Some(ExecutorStatus {
            state: rpc::ExecutorState::from(e.state).into(),
            session: e.session.clone().map(|s| s.session),
        });

        rpc::Executor {
            metadata,
            spec,
            status,
        }
    }
}

impl Executor {
    pub(crate) fn release(&mut self) {
        self.shim_instance = None;
        self.session = None;
        self.task = None;
        self.state = ExecutorState::Released;
    }

    pub fn update(&mut self, next: &Executor) {
        tracing::debug!(
            "Update executor <{}> from <{}> to <{}>",
            self.name,
            self.state,
            next.state
        );
        self.application = next.application.clone();
        self.workspace = next.workspace.clone();
        self.state = next.state;
        self.shim_instance = next.shim_instance.clone();
        self.session = next.session.clone();
        self.task = next.task.clone();
    }
}

pub fn start(client: BackendClient, executor: ExecutorPtr, app_manager: Arc<ApplicationManager>) {
    tokio::task::spawn(async move {
        loop {
            let exec = {
                let exec = lock_ptr!(executor);
                match exec {
                    Ok(exec) => exec.clone(),
                    Err(e) => {
                        tracing::error!("Failed to lock executor: {e}");
                        return;
                    }
                }
            };

            if exec.state == ExecutorState::Released {
                tracing::info!("Executor <{}> is released, exit.", exec.name);
                break;
            }

            let mut state = states::from(client.clone(), exec.clone(), app_manager.clone());
            match state.execute().await {
                Ok(next_state) => {
                    let mut exec = lock_ptr!(executor);
                    match exec {
                        Ok(mut exec) => {
                            // A server-pushed removal wins over a state
                            // transition that was already in flight.
                            if exec.state == ExecutorState::Released {
                                break;
                            }
                            exec.update(&next_state);
                        }
                        Err(e) => {
                            tracing::error!("Failed to lock executor: {e}");
                        }
                    }
                }
                Err(e) => {
                    let session = exec
                        .session
                        .as_ref()
                        .map(|session| session.session.as_str());
                    let application = exec
                        .session
                        .as_ref()
                        .map(|session| session.application.name.as_str());
                    let task = exec.task.as_ref().map(|task| task.task.as_str());
                    let task_session = exec.task.as_ref().map(|task| task.session.as_str());
                    tracing::error!(
                        executor = %exec.name,
                        node = %exec.node,
                        state = %exec.state,
                        session = ?session,
                        application = ?application,
                        task_session = ?task_session,
                        task = ?task,
                        error = %e,
                        "Failed to execute executor state"
                    );
                    // State/RPC errors are not necessarily shim crashes. Keep the worker alive so
                    // session-manager can drive the executor through unbinding and release.
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executor_message() -> rpc::Executor {
        rpc::Executor::from(common::apis::Executor {
            id: uuid::Uuid::new_v4().to_string(),
            name: "executor".into(),
            workspace: "team".into(),
            application: "app".into(),
            node: "node".into(),
            ..Default::default()
        })
    }

    #[test]
    fn rpc_roundtrip_preserves_workspace_in_metadata() {
        let message = executor_message();
        let executor = Executor::try_from(&message).unwrap();
        assert_eq!(executor.workspace, "team");
        let restored = rpc::Executor::from(executor);
        assert_eq!(restored.metadata, message.metadata);
        assert_eq!(restored.spec, message.spec);
    }

    #[test]
    fn rpc_requires_workspace_in_metadata() {
        let mut message = executor_message();
        message.metadata.as_mut().unwrap().workspace = None;
        assert!(Executor::try_from(&message).is_err());
    }
}

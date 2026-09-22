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

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use stdng::collections;

use crate::controller::ControllerPtr;
use crate::model::{ExecutorInfo, ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShotPtr};
use crate::scheduler::actions::{ActionPtr, AllocateAction, DispatchAction, ShuffleAction};
use crate::scheduler::plugins::{PluginManager, PluginManagerPtr, PluginsOptions};
use common::apis::{ExecutorID, ExecutorState};
use common::FlameError;

/// One scheduling cycle: a single `Context` (one [`PluginManager::setup`] on the current
/// snapshot) is shared by Dispatch → Allocate → Shuffle. In-memory plugin counters accumulate
/// across those actions; do not re-run `setup` between them.
pub struct Context {
    pub snapshot: SnapShotPtr,
    pub controller: ControllerPtr,
    pub actions: Vec<ActionPtr>,
    pub plugins: PluginManagerPtr,
}

impl Context {
    pub fn new(controller: ControllerPtr, options: &PluginsOptions) -> Result<Self, FlameError> {
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot.clone(), options)?;

        Ok(Context {
            snapshot,
            plugins,
            controller,
            actions: vec![
                DispatchAction::new_ptr(),
                AllocateAction::new_ptr(),
                ShuffleAction::new_ptr(),
            ],
        })
    }

    pub fn is_underused(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        self.plugins.is_underused(ssn)
    }

    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        self.plugins.is_ready(ssn)
    }

    pub fn is_preemptible(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        self.plugins.is_preemptible(ssn)
    }

    pub fn is_allocatable(
        &self,
        node: &NodeInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<bool, FlameError> {
        self.plugins.is_allocatable(node, ssn)
    }

    pub fn is_available(
        &self,
        exec: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<bool, FlameError> {
        if exec.application != ssn.application {
            return Ok(false);
        }
        if ssn
            .resreq
            .as_ref()
            .is_some_and(|resreq| resreq != &exec.resreq)
        {
            return Ok(false);
        }
        self.plugins.is_available(exec, ssn)
    }

    pub fn select_executor(
        &self,
        session: &SessionInfoPtr,
        idle_executors: &HashMap<ExecutorID, ExecutorInfoPtr>,
    ) -> Result<Option<ExecutorInfoPtr>, FlameError> {
        let mut eligible = idle_executors
            .values()
            .map(|executor| {
                Ok(
                    (executor.ssn_id.is_none() && self.is_available(executor, session)?)
                        .then(|| executor.clone()),
                )
            })
            .collect::<Result<Vec<_>, FlameError>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        eligible.sort_by(|e1, e2| self.plugins.executor_order_fn(session, e2, e1));
        Ok(eligible.into_iter().next())
    }

    pub async fn allocate_executor(
        &self,
        node: &NodeInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        let executor = self
            .controller
            .create_executor(node.name.clone(), ssn.id.clone())
            .await?;
        let exec_info = Arc::new(ExecutorInfo::from(&executor));
        self.snapshot.add_executor(exec_info.clone())?;
        self.plugins.on_executor_pipeline(exec_info, ssn.clone())
    }

    /// Accounts for an executor reserved by this scheduling cycle.
    pub fn pipeline_executor(
        &self,
        exec: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        self.plugins.on_executor_pipeline(exec.clone(), ssn.clone())
    }

    pub async fn bind_session(
        &self,
        exec: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        self.controller
            .bind_session(exec.id.clone(), ssn.id.clone())
            .await?;
        self.plugins.on_session_bind(ssn.clone())?;
        self.snapshot
            .update_executor_state(exec.clone(), ExecutorState::Binding)?;

        Ok(())
    }

    pub async fn unbind_session(
        &self,
        exec: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        self.controller.unbind_executor(exec.id.clone()).await?;
        self.plugins.on_session_unbind(ssn.clone())?;
        self.snapshot
            .update_executor_state(exec.clone(), ExecutorState::Unbinding)?;

        Ok(())
    }

    pub async fn release_executor(&self, exec: &ExecutorInfoPtr) -> Result<(), FlameError> {
        self.controller.release_executor(exec.id.clone()).await?;

        self.snapshot
            .update_executor_state(exec.clone(), ExecutorState::Releasing)?;

        Ok(())
    }
}

pub fn ssn_order_fn(ctx: &Context) -> impl collections::Cmp<SessionInfoPtr> {
    SsnOrderFn {
        plugin_mgr: ctx.plugins.clone(),
    }
}

struct SsnOrderFn {
    plugin_mgr: PluginManagerPtr,
}

impl collections::Cmp<SessionInfoPtr> for SsnOrderFn {
    fn cmp(&self, t1: &SessionInfoPtr, t2: &SessionInfoPtr) -> Ordering {
        self.plugin_mgr.ssn_order_fn(t1, t2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionInfo;
    use bytes::Bytes;
    use common::apis::{ExecutorState, ResourceRequirement, Session, Shim, Task};
    use common::ctx::{FlameCluster, FlameClusterContext};
    use std::collections::HashSet;

    #[tokio::test]
    async fn select_executor_uses_context_plugins() {
        let resreq = ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        };
        let mut source = Session {
            id: "session".to_string(),
            application: "test-app".to_string(),
            resreq: Some(resreq.clone()),
            ..Default::default()
        };
        source
            .update_task(&Task {
                id: 1,
                ssn_id: source.id.clone(),
                affinity: HashSet::from([Bytes::from_static(b"local")]),
                ..Default::default()
            })
            .unwrap();

        let session = Arc::new(SessionInfo::try_from(&source).unwrap());
        let snapshot = crate::model::SnapShot::new();
        snapshot.add_session(session.clone()).unwrap();
        let options = PluginsOptions {
            policies: vec!["das".to_string()],
            ..Default::default()
        };
        let plugins = PluginManager::setup(&snapshot, &options).unwrap();

        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        let controller = crate::controller::new_ptr(storage);
        let context = Context {
            snapshot: Arc::new(snapshot),
            controller,
            actions: vec![],
            plugins,
        };

        let local = Arc::new(ExecutorInfo {
            id: "z-local".to_string(),
            state: ExecutorState::Idle,
            application: "test-app".to_string(),
            resreq: resreq.clone(),
            attributes: HashSet::from([Bytes::from_static(b"local")]),
            ..Default::default()
        });
        let remote = Arc::new(ExecutorInfo {
            id: "y-remote".to_string(),
            state: ExecutorState::Idle,
            application: "test-app".to_string(),
            resreq: resreq.clone(),
            ..Default::default()
        });
        let owned = Arc::new(ExecutorInfo {
            id: "a-owned".to_string(),
            state: ExecutorState::Idle,
            application: "test-app".to_string(),
            ssn_id: Some("other-session".to_string()),
            resreq: resreq.clone(),
            attributes: HashSet::from([Bytes::from_static(b"local")]),
            ..Default::default()
        });
        let wrong_resource = Arc::new(ExecutorInfo {
            id: "b-wrong-resource".to_string(),
            state: ExecutorState::Idle,
            application: "test-app".to_string(),
            resreq: ResourceRequirement {
                cpu: 2,
                ..resreq.clone()
            },
            attributes: HashSet::from([Bytes::from_static(b"local")]),
            ..Default::default()
        });
        let wrong_shim = Arc::new(ExecutorInfo {
            id: "c-wrong-shim".to_string(),
            state: ExecutorState::Idle,
            application: "test-app".to_string(),
            resreq: resreq.clone(),
            shim: Shim::Wasm,
            attributes: HashSet::from([Bytes::from_static(b"local")]),
            ..Default::default()
        });
        let wrong_application = Arc::new(ExecutorInfo {
            id: "d-wrong-application".to_string(),
            state: ExecutorState::Idle,
            application: "other-app".to_string(),
            resreq,
            attributes: HashSet::from([Bytes::from_static(b"local")]),
            ..Default::default()
        });
        let idle_executors = HashMap::from([
            (local.id.clone(), local.clone()),
            (remote.id.clone(), remote.clone()),
            (owned.id.clone(), owned),
            (wrong_resource.id.clone(), wrong_resource),
            (wrong_shim.id.clone(), wrong_shim),
            (wrong_application.id.clone(), wrong_application.clone()),
        ]);

        assert!(context.is_available(&local, &session).unwrap());
        assert!(context.is_available(&remote, &session).unwrap());
        assert!(!context.is_available(&wrong_application, &session).unwrap());

        let selected = context
            .select_executor(&session, &idle_executors)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&selected, &local));

        let first_eligible = idle_executors
            .values()
            .find(|executor| {
                executor.ssn_id.is_none()
                    && context.is_available(executor, &session).unwrap_or(false)
            })
            .unwrap()
            .clone();
        let options = PluginsOptions {
            policies: vec![],
            ..Default::default()
        };
        let plugins = PluginManager::setup(&context.snapshot, &options).unwrap();
        let context_without_das = Context {
            snapshot: context.snapshot.clone(),
            controller: context.controller.clone(),
            actions: vec![],
            plugins,
        };
        let selected = context_without_das
            .select_executor(&session, &idle_executors)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&selected, &first_eligible));
    }
}

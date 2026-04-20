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
use std::sync::Arc;

use stdng::collections;

use crate::controller::ControllerPtr;
use crate::model::{ExecutorInfo, ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShotPtr};
use crate::scheduler::actions::{ActionPtr, AllocateAction, DispatchAction, ShuffleAction};
use crate::scheduler::plugins::{PluginManager, PluginManagerPtr};
use common::apis::ExecutorState;
use common::FlameError;

/// One scheduling cycle: a single `Context` (one [`PluginManager::setup`] on the current
/// snapshot) is shared by Dispatch → Allocate → Shuffle. In-memory plugin counters (e.g. Gang)
/// accumulate across those actions; do not re-run `setup` between them.
pub struct Context {
    pub snapshot: SnapShotPtr,
    pub controller: ControllerPtr,
    pub actions: Vec<ActionPtr>,
    pub plugins: PluginManagerPtr,
}

impl Context {
    pub fn new(controller: ControllerPtr) -> Result<Self, FlameError> {
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot.clone())?;

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
        self.plugins.is_available(exec, ssn)
    }

    /// Allocation-side batch readiness (e.g. Gang: pipelined + allocated executors form full
    /// batches). Reflects in-memory plugin state, including `Statement` ops earlier in this
    /// same cycle (typically pipeline/allocate before commit).
    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        self.plugins.is_ready(ssn)
    }

    /// Binding-side batch readiness (e.g. Gang: bound + on-session executors form full batches).
    /// After Dispatch commits binds, this can be true so Allocate skips provisioning.
    pub fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        self.plugins.is_fulfilled(ssn)
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

    pub async fn pipeline_session(
        &self,
        exec: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<(), FlameError> {
        self.plugins.on_session_bind(ssn.clone())?;

        // self.snapshot
        //     .update_executor_state(exec.clone(), ExecutorState::Binding)?;

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

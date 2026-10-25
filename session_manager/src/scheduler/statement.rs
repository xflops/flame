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

use crate::model::{ExecutorInfo, ExecutorInfoPtr, SessionInfoPtr};
use crate::scheduler::Context;
use common::apis::ExecutorState;
use common::FlameError;

/// A batch of Dispatch decisions that takes effect all at once.
///
/// `bind` stages a decision: the plugins and the cycle's snapshot see it at
/// once, so later decisions in the same cycle account for it, but the
/// controller is untouched. `commit` applies the staged binds to the
/// controller; `discard` takes them back from the plugins and the snapshot.
pub struct Statement<'a> {
    ctx: &'a Context,
    binds: Vec<(ExecutorInfoPtr, SessionInfoPtr)>,
}

impl<'a> Statement<'a> {
    pub fn new(ctx: &'a Context) -> Self {
        Statement { ctx, binds: vec![] }
    }

    /// Whether nothing has been staged.
    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }

    /// Stages binding the Idle executor to the session.
    pub fn bind(&mut self, exec: &ExecutorInfoPtr, ssn: &SessionInfoPtr) -> Result<(), FlameError> {
        self.ctx
            .plugins
            .on_session_bind(exec.clone(), ssn.clone())?;
        let bound = Arc::new(ExecutorInfo {
            ssn_id: Some(ssn.id.clone()),
            ..exec.as_ref().clone()
        });
        self.ctx
            .snapshot
            .update_executor_state(bound, ExecutorState::Binding)?;
        self.binds.push((exec.clone(), ssn.clone()));

        Ok(())
    }

    /// Applies the staged binds to the controller, in order.
    ///
    /// Controller calls cannot be undone, so a failure keeps the binds already
    /// applied and discards the rest from the plugins and the snapshot; the
    /// next cycle sees what is still missing.
    pub async fn commit(mut self) -> Result<(), FlameError> {
        for committed in 0..self.binds.len() {
            let (exec, ssn) = &self.binds[committed];
            if let Err(e) = self
                .ctx
                .controller
                .bind_session(exec.id.clone(), ssn.id.clone())
                .await
            {
                self.binds.drain(..committed);
                self.discard()?;
                return Err(e);
            }
        }

        Ok(())
    }

    /// Takes the staged binds back, newest first, and returns their executors
    /// to the Idle pool.
    pub fn discard(self) -> Result<Vec<ExecutorInfoPtr>, FlameError> {
        let mut execs = vec![];
        for (exec, ssn) in self.binds.into_iter().rev() {
            self.ctx
                .plugins
                .on_session_unbind(exec.clone(), ssn.clone())?;
            self.ctx
                .snapshot
                .update_executor_state(exec.clone(), ExecutorState::Idle)?;
            execs.push(exec);
        }

        Ok(execs)
    }
}

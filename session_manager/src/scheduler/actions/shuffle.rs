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

use stdng::collections::{BinaryHeap, Cmp};
use stdng::{logs::TraceFn, trace_fn};

use crate::model::{BOUND_EXECUTOR, IDLE_EXECUTOR, READY_SESSION};
use crate::scheduler::actions::{Action, ActionPtr};
use crate::scheduler::ctx::Context;
use crate::scheduler::plugins::ssn_order_fn;

use common::FlameError;

pub struct ShuffleAction {}

impl ShuffleAction {
    pub fn new_ptr() -> ActionPtr {
        Arc::new(ShuffleAction {})
    }
}

#[async_trait::async_trait]
impl Action for ShuffleAction {
    async fn execute(&self, ctx: &mut Context) -> Result<(), FlameError> {
        trace_fn!("ShuffleAction::execute");
        let ss = ctx.snapshot.clone();

        let mut underused = BinaryHeap::new(ssn_order_fn(ctx));
        let open_ssns = ss.find_sessions(READY_SESSION)?;
        for ssn in open_ssns.values() {
            if ctx.is_underused(ssn)? {
                underused.push(ssn.clone());
            }
        }

        let mut bound_execs = ss.find_executors(BOUND_EXECUTOR)?;

        // Unbind overused sessions for underused sessions.
        loop {
            if underused.is_empty() {
                break;
            }

            let ssn = underused
                .pop()
                .expect("failed to pop underused session: loop guard ensures non-empty");
            if !ctx.is_underused(&ssn)? {
                continue;
            }

            let mut exec = None;
            for (_, e) in bound_execs.iter_mut() {
                tracing::debug!(
                    "Try to unbound Executor <{}> for session <{}>",
                    e.id,
                    ssn.id.clone()
                );

                let target_ssn = match e.ssn_id.clone() {
                    Some(ssn_id) => Some(ss.get_session(&ssn_id)?),
                    None => None,
                };

                if let Some(target_ssn) = target_ssn {
                    if !ctx.is_preemptible(&target_ssn)? {
                        continue;
                    }

                    // Unbind the overused session, so the executor will
                    // become idle and be allocated to the underused session.
                    ctx.unbind_session(e, &target_ssn).await?;
                    exec = Some(e.clone());

                    break;
                }
            }

            if let Some(exec) = exec {
                tracing::debug!(
                    "Executor <{}> was pipelined to session <{}>, remove it from bound list.",
                    exec.id,
                    ssn.id.clone()
                );

                bound_execs.remove(&exec.id);

                // Pipeline the executor to the underused session to avoid over allocation.
                ctx.pipeline_session(&exec, &ssn).await?;
                underused.push(ssn.clone());
            }
        }

        // Release Idle executors, so the resource can be reallocated.
        let idle_execs = ss.find_executors(IDLE_EXECUTOR)?;
        for exec in idle_execs.values() {
            ctx.release_executor(exec).await?;
        }

        Ok(())
    }
}

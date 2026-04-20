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

use crate::model::{ALL_NODE, OPEN_SESSION, UNBINDING_EXECUTOR, VOID_EXECUTOR};
use crate::scheduler::actions::{Action, ActionPtr};
use crate::scheduler::plugins::node_order_fn;
use crate::scheduler::plugins::ssn_order_fn;
use crate::scheduler::statement::Statement;
use crate::scheduler::Context;

use common::FlameError;

pub struct AllocateAction {}

impl AllocateAction {
    pub fn new_ptr() -> ActionPtr {
        Arc::new(AllocateAction {})
    }
}

#[async_trait::async_trait]
impl Action for AllocateAction {
    async fn execute(&self, ctx: &mut Context) -> Result<(), FlameError> {
        trace_fn!("AllocateAction::execute");
        let ss = ctx.snapshot.clone();

        ss.debug()?;

        let mut open_ssns = BinaryHeap::new(ssn_order_fn(ctx));
        let ssn_list = ss.find_sessions(OPEN_SESSION)?;
        for ssn in ssn_list.values() {
            open_ssns.push(ssn.clone());
        }

        let mut nodes = vec![];
        let node_list = ss.find_nodes(ALL_NODE)?;
        for node in node_list.values() {
            nodes.push(node.clone());
        }

        tracing::debug!(
            "AllocateAction: {} open sessions, {} nodes available",
            open_ssns.len(),
            nodes.len()
        );

        let mut void_executors = ss.find_executors(VOID_EXECUTOR)?;
        let mut unbinding_executors = ss.find_executors(UNBINDING_EXECUTOR)?;

        tracing::debug!(
            "AllocateAction: {} void executors, {} unbinding executors",
            void_executors.len(),
            unbinding_executors.len()
        );

        let node_order_fn = node_order_fn(ctx);

        loop {
            if open_ssns.is_empty() {
                break;
            }

            let ssn = open_ssns
                .pop()
                .expect("failed to pop open session: loop guard ensures non-empty");

            let is_underused = ctx.is_underused(&ssn)?;
            if !is_underused {
                tracing::debug!(
                    "Session <{}> is NOT underused (pending={:?}, running={:?}), skipping allocation",
                    ssn.id,
                    ssn.tasks_status.get(&common::apis::TaskState::Pending),
                    ssn.tasks_status.get(&common::apis::TaskState::Running)
                );
                continue;
            }

            tracing::debug!(
                "Session <{}> IS underused (pending={:?}, running={:?}), attempting allocation",
                ssn.id,
                ssn.tasks_status.get(&common::apis::TaskState::Pending),
                ssn.tasks_status.get(&common::apis::TaskState::Running)
            );

            if let Some(max_instances) = ssn.max_instances {
                let all_executors = ss.find_executors(None)?;
                let current_count = all_executors
                    .values()
                    .filter(|e| e.ssn_id.as_ref() == Some(&ssn.id))
                    .count();
                if current_count >= max_instances as usize {
                    tracing::debug!(
                        "Session <{}> has reached max_instances limit: {} >= {}",
                        ssn.id,
                        current_count,
                        max_instances
                    );
                    continue;
                }
            }

            // Dispatch runs first in this cycle. `PluginManager` is not re-setup between actions
            // (see `Context`): Gang's counters include binds Dispatch committed via
            // `Statement` in this same `Context`. If those already satisfy gang scheduling, do not
            // create more executors here.
            let fulfilled = ctx.is_fulfilled(&ssn)?;
            let ready = ctx.is_ready(&ssn)?;
            if fulfilled || ready {
                tracing::debug!(
                    "Skip allocate resources for session <{}>: is_fulfilled={}, is_ready={}",
                    ssn.id,
                    fulfilled,
                    ready
                );
                continue;
            }

            let mut stmt = Statement::new(ss.clone(), ctx.plugins.clone(), ctx.controller.clone());

            let pipelineable = void_executors
                .values()
                .chain(unbinding_executors.values())
                .filter(|e| ctx.is_available(e, &ssn).unwrap_or(false));

            for exec in pipelineable {
                stmt.pipeline(exec, &ssn)?;
                if ctx.is_ready(&ssn)? {
                    break;
                }
            }

            for node in nodes.iter() {
                if ctx.is_ready(&ssn)? {
                    break;
                }
                while ctx.is_allocatable(node, &ssn)? && !ctx.is_ready(&ssn)? {
                    stmt.allocate(node, &ssn)?;
                }
            }

            if ctx.is_ready(&ssn)? {
                let op_count = stmt.len();
                let pipelined_ids = stmt.commit().await?;
                for id in &pipelined_ids {
                    void_executors.remove(id);
                    unbinding_executors.remove(id);
                }
                tracing::info!("Committed {} op(s) for session <{}>", op_count, ssn.id);
                nodes.sort_by(|a, b| node_order_fn.cmp(a, b));
                open_ssns.push(ssn.clone());
            } else if !stmt.is_empty() {
                tracing::debug!(
                    "Discarding incomplete batch for session <{}>: not enough resources",
                    ssn.id
                );
                stmt.discard()?;
            }
        }

        Ok(())
    }
}

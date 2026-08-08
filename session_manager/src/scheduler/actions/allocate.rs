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

use crate::model::{ALL_NODE, READY_SESSION, UNBINDING_EXECUTOR, VOID_EXECUTOR};
use crate::scheduler::actions::{Action, ActionPtr};
use crate::scheduler::plugins::ssn_order_fn;
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
        let ssn_list = ss.find_sessions(READY_SESSION)?;
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

            if ctx.is_ready(&ssn)? {
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

            if let Some(exec) = void_executors
                .values()
                .find(|e| ctx.is_available(e, &ssn).unwrap_or(false))
                .cloned()
            {
                ctx.pipeline_executor(&exec, &ssn)?;
                void_executors.remove(&exec.id);
            } else if let Some(exec) = unbinding_executors
                .values()
                .find(|e| ctx.is_available(e, &ssn).unwrap_or(false))
                .cloned()
            {
                ctx.pipeline_executor(&exec, &ssn)?;
                unbinding_executors.remove(&exec.id);
            } else if let Some(node) = nodes
                .iter()
                .find(|node| ctx.is_allocatable(node, &ssn).unwrap_or(false))
            {
                ctx.allocate_executor(node, &ssn).await?;
                tracing::debug!("Allocated executor for session <{}>", ssn.id);
            }
        }

        Ok(())
    }
}

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

use async_trait::async_trait;
use std::sync::Arc;
use std::{thread, time};
use tokio::time::Duration;

use crate::controller::ControllerPtr;
use crate::scheduler::ctx::Context;
use crate::scheduler::plugins::PluginsOptions;

use crate::FlameThread;
use common::ctx::FlameClusterContext;
use common::FlameError;

mod actions;
mod ctx;
mod plugins;
mod statement;

pub fn new(controller: ControllerPtr) -> Arc<dyn FlameThread> {
    Arc::new(ScheduleRunner { controller })
}

struct ScheduleRunner {
    controller: ControllerPtr,
}

#[async_trait]
impl FlameThread for ScheduleRunner {
    async fn run(&self, flame_ctx: FlameClusterContext) -> Result<(), FlameError> {
        let schedule_interval = flame_ctx.cluster.schedule_interval;
        let options = PluginsOptions::from(&flame_ctx);
        tracing::info!(
            "Scheduler started with interval: {}ms, enabled policies: {:?}, max executors per node: {}",
            schedule_interval,
            options.policies,
            options.limits.node.max_executors,
        );
        let schedule_interval = Duration::from_millis(schedule_interval);

        loop {
            let mut ctx = Context::new(self.controller.clone(), &options)?;

            // Same `ctx` (and thus same in-memory `plugins`) is used for every action.
            for action in ctx.actions.clone() {
                if let Err(e) = action.execute(&mut ctx).await {
                    tracing::error!("Failed to run scheduling: {e}");
                    break;
                };
            }

            self.controller
                .wait_for_scheduler_event(schedule_interval)
                .await;
        }
    }
}

#[cfg(test)]
mod tests;

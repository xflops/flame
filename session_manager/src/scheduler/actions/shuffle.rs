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

use std::collections::HashSet;
use std::sync::Arc;

use stdng::{logs::TraceFn, trace_fn};

use chrono::{DateTime, Duration, Utc};

use crate::model::{
    ExecutorInfo, SessionFilter, SnapShot, ALL_EXECUTOR, BOUND_EXECUTOR, IDLE_EXECUTOR,
};
use crate::scheduler::actions::{Action, ActionPtr};
use crate::scheduler::ctx::Context;

use common::apis::SessionState;
use common::FlameError;

pub struct ShuffleAction {}

pub(crate) fn idle_executor_expired(
    snapshot: &SnapShot,
    executor: &ExecutorInfo,
) -> Result<bool, FlameError> {
    let Some(application) = snapshot.get_application(&executor.application)? else {
        return Ok(true);
    };
    let delay_release = application.delay_release;
    let idle_delay = delay_release.max(Duration::zero()) * 2;
    let idle_age = Utc::now().signed_duration_since(executor.latest_updated_timestamp);
    Ok(idle_age < Duration::zero() || idle_age >= idle_delay)
}

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

        // Complete the suspends: a Suspending session no executor serves becomes Suspended. Binds run
        // in this same loop, so the snapshot cannot miss one; a stale snapshot only over-counts executors.
        let suspending =
            ss.find_sessions(Some(SessionFilter::by_state(SessionState::Suspending)))?;
        if !suspending.is_empty() {
            let serving: HashSet<_> = ss
                .find_executors(ALL_EXECUTOR)?
                .values()
                .filter_map(|executor| executor.ssn_id.clone())
                .collect();
            for session in suspending.values() {
                if serving.contains(&session.id) {
                    continue;
                }
                if let Err(e) = ctx.suspend_session(session).await {
                    tracing::warn!(
                        "Failed to complete suspend of session <{}>: {e}",
                        session.id
                    );
                }
            }
        }

        // Reclaim surplus bindings even when no recipient currently needs them.
        // Interrupt requests take effect after any in-flight task completes.
        for executor in ss.find_executors(BOUND_EXECUTOR)?.values() {
            if !ctx.is_reclaimable(executor)? {
                continue;
            }
            let Some(session_id) = &executor.ssn_id else {
                continue;
            };
            let session = ss.get_session(session_id)?;
            if ctx.is_overused(&session)? {
                ctx.interrupt_executor(executor, &session).await?;
            }
        }

        // Release retained Idle executors after the application's grace period,
        // so DAS can reuse them without allowing indefinite resource starvation.
        let idle_execs = ss.find_executors(IDLE_EXECUTOR)?;
        for exec in idle_execs.values() {
            if idle_executor_expired(&ss, exec)? {
                ctx.release_executor(exec).await?;
            }
        }

        Ok(())
    }
}

/*
Copyright 2026 The Flame Authors.
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
use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use stdng::lock_ptr;

use crate::model::{ExecutorInfo, SessionInfo, SnapShot};
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::apis::SessionID;
use common::FlameError;

/// Orders eligible Idle executors by coverage of a session's Pending affinity.
///
/// Equal scores, including no match at all, prefer the executor that publishes fewer attributes: the
/// attributes are the keys an executor holds, so that is the emptier cache, and work without a
/// match lands where it evicts the least. Equal counts prefer the executor that has been Idle the
/// longest (it was used least recently), and equal times fall back to the executor ID.
#[derive(Default)]
pub struct DasPlugin {
    affinity: HashMap<SessionID, HashSet<Bytes>>,
}

impl DasPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(Self::default())
    }

    pub(crate) fn score(&self, executor: &ExecutorInfo, session: &SessionInfo) -> usize {
        self.affinity.get(&session.id).map_or(0, |affinity| {
            executor
                .attributes
                .iter()
                .filter(|key| affinity.contains(*key))
                .count()
        })
    }
}

impl Plugin for DasPlugin {
    fn name(&self) -> &'static str {
        "das"
    }

    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.affinity.clear();

        let sessions = lock_ptr!(ss.sessions)?;
        for session in sessions.values() {
            let affinity = self.affinity.entry(session.id.clone()).or_default();
            if let Some(tasks) = session.task_index.get(&common::apis::TaskState::Pending) {
                for task in tasks.values() {
                    affinity.extend(task.affinity.iter().cloned());
                }
            }
        }

        Ok(())
    }

    fn executor_order_fn(
        &self,
        session: &SessionInfo,
        e1: &ExecutorInfo,
        e2: &ExecutorInfo,
    ) -> Option<Ordering> {
        Some(
            self.score(e1, session)
                .cmp(&self.score(e2, session))
                // Equal scores: fewer attributes, an emptier cache, wins ...
                .then_with(|| e2.attributes.len().cmp(&e1.attributes.len()))
                // ... then the executor Idle the longest, which was updated least recently ...
                .then_with(|| {
                    e2.latest_updated_timestamp
                        .cmp(&e1.latest_updated_timestamp)
                })
                // ... and then a smaller ID, so the order is deterministic.
                .then_with(|| e2.id.cmp(&e1.id)),
        )
    }
}

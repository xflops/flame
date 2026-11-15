/*
Copyright 2025 The Flame Authors.
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

use common::apis::{SessionID, TaskState};
use common::FlameError;

use crate::model::{SessionInfo, SessionInfoPtr, SnapShot, OPEN_SESSION};
use crate::scheduler::plugins::{Plugin, PluginPtr};

/// PriorityPlugin orders sessions by priority; it never blocks allocation.
///
/// Higher `priority` value = scheduled first. Sessions with the same priority
/// defer ordering to downstream plugins. Demand, bounds and fairness are owned
/// by MinMax and the other policies.
///
/// # Stale Data
///
/// Like all plugins, state is computed once in `setup()` per scheduling cycle
/// from the snapshot. Changes during the cycle are not reflected until the next cycle.
pub struct PriorityPlugin {
    /// Priority for each open session, keyed by session ID.
    /// Populated in `setup()`.
    pub(crate) ssn_priority: HashMap<SessionID, u32>,
}

impl PriorityPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(PriorityPlugin {
            ssn_priority: HashMap::new(),
        })
    }
}

impl Plugin for PriorityPlugin {
    fn name(&self) -> &'static str {
        "priority"
    }

    /// Records each open session's priority.
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_priority.clear();
        for ssn in ss.find_sessions(OPEN_SESSION)?.values() {
            self.ssn_priority.insert(ssn.id.clone(), ssn.priority);
        }
        Ok(())
    }

    /// Returns ordering based on priority (descending). Returns `None` when
    /// priorities are equal, deferring tiebreaking to the next plugin in the chain.
    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let p1 = self.ssn_priority.get(&s1.id).copied().unwrap_or(0);
        let p2 = self.ssn_priority.get(&s2.id).copied().unwrap_or(0);

        if p1 != p2 {
            // Higher priority comes first → descending order.
            Some(p2.cmp(&p1))
        } else {
            // Equal priority: no opinion; defer to the next plugin in the chain.
            None
        }
    }
}

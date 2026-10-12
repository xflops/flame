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
    ssn_priority: HashMap<SessionID, u32>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppInfo, SessionInfo, SnapShot};
    use chrono::Utc;
    use common::apis::{ApplicationState, ResourceRequirement, SessionState, Shim, TaskState};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_plugin() -> PriorityPlugin {
        PriorityPlugin {
            ssn_priority: HashMap::new(),
        }
    }

    fn create_test_session(id: &str, priority: u32, pending: i32) -> Arc<SessionInfo> {
        let mut tasks_status = HashMap::new();
        if pending > 0 {
            tasks_status.insert(TaskState::Pending, pending);
        }
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            tasks_status,
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            retry_count: 0,
            task_index: HashMap::new(),
        })
    }

    fn create_snapshot(sessions: Vec<Arc<SessionInfo>>) -> SnapShot {
        let ss = SnapShot::new();
        ss.add_application(Arc::new(AppInfo {
            name: "test-app".to_string(),
            state: ApplicationState::Enabled,
            shim: Shim::Host,
            max_instances: 0,
            delay_release: chrono::Duration::zero(),
        }))
        .unwrap();
        for ssn in sessions {
            ss.add_session(ssn).unwrap();
        }
        ss
    }

    // ── setup() tests ────────────────────────────────────────────────────────

    #[test]
    fn test_setup_records_priorities() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 2);
        let ss = create_snapshot(vec![ssn_high, ssn_low]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_priority.get("ssn-high"), Some(&100));
        assert_eq!(plugin.ssn_priority.get("ssn-low"), Some(&10));
    }

    // ── ssn_order_fn() tests ─────────────────────────────────────────────────

    #[test]
    fn test_ssn_order_fn_different_priorities() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot(vec![ssn_high.clone(), ssn_low.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(
            plugin.ssn_order_fn(&ssn_high, &ssn_low),
            Some(Ordering::Less)
        );
        assert_eq!(
            plugin.ssn_order_fn(&ssn_low, &ssn_high),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn test_ssn_order_fn_equal_priorities_defers() {
        let ssn_a = create_test_session("ssn-a", 50, 4);
        let ssn_b = create_test_session("ssn-b", 50, 4);
        let ss = create_snapshot(vec![ssn_a.clone(), ssn_b.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_order_fn(&ssn_a, &ssn_b), None);
    }

    #[test]
    fn test_priority_never_blocks_allocation() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot(vec![ssn_high.clone(), ssn_low.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn_low), None);
        assert_eq!(plugin.is_ready(&ssn_low), None);
    }
}

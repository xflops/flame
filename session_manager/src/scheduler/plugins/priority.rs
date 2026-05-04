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

use crate::model::{
    ExecutorInfoPtr, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot, ALL_EXECUTOR, OPEN_SESSION,
};
use crate::scheduler::plugins::{Plugin, PluginPtr};

/// PriorityPlugin implements priority-based session ordering and allocation blocking.
///
/// # Behavior
///
/// - Higher `priority` value = higher scheduling priority.
/// - Sessions with the same priority use FairShare ordering (tiebreaker).
/// - Sessions at the highest needy priority tier receive executors until their full
///   task demand is met (`ssn_desired`).  PriorityPlugin returns `Some(true)` while
///   `allocated < desired`, so FairShare's conservative one-unit `deserved` is bypassed
///   for high-priority sessions.
/// - All sessions at a lower priority than `max_needy_priority` are hard-blocked
///   (`Some(false)`).
///
/// # Interaction with PluginManager
///
/// `PluginManager::is_underused` uses "first non-`None` wins" ordering.  Because
/// PriorityPlugin is registered first, its opinion is always definitive.  FairShare is
/// consulted only when PriorityPlugin returns `None` (satisfied sessions where desired
/// is already met).
///
/// # Stale Data
///
/// Like all plugins, state is computed once in `setup()` per scheduling cycle
/// from the snapshot. Changes during the cycle are not reflected until the next cycle.
pub struct PriorityPlugin {
    /// Maximum priority among open sessions that have pending tasks.
    /// Computed in `setup()`; used in `ssn_order_fn` and `is_underused`.
    max_needy_priority: u32,
    /// Priority for each open session, keyed by session ID.
    /// Populated in `setup()`.
    ssn_priority: HashMap<SessionID, u32>,
    /// Full task-demand for each session (batch-aligned pending+running × slots).
    /// Computed in `setup()` using the task-count formula.  High-priority sessions
    /// remain underused until `ssn_allocated[id] >= ssn_desired[id]`.
    ssn_desired: HashMap<SessionID, f64>,
    /// Executor slots currently allocated per session.
    /// Initialised from the snapshot in `setup()`; updated via callbacks.
    ssn_allocated: HashMap<SessionID, f64>,
}

impl PriorityPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(PriorityPlugin {
            max_needy_priority: 0,
            ssn_priority: HashMap::new(),
            ssn_desired: HashMap::new(),
            ssn_allocated: HashMap::new(),
        })
    }
}

impl Plugin for PriorityPlugin {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_priority.clear();
        self.ssn_desired.clear();
        self.ssn_allocated.clear();
        self.max_needy_priority = 0;

        let open_ssns = ss.find_sessions(OPEN_SESSION)?;
        for ssn in open_ssns.values() {
            let priority = ssn.priority;
            self.ssn_priority.insert(ssn.id.clone(), priority);

            // Full task demand: batch-aligned (pending + running) × slots.
            // Includes running tasks so the session retains executors that are
            // already busy and continues to ask for more while pending work remains.
            let mut task_count = 0.0_f64;
            for state in [TaskState::Pending, TaskState::Running] {
                if let Some(d) = ssn.tasks_status.get(&state) {
                    task_count += *d as f64;
                }
            }
            let batch_size = ssn.batch_size.max(1) as f64;
            let batched = (task_count / batch_size).floor() * batch_size;
            let mut desired = batched * ssn.slots as f64;

            if let Some(max_instances) = ssn.max_instances {
                desired = desired.min((max_instances * ssn.slots) as f64);
            }
            let min_alloc = (ssn.min_instances * ssn.slots) as f64;
            desired = desired.max(min_alloc);

            self.ssn_desired.insert(ssn.id.clone(), desired);
            self.ssn_allocated.insert(ssn.id.clone(), 0.0);

            // A session is "needy" if it has pending tasks — it can benefit from more executors.
            let pending = ssn
                .tasks_status
                .get(&TaskState::Pending)
                .copied()
                .unwrap_or(0);

            if pending > 0 && priority > self.max_needy_priority {
                self.max_needy_priority = priority;
            }
        }

        // Initialise allocated counts from current executor assignments.
        let executors = ss.find_executors(ALL_EXECUTOR)?;
        for exe in executors.values() {
            if let Some(ref ssn_id) = exe.ssn_id {
                if let Some(alloc) = self.ssn_allocated.get_mut(ssn_id) {
                    *alloc += exe.slots as f64;
                }
            }
        }

        tracing::debug!(
            "[PriorityPlugin] setup: max_needy_priority={}, tracked_sessions={}",
            self.max_needy_priority,
            self.ssn_priority.len()
        );

        Ok(())
    }

    /// Returns ordering based on priority (descending). Returns `None` when
    /// priorities are equal, deferring tiebreaking to FairShare.
    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let p1 = self.ssn_priority.get(&s1.id).copied().unwrap_or(0);
        let p2 = self.ssn_priority.get(&s2.id).copied().unwrap_or(0);

        if p1 != p2 {
            // Higher priority comes first → descending order.
            // p2.cmp(&p1) reverses the natural ascending order.
            Some(p2.cmp(&p1))
        } else {
            // Equal priority: no opinion; defer to FairShare for ratio-based tiebreaking.
            None
        }
    }

    /// Priority-aware underuse decision.
    ///
    /// - `priority < max_needy_priority` → `Some(false)`: hard-blocked by a higher-priority
    ///   needy session.
    /// - `priority >= max_needy_priority` and `allocated < desired` → `Some(true)`: session
    ///   still has unmet task demand; keep allocating.
    /// - `priority >= max_needy_priority` and demand satisfied → `None`: defer to FairShare.
    ///
    /// Because `PluginManager::is_underused` uses "first non-`None` wins", the `Some(true)`
    /// path overrides FairShare's conservative `deserved`-based veto for high-priority sessions.
    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let priority = self.ssn_priority.get(&ssn.id).copied()?;

        if priority < self.max_needy_priority {
            tracing::debug!(
                "[PriorityPlugin] Session <{}> (priority={}) blocked: needy session at priority={} exists",
                ssn.id, priority, self.max_needy_priority
            );
            return Some(false);
        }

        // At or above the highest needy priority: check whether this session still
        // has unmet demand (allocated < desired).
        let desired = self.ssn_desired.get(&ssn.id).copied().unwrap_or(0.0);
        let allocated = self.ssn_allocated.get(&ssn.id).copied().unwrap_or(0.0);

        if desired > 0.0 && allocated < desired {
            tracing::debug!(
                "[PriorityPlugin] Session <{}> (priority={}) underused: allocated={} < desired={}",
                ssn.id,
                priority,
                allocated,
                desired
            );
            Some(true)
        } else {
            // Demand satisfied or no demand: let FairShare decide.
            None
        }
    }

    fn on_executor_allocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            *alloc += ssn.slots as f64;
        }
    }

    fn on_executor_unallocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            *alloc -= ssn.slots as f64;
        }
    }

    fn on_executor_pipeline(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            *alloc += ssn.slots as f64;
        }
    }

    fn on_executor_discard(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            *alloc -= ssn.slots as f64;
        }
    }

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            *alloc += ssn.slots as f64;
        }
    }

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            *alloc -= ssn.slots as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppInfo, ExecutorInfo, NodeInfo, SessionInfo, SnapShot};
    use chrono::Utc;
    use common::apis::{
        ExecutorState, NodeState, ResourceRequirement, SessionState, Shim, TaskState,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_plugin() -> PriorityPlugin {
        PriorityPlugin {
            max_needy_priority: 0,
            ssn_priority: HashMap::new(),
            ssn_desired: HashMap::new(),
            ssn_allocated: HashMap::new(),
        }
    }

    fn create_test_session(id: &str, priority: u32, pending: i32) -> Arc<SessionInfo> {
        create_test_session_full(id, priority, pending, 0, 1, 1, 0, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_test_session_full(
        id: &str,
        priority: u32,
        pending: i32,
        running: i32,
        slots: u32,
        batch_size: u32,
        min_instances: u32,
        max_instances: Option<u32>,
    ) -> Arc<SessionInfo> {
        let mut tasks_status = HashMap::new();
        if pending > 0 {
            tasks_status.insert(TaskState::Pending, pending);
        }
        if running > 0 {
            tasks_status.insert(TaskState::Running, running);
        }
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            slots,
            tasks_status,
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances,
            max_instances,
            batch_size,
            priority,
        })
    }

    fn create_test_app(name: &str) -> Arc<AppInfo> {
        Arc::new(AppInfo {
            name: name.to_string(),
            shim: Shim::Host,
            max_instances: 0,
            delay_release: chrono::Duration::zero(),
        })
    }

    fn create_snapshot(sessions: Vec<Arc<SessionInfo>>) -> SnapShot {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });
        ss.add_application(create_test_app("test-app")).unwrap();
        for ssn in sessions {
            ss.add_session(ssn).unwrap();
        }
        ss
    }

    fn create_snapshot_with_executor(
        sessions: Vec<Arc<SessionInfo>>,
        exec_ssn_id: &str,
        slots: u32,
    ) -> SnapShot {
        let ss = create_snapshot(sessions);
        let exec = Arc::new(ExecutorInfo {
            id: "exec-1".to_string(),
            node: "node-1".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
            },
            slots,
            shim: Shim::Host,
            task_id: None,
            ssn_id: Some(exec_ssn_id.to_string()),
            creation_time: Utc::now(),
            state: ExecutorState::Bound,
        });
        ss.add_executor(exec).unwrap();
        ss
    }

    // ── setup() tests ────────────────────────────────────────────────────────

    #[test]
    fn test_setup_max_needy_priority() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 2);
        let ss = create_snapshot(vec![ssn_high, ssn_low]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.max_needy_priority, 100);
        assert_eq!(plugin.ssn_priority.get("ssn-high"), Some(&100));
        assert_eq!(plugin.ssn_priority.get("ssn-low"), Some(&10));
    }

    #[test]
    fn test_setup_desired_equals_batch_aligned_task_count_times_slots() {
        // pending=5, running=1, batch_size=2, slots=3
        // task_count=6, batched=floor(6/2)*2=6, desired=6*3=18
        let ssn = create_test_session_full("s", 0, 5, 1, 3, 2, 0, None);
        let ss = create_snapshot(vec![ssn]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(18.0));
        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(0.0));
    }

    #[test]
    fn test_setup_desired_respects_max_instances() {
        // pending=10, slots=1, max_instances=4 → desired capped at 4
        let ssn = create_test_session_full("s", 0, 10, 0, 1, 1, 0, Some(4));
        let ss = create_snapshot(vec![ssn]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(4.0));
    }

    #[test]
    fn test_setup_desired_respects_min_instances() {
        // pending=0, min_instances=2, slots=3 → desired floored at 6
        let ssn = create_test_session_full("s", 0, 0, 0, 3, 1, 2, None);
        let ss = create_snapshot(vec![ssn]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(6.0));
    }

    #[test]
    fn test_setup_allocated_counts_existing_executors() {
        // Session with 1 executor (slots=2) already bound
        let ssn = create_test_session_full("s", 0, 4, 0, 2, 1, 0, None);
        let ss = create_snapshot_with_executor(vec![ssn], "s", 2);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(2.0));
    }

    #[test]
    fn test_setup_no_pending_does_not_set_max_needy() {
        let ssn_high = create_test_session("ssn-high", 100, 0); // 0 pending
        let ssn_low = create_test_session("ssn-low", 10, 5);
        let ss = create_snapshot(vec![ssn_high, ssn_low]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.max_needy_priority, 10);
    }

    #[test]
    fn test_setup_all_same_priority_no_blocking() {
        let ssn_a = create_test_session("ssn-a", 0, 5);
        let ssn_b = create_test_session("ssn-b", 0, 3);
        let ss = create_snapshot(vec![ssn_a, ssn_b]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.max_needy_priority, 0);
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

    // ── is_underused() tests ─────────────────────────────────────────────────

    #[test]
    fn test_is_underused_lower_priority_blocked() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot(vec![ssn_high, ssn_low.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn_low), Some(false));
    }

    #[test]
    fn test_is_underused_high_priority_with_demand_returns_true() {
        // High-priority session with pending tasks and no executors yet → Some(true)
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot(vec![ssn_high.clone(), ssn_low]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        // allocated=0 < desired=4 → Some(true)
        assert_eq!(plugin.is_underused(&ssn_high), Some(true));
    }

    #[test]
    fn test_is_underused_high_priority_fully_allocated_defers() {
        // Session at max priority but already fully allocated → None (defer to FairShare)
        let ssn_high = create_test_session("ssn-high", 100, 4); // desired=4
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot_with_executor(vec![ssn_high.clone(), ssn_low], "ssn-high", 1);

        // Setup gives allocated=1 for ssn-high. desired=4, so still underused.
        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();
        assert_eq!(plugin.is_underused(&ssn_high), Some(true));

        // Manually drive allocated to full desired (4).
        if let Some(a) = plugin.ssn_allocated.get_mut("ssn-high") {
            *a = 4.0;
        }
        // Now allocated == desired → None (defers to FairShare)
        assert_eq!(plugin.is_underused(&ssn_high), None);
    }

    #[test]
    fn test_is_underused_all_same_priority_with_demand_returns_true() {
        // All sessions at same priority with demand → Some(true) for each
        let ssn_a = create_test_session("ssn-a", 0, 5);
        let ssn_b = create_test_session("ssn-b", 0, 3);
        let ss = create_snapshot(vec![ssn_a.clone(), ssn_b.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn_a), Some(true));
        assert_eq!(plugin.is_underused(&ssn_b), Some(true));
    }

    #[test]
    fn test_is_underused_no_demand_defers() {
        // Session with no tasks (desired=0) → None regardless of priority
        let ssn = create_test_session("s", 100, 0);
        let ss = create_snapshot(vec![ssn.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn), None);
    }

    // ── allocation callback tests ────────────────────────────────────────────

    #[test]
    fn test_on_executor_allocate_increments_allocated() {
        let ssn = create_test_session_full("s", 0, 4, 0, 2, 1, 0, None);
        let ss = create_snapshot(vec![ssn.clone()]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();
        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(0.0));

        let node = Arc::new(NodeInfo {
            name: "n".to_string(),
            allocatable: ResourceRequirement::default(),
            state: NodeState::Ready,
        });
        plugin.on_executor_allocate(node, ssn.clone());
        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(2.0));
    }

    #[test]
    fn test_on_executor_unallocate_decrements_allocated() {
        let ssn = create_test_session_full("s", 0, 4, 0, 2, 1, 0, None);
        let ss = create_snapshot_with_executor(vec![ssn.clone()], "s", 2);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();
        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(2.0));

        let node = Arc::new(NodeInfo {
            name: "n".to_string(),
            allocatable: ResourceRequirement::default(),
            state: NodeState::Ready,
        });
        plugin.on_executor_unallocate(node, ssn.clone());
        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(0.0));
    }
}

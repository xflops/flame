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
    ExecutorInfoPtr, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot, ALL_EXECUTOR, ALL_NODE,
    OPEN_SESSION,
};
use crate::scheduler::plugins::{Plugin, PluginPtr};

/// PriorityPlugin implements priority-based session ordering and allocation blocking.
///
/// # Behavior
///
/// - Higher `priority` value = higher scheduling priority.
/// - Sessions with the same priority use FairShare ordering (tiebreaker for `ssn_order_fn`)
///   and creation_time ascending order (tiebreaker for the `setup()` distribution loop).
/// - `setup()` distributes the cluster's `total_slots` across open sessions in
///   `(priority desc, creation_time asc)` order, capping per-session demand at the
///   remaining cluster capacity. This guarantees `Σ ssn_desired ≤ total_slots`.
/// - Sessions at the highest needy priority tier remain underused until their
///   priority-distributed share (`ssn_desired`) is filled.
/// - All sessions at a lower priority than `max_needy_priority` are hard-blocked
///   (`Some(false)`).
///
/// # Interaction with PluginManager
///
/// `PluginManager::is_underused` uses "first non-`None` wins" ordering. Because
/// PriorityPlugin is registered first, its opinion is always definitive. FairShare is
/// consulted only when PriorityPlugin returns `None` (satisfied sessions where desired
/// is already met).
///
/// # Stale Data
///
/// Like all plugins, state is computed once in `setup()` per scheduling cycle
/// from the snapshot. Changes during the cycle are not reflected until the next cycle.
pub struct PriorityPlugin {
    /// Maximum priority among open sessions that have pending tasks.
    /// Computed in `setup()`; used in `is_underused`.
    max_needy_priority: u32,
    /// Priority for each open session, keyed by session ID.
    /// Populated in `setup()`.
    ssn_priority: HashMap<SessionID, u32>,
    /// Per-session priority-distributed share. Populated in `setup()` step 2 from the
    /// `total_slots` distribution loop. Read-only thereafter for the cycle.
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

/// Per-session task-driven demand ceiling, in slots.
///
/// Sums pending + running task counts, rounds the total down to whole batches,
/// multiplies by `slots`, then clamps to `[min_instances * slots, max_instances * slots]`.
/// This is the same formula used by the previous (uncapped) implementation; the
/// only behavioural change in this RFE is that the cluster-capacity cap moves into
/// `setup()` and is applied *after* this per-session ceiling.
fn compute_demand(ssn: &SessionInfo) -> f64 {
    let mut task_count = 0.0_f64;
    for state in [TaskState::Pending, TaskState::Running] {
        if let Some(c) = ssn.tasks_status.get(&state) {
            task_count += *c as f64;
        }
    }
    let batch_size = ssn.batch_size.max(1) as f64;
    let batched = (task_count / batch_size).floor() * batch_size;
    let mut demand = batched * ssn.slots as f64;

    if let Some(max_i) = ssn.max_instances {
        demand = demand.min((max_i * ssn.slots) as f64);
    }
    demand.max((ssn.min_instances * ssn.slots) as f64)
}

impl Plugin for PriorityPlugin {
    fn name(&self) -> &'static str {
        "priority"
    }

    /// Priority-aware resource distribution per FS.md §3 *PriorityPlugin*.
    ///
    /// Three-step algorithm executed once per scheduling cycle:
    ///
    /// 1. Sum `total_slots` from the snapshot's nodes.
    /// 2. Sort open sessions by `(priority desc, creation_time asc)` and walk them in
    ///    order, granting each session `min(compute_demand(ssn), remaining)` slots
    ///    and decrementing `remaining`. The result is `ssn_desired[id]`.
    ///    `max_needy_priority` is updated in the same pass for any session with
    ///    pending tasks.
    /// 3. Initialise `ssn_allocated[id]` from currently-bound executors.
    ///
    /// Invariants enforced (FS.md §3 *Invariants*):
    /// - `Σ ssn_desired ≤ total_slots` — each grant is capped at `remaining`,
    ///   which starts at `total_slots` and is monotonically decremented.
    /// - Equality holds when aggregate demand exceeds cluster capacity (capacity
    ///   binds): higher-priority and earlier sessions saturate first, later ones
    ///   absorb the residual until the cluster is exhausted.
    /// - Within a priority tier, earlier-created sessions are filled before later
    ///   ones (`creation_time ascending` is the secondary sort key).
    /// - `ssn_allocated` runtime updates remain handled by the executor / session
    ///   lifecycle callbacks; this method only sets the *initial* counts.
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_priority.clear();
        self.ssn_desired.clear();
        self.ssn_allocated.clear();
        self.max_needy_priority = 0;

        // ── Step 1: total_slots ──────────────────────────────────────────────
        // Cluster's physical scheduling capacity, taken from the same snapshot
        // every plugin sees this cycle.
        let total_slots: f64 = ss
            .find_nodes(ALL_NODE)?
            .values()
            .map(|n| n.allocatable.to_slots(&ss.unit) as f64)
            .sum();

        // ── Step 2: distribute total_slots by (priority desc, creation_time asc) ─
        // Hash iteration is non-deterministic; collect and sort explicitly so that
        // earlier-created sessions within a priority tier are filled first.
        let open_ssns = ss.find_sessions(OPEN_SESSION)?;
        let mut sessions: Vec<SessionInfoPtr> = open_ssns.values().cloned().collect();
        sessions.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority) // priority descending
                .then(a.creation_time.cmp(&b.creation_time)) // earlier first
        });

        let mut remaining = total_slots;
        for ssn in &sessions {
            self.ssn_priority.insert(ssn.id.clone(), ssn.priority);

            let demand = compute_demand(ssn);
            let granted = demand.min(remaining.max(0.0));

            self.ssn_desired.insert(ssn.id.clone(), granted);
            self.ssn_allocated.insert(ssn.id.clone(), 0.0);
            remaining -= granted;

            // A session is "needy" if it has pending tasks — it can benefit from
            // more executors. Used by is_underused() to block strictly lower tiers.
            let pending = ssn
                .tasks_status
                .get(&TaskState::Pending)
                .copied()
                .unwrap_or(0);
            if pending > 0 && ssn.priority > self.max_needy_priority {
                self.max_needy_priority = ssn.priority;
            }
        }

        // ── Step 3: ssn_allocated initial counts from bound executors ────────
        // Runtime updates after this point flow through the on_executor_* /
        // on_session_* callbacks (unchanged by this RFE).
        let executors = ss.find_executors(ALL_EXECUTOR)?;
        for exe in executors.values() {
            if let Some(ref ssn_id) = exe.ssn_id {
                if let Some(alloc) = self.ssn_allocated.get_mut(ssn_id) {
                    *alloc += exe.slots as f64;
                }
            }
        }

        tracing::debug!(
            "[PriorityPlugin] setup: total_slots={}, max_needy_priority={}, tracked_sessions={}",
            total_slots,
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
    ///   still has unmet, priority-distributed demand; keep allocating.
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
    use chrono::{DateTime, Duration, Utc};
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
        create_test_session_full(id, priority, pending, 0, 1, 1, 0, None, Utc::now())
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
        creation_time: DateTime<Utc>,
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
            creation_time,
            completion_time: None,
            state: SessionState::Open,
            min_instances,
            max_instances,
            batch_size,
            priority,
            resreq: None,
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

    /// Build a snapshot with the given sessions and a single node providing
    /// `total_slots` worth of capacity (1 slot = `unit`).
    fn create_snapshot_with_capacity(
        sessions: Vec<Arc<SessionInfo>>,
        total_slots: u32,
    ) -> SnapShot {
        let unit = ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        };
        let ss = SnapShot::new(unit.clone());
        ss.add_application(create_test_app("test-app")).unwrap();
        for ssn in sessions {
            ss.add_session(ssn).unwrap();
        }
        // Node with allocatable = total_slots × unit
        let node = Arc::new(NodeInfo {
            name: "node-1".to_string(),
            allocatable: ResourceRequirement {
                cpu: u64::from(total_slots) * unit.cpu,
                memory: u64::from(total_slots) * unit.memory,
                gpu: 0,
            },
            state: NodeState::Ready,
        });
        ss.add_node(node).unwrap();
        ss
    }

    /// Snapshot with no node — total_slots = 0. Use only for tests that don't
    /// rely on the cluster-capacity cap (e.g. blocking semantics).
    fn create_snapshot(sessions: Vec<Arc<SessionInfo>>) -> SnapShot {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
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
        total_slots: u32,
    ) -> SnapShot {
        let ss = create_snapshot_with_capacity(sessions, total_slots);
        let exec = Arc::new(ExecutorInfo {
            id: "exec-1".to_string(),
            node: "node-1".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
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
        let ss = create_snapshot_with_capacity(vec![ssn_high, ssn_low], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.max_needy_priority, 100);
        assert_eq!(plugin.ssn_priority.get("ssn-high"), Some(&100));
        assert_eq!(plugin.ssn_priority.get("ssn-low"), Some(&10));
    }

    #[test]
    fn test_setup_distribution_case1_cluster_has_slack() {
        // FS.md §3 Example Walkthrough Case 1.
        // total_slots=22, three sessions, all batch_size=1.
        //   A: priority=100, slots=2, pending=4 → demand=8,  granted=8,  remaining=14
        //   B: priority=100, slots=2, pending=3 → demand=6,  granted=6,  remaining=8
        //   C: priority=10,  slots=4, pending=5 → demand=20, granted=8,  remaining=0
        // Σ ssn_desired = 22 = total_slots.
        let t0 = Utc::now();
        let t1 = t0 + Duration::seconds(1);
        let t2 = t0 + Duration::seconds(2);

        let ssn_a = create_test_session_full("A", 100, 4, 0, 2, 1, 0, None, t0);
        let ssn_b = create_test_session_full("B", 100, 3, 0, 2, 1, 0, None, t1);
        let ssn_c = create_test_session_full("C", 10, 5, 0, 4, 1, 0, None, t2);
        let ss = create_snapshot_with_capacity(vec![ssn_a, ssn_b, ssn_c], 22);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("A").copied(), Some(8.0));
        assert_eq!(plugin.ssn_desired.get("B").copied(), Some(6.0));
        assert_eq!(plugin.ssn_desired.get("C").copied(), Some(8.0));
        let total: f64 = plugin.ssn_desired.values().sum();
        assert_eq!(total, 22.0);
        assert_eq!(plugin.max_needy_priority, 100);
    }

    #[test]
    fn test_setup_distribution_case2_cluster_contended() {
        // FS.md §3 Example Walkthrough Case 2.
        // total_slots=4, same sessions as Case 1.
        //   A: granted=min(8,4)=4 → remaining=0
        //   B: granted=min(6,0)=0
        //   C: granted=min(20,0)=0
        // Σ ssn_desired = 4 = total_slots.
        let t0 = Utc::now();
        let t1 = t0 + Duration::seconds(1);
        let t2 = t0 + Duration::seconds(2);

        let ssn_a = create_test_session_full("A", 100, 4, 0, 2, 1, 0, None, t0);
        let ssn_b = create_test_session_full("B", 100, 3, 0, 2, 1, 0, None, t1);
        let ssn_c = create_test_session_full("C", 10, 5, 0, 4, 1, 0, None, t2);
        let ss = create_snapshot_with_capacity(vec![ssn_a, ssn_b, ssn_c], 4);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("A").copied(), Some(4.0));
        assert_eq!(plugin.ssn_desired.get("B").copied(), Some(0.0));
        assert_eq!(plugin.ssn_desired.get("C").copied(), Some(0.0));
        let total: f64 = plugin.ssn_desired.values().sum();
        assert_eq!(total, 4.0);
        assert_eq!(plugin.max_needy_priority, 100);
    }

    #[test]
    fn test_setup_equal_priority_orders_by_creation_time() {
        // Two sessions at the same priority must be filled in creation_time ascending
        // order regardless of the order they were inserted into the snapshot or their
        // session IDs. total_slots=5 with two demands of 4 each → earlier gets 4,
        // later gets 1.
        let t_early = Utc::now();
        let t_late = t_early + Duration::seconds(60);

        // Insert "later-z" first, "earlier-a" second. Note the alphabetic id is
        // adversarial: if sort fell back to id, "earlier-a" would still win, so we
        // also flip the alphabetic order to "z-early" / "a-late" in a sibling test
        // below to be sure creation_time — not id — is the actual key.
        let ssn_late = create_test_session_full("z-early-id", 50, 4, 0, 1, 1, 0, None, t_late);
        let ssn_early = create_test_session_full("a-late-id", 50, 4, 0, 1, 1, 0, None, t_early);

        // Even though "a-late-id" sorts first alphabetically, its creation_time is
        // EARLIER, so it should still be filled first. (This confirms the sort
        // key is creation_time, not session id, when priorities tie.)
        let ss = create_snapshot_with_capacity(vec![ssn_late, ssn_early], 5);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        // Earlier session (a-late-id, t_early) is filled first → gets full demand=4.
        // Later session (z-early-id, t_late) absorbs the residual → gets 1.
        assert_eq!(plugin.ssn_desired.get("a-late-id").copied(), Some(4.0));
        assert_eq!(plugin.ssn_desired.get("z-early-id").copied(), Some(1.0));
    }

    #[test]
    fn test_setup_equal_priority_creation_time_wins_over_id() {
        // Mirror image of the above: earlier session has the alphabetically-LATER id.
        // Confirms creation_time alone determines fill order within a priority tier.
        let t_early = Utc::now();
        let t_late = t_early + Duration::seconds(60);

        let ssn_early = create_test_session_full("z-id", 50, 4, 0, 1, 1, 0, None, t_early);
        let ssn_late = create_test_session_full("a-id", 50, 4, 0, 1, 1, 0, None, t_late);
        let ss = create_snapshot_with_capacity(vec![ssn_early, ssn_late], 5);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("z-id").copied(), Some(4.0));
        assert_eq!(plugin.ssn_desired.get("a-id").copied(), Some(1.0));
    }

    #[test]
    fn test_setup_sum_desired_never_exceeds_total_slots() {
        // Regression test for the over-allocation bug: many high-demand sessions on
        // a small cluster must not collectively exceed total_slots.
        let total_slots: u32 = 22;
        let mut sessions = Vec::new();
        let base = Utc::now();
        for i in 0..10 {
            // priority varies, demand large (pending=50 × slots=2 = 100 each)
            let priority = (i as u32) * 10;
            let creation_time = base + Duration::seconds(i as i64);
            sessions.push(create_test_session_full(
                &format!("s-{i}"),
                priority,
                50,
                0,
                2,
                1,
                0,
                None,
                creation_time,
            ));
        }
        let ss = create_snapshot_with_capacity(sessions, total_slots);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        let sum: f64 = plugin.ssn_desired.values().sum();
        assert!(
            sum <= total_slots as f64,
            "Σ ssn_desired ({sum}) must not exceed total_slots ({total_slots})"
        );
        // With aggregate demand far exceeding capacity, equality must hold.
        assert_eq!(sum, total_slots as f64);
    }

    #[test]
    fn test_setup_compute_demand_batch_aligned_and_capped() {
        // Reach into compute_demand via a single-session snapshot.
        // pending=5, running=1, batch_size=2, slots=3, total_slots large enough
        // not to bind. task_count=6, batched=floor(6/2)*2=6, demand=6*3=18.
        let ssn = create_test_session_full("s", 0, 5, 1, 3, 2, 0, None, Utc::now());
        let ss = create_snapshot_with_capacity(vec![ssn], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(18.0));
        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(0.0));
    }

    #[test]
    fn test_setup_compute_demand_respects_max_instances() {
        // pending=10, slots=1, max_instances=4 → per-session demand capped at 4.
        // Cluster has plenty of slack so the per-session cap binds, not capacity.
        let ssn = create_test_session_full("s", 0, 10, 0, 1, 1, 0, Some(4), Utc::now());
        let ss = create_snapshot_with_capacity(vec![ssn], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(4.0));
    }

    #[test]
    fn test_setup_compute_demand_respects_min_instances() {
        // pending=0, min_instances=2, slots=3 → per-session demand floored at 6.
        let ssn = create_test_session_full("s", 0, 0, 0, 3, 1, 2, None, Utc::now());
        let ss = create_snapshot_with_capacity(vec![ssn], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(6.0));
    }

    #[test]
    fn test_setup_allocated_counts_existing_executors() {
        // Session with 1 executor (slots=2) already bound.
        let ssn = create_test_session_full("s", 0, 4, 0, 2, 1, 0, None, Utc::now());
        let ss = create_snapshot_with_executor(vec![ssn], "s", 2, 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_allocated.get("s").copied(), Some(2.0));
    }

    #[test]
    fn test_setup_no_pending_does_not_set_max_needy() {
        let ssn_high = create_test_session("ssn-high", 100, 0); // 0 pending
        let ssn_low = create_test_session("ssn-low", 10, 5);
        let ss = create_snapshot_with_capacity(vec![ssn_high, ssn_low], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.max_needy_priority, 10);
    }

    #[test]
    fn test_setup_all_same_priority_no_blocking() {
        let ssn_a = create_test_session("ssn-a", 0, 5);
        let ssn_b = create_test_session("ssn-b", 0, 3);
        let ss = create_snapshot_with_capacity(vec![ssn_a, ssn_b], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.max_needy_priority, 0);
    }

    #[test]
    fn test_setup_zero_total_slots_grants_zero() {
        // No nodes registered → total_slots=0; every session gets ssn_desired=0.
        let ssn = create_test_session_full("s", 100, 5, 0, 1, 1, 0, None, Utc::now());
        let ss = create_snapshot(vec![ssn]);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_desired.get("s").copied(), Some(0.0));
    }

    // ── ssn_order_fn() tests ─────────────────────────────────────────────────

    #[test]
    fn test_ssn_order_fn_different_priorities() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot_with_capacity(vec![ssn_high.clone(), ssn_low.clone()], 100);

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
        let ss = create_snapshot_with_capacity(vec![ssn_a.clone(), ssn_b.clone()], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.ssn_order_fn(&ssn_a, &ssn_b), None);
    }

    // ── is_underused() tests ─────────────────────────────────────────────────

    #[test]
    fn test_is_underused_lower_priority_blocked() {
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot_with_capacity(vec![ssn_high, ssn_low.clone()], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn_low), Some(false));
    }

    #[test]
    fn test_is_underused_high_priority_with_demand_returns_true() {
        // High-priority session with pending tasks and no executors yet → Some(true)
        let ssn_high = create_test_session("ssn-high", 100, 4);
        let ssn_low = create_test_session("ssn-low", 10, 4);
        let ss = create_snapshot_with_capacity(vec![ssn_high.clone(), ssn_low], 100);

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
        let ss = create_snapshot_with_executor(vec![ssn_high.clone(), ssn_low], "ssn-high", 1, 100);

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
        let ss = create_snapshot_with_capacity(vec![ssn_a.clone(), ssn_b.clone()], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn_a), Some(true));
        assert_eq!(plugin.is_underused(&ssn_b), Some(true));
    }

    #[test]
    fn test_is_underused_no_demand_defers() {
        // Session with no tasks (desired=0) → None regardless of priority
        let ssn = create_test_session("s", 100, 0);
        let ss = create_snapshot_with_capacity(vec![ssn.clone()], 100);

        let mut plugin = make_plugin();
        plugin.setup(&ss).unwrap();

        assert_eq!(plugin.is_underused(&ssn), None);
    }

    // ── allocation callback tests ────────────────────────────────────────────

    #[test]
    fn test_on_executor_allocate_increments_allocated() {
        let ssn = create_test_session_full("s", 0, 4, 0, 2, 1, 0, None, Utc::now());
        let ss = create_snapshot_with_capacity(vec![ssn.clone()], 100);

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
        let ssn = create_test_session_full("s", 0, 4, 0, 2, 1, 0, None, Utc::now());
        let ss = create_snapshot_with_executor(vec![ssn.clone()], "s", 2, 100);

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

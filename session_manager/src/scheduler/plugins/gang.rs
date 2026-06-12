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

use std::collections::HashMap;

use common::apis::{SessionID, TaskState};
use common::FlameError;

use crate::model::{ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShot};
use crate::scheduler::plugins::{Plugin, PluginPtr};

struct GangState {
    batch_size: u32,
    /// Number of incomplete (Pending + Running) tasks for this session, sampled once
    /// per scheduling cycle in `setup()`.  Used to compute the allocation threshold:
    /// `needed = div_ceil(incomplete_tasks, batch_size) * batch_size`.
    incomplete_tasks: u32,
    allocated: u32,
    pipelined: u32,
    bound: u32,
}

pub struct GangPlugin {
    ssn_state: HashMap<SessionID, GangState>,
}

impl GangPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(GangPlugin {
            ssn_state: HashMap::new(),
        })
    }
}

impl Plugin for GangPlugin {
    fn name(&self) -> &'static str {
        "gang"
    }

    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_state.clear();

        {
            let sessions = ss
                .sessions
                .lock()
                .map_err(|e| FlameError::Internal(format!("failed to lock sessions: {}", e)))?;

            for ssn in sessions.values() {
                // Count tasks that have not yet completed (Pending + Running).
                let mut incomplete_tasks: u32 = 0;
                for state in [TaskState::Pending, TaskState::Running] {
                    if let Some(c) = ssn.tasks_status.get(&state) {
                        incomplete_tasks = incomplete_tasks.saturating_add((*c).max(0) as u32);
                    }
                }

                self.ssn_state.insert(
                    ssn.id.clone(),
                    GangState {
                        batch_size: ssn.batch_size.max(1),
                        incomplete_tasks,
                        allocated: 0,
                        pipelined: 0,
                        bound: 0,
                    },
                );
            }
        }

        let executors = ss.find_executors(None)?;
        for exec in executors.values() {
            if let Some(ssn_id) = &exec.ssn_id {
                if let Some(state) = self.ssn_state.get_mut(ssn_id) {
                    state.allocated += 1;
                }
            }
        }

        Ok(())
    }

    /// Returns `true` when the cumulative executor count meets the allocation demand
    /// for this session's incomplete tasks.
    ///
    /// ```text
    /// needed = div_ceil(incomplete_tasks, batch_size) * batch_size
    /// total  = allocated + pipelined
    /// ready  = needed == 0 || total >= needed
    /// ```
    ///
    /// `needed` is the smallest multiple of `batch_size` ≥ `incomplete_tasks` (Pending +
    /// Running), sampled once per cycle in `setup()`.  Semantics:
    ///
    /// - `batch_size=1`, 5 tasks: `needed=5`.  AllocateAction allocates until 5 executors
    ///   exist, saturating all task slots in one scheduling cycle.
    /// - `batch_size=2`, 5 tasks: `needed=6` (ceil(5/2)×2).  6 executors created (3 gangs);
    ///   covers all 5 tasks with one executor left unbound until a new task arrives.
    /// - Duplicate-prevention: `batch_size=1`, 1 task, 1 Binding executor in snapshot →
    ///   `needed=1, total=1+0=1 → true` → pre-check skips → no second executor.
    fn is_ready(&self, ssn: &SessionInfoPtr) -> bool {
        let Some(state) = self.ssn_state.get(&ssn.id) else {
            return false;
        };
        let needed = state.incomplete_tasks.div_ceil(state.batch_size) * state.batch_size;
        let total = state.allocated + state.pipelined;
        needed == 0 || (needed > 0 && total >= needed)
    }

    /// Mirrors `is_ready` for the Dispatch path (bind instead of pipeline/allocate).
    ///
    /// ```text
    /// needed = div_ceil(incomplete_tasks, batch_size) * batch_size
    /// total  = allocated + bound
    /// ready  = needed == 0 || total >= needed
    /// ```
    fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> bool {
        let Some(state) = self.ssn_state.get(&ssn.id) else {
            return false;
        };
        let needed = state.incomplete_tasks.div_ceil(state.batch_size) * state.batch_size;
        let total = state.allocated + state.bound;
        needed == 0 || (needed > 0 && total >= needed)
    }

    fn on_executor_allocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined += 1;
        }
    }

    fn on_executor_unallocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined = state.pipelined.saturating_sub(1);
        }
    }

    fn on_executor_pipeline(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined += 1;
        }
    }

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.bound += 1;
        }
    }

    fn on_executor_discard(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined = state.pipelined.saturating_sub(1);
        }
    }

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.bound = state.bound.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExecutorInfo, NodeInfo, SessionInfo};
    use chrono::Utc;
    use common::apis::{ExecutorState, ResourceRequirement, SessionState, Shim, TaskState};
    use std::sync::Arc;

    /// Build a test session with `batch_size.max(1)` pending tasks so that
    /// `needed = (pending / batch_size) * batch_size == batch_size` for any
    /// batch_size — i.e. exactly 1 complete batch is always the expected demand.
    fn create_test_session(id: &str, batch_size: u32) -> SessionInfoPtr {
        create_test_session_with_pending(id, batch_size, batch_size.max(1) as i32)
    }

    fn create_test_session_with_pending(id: &str, batch_size: u32, pending: i32) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            tasks_status: HashMap::from([(TaskState::Pending, pending)]),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            batch_size,
            priority: 0,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            retry_count: 0,
        })
    }

    fn create_test_executor(id: &str, ssn_id: Option<&str>) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            },
            shim: Shim::Host,
            task_id: None,
            ssn_id: ssn_id.map(|s| s.to_string()),
            creation_time: Utc::now(),
            state: ExecutorState::Idle,
        })
    }

    fn create_test_node(name: &str) -> NodeInfoPtr {
        Arc::new(NodeInfo {
            name: name.to_string(),
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            state: common::apis::NodeState::Ready,
        })
    }

    #[test]
    fn test_is_fulfilled_batch_size_1() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_fulfilled(&ssn));

        let node = create_test_node("node-1");
        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn));
    }

    #[test]
    fn test_is_fulfilled_batch_size_2() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_fulfilled(&ssn));

        let node = create_test_node("node-1");
        plugin.on_session_bind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn));

        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn));
    }

    #[test]
    fn test_is_fulfilled_with_allocated() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let exec = create_test_executor("exec-1", Some("ssn-1"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_fulfilled(&ssn));

        let node = create_test_node("node-1");
        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn));
    }

    #[test]
    fn test_is_ready_batch_size_1() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_ready(&ssn));

        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node, ssn.clone());

        assert!(plugin.is_ready(&ssn));
    }

    #[test]
    fn test_is_ready_batch_size_2() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_ready(&ssn));

        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node.clone(), ssn.clone());

        assert!(!plugin.is_ready(&ssn));

        plugin.on_executor_allocate(node, ssn.clone());

        assert!(plugin.is_ready(&ssn));
    }

    #[test]
    fn test_is_ready_with_allocated() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let exec = create_test_executor("exec-1", Some("ssn-1"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_ready(&ssn));

        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node, ssn.clone());

        assert!(plugin.is_ready(&ssn));
    }

    #[test]
    fn test_on_pipeline_and_discard() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        let exec = create_test_executor("exec-1", None);
        plugin.on_executor_pipeline(exec.clone(), ssn.clone());

        assert!(!plugin.is_ready(&ssn));

        plugin.on_executor_pipeline(exec.clone(), ssn.clone());

        assert!(plugin.is_ready(&ssn));

        plugin.on_executor_discard(exec.clone(), ssn.clone());

        assert!(!plugin.is_ready(&ssn));

        plugin.on_executor_discard(exec, ssn.clone());

        assert!(!plugin.is_ready(&ssn));
    }

    #[test]
    fn test_on_bind_and_unbind() {
        let ss = SnapShot::new();

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        plugin.on_session_bind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn));

        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn));

        plugin.on_session_unbind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn));

        plugin.on_session_unbind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn));
    }

    /// With batch_size=1 and 1 pending task, a Binding executor already in the snapshot
    /// satisfies `needed=1`, so both `is_ready` and `is_fulfilled` return `Some(true)`.
    /// This prevents AllocateAction / DispatchAction from creating a duplicate executor
    /// while the existing one is still running `on_session_enter`.
    #[test]
    fn test_is_ready_and_fulfilled_with_binding_executor_batch_size_1() {
        let ss = SnapShot::new();

        // 1 pending task, batch_size=1 → needed = (1/1)*1 = 1
        let ssn = create_test_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        // Simulate a Binding executor: ssn_id is set (the scheduler bound it).
        let exec = create_test_executor("exec-1", Some("ssn-1"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();
        // incomplete_tasks=1, needed=1, allocated=1
        // is_ready:    total = 1 + 0 = 1 == needed(1)  → Some(true)
        // is_fulfilled: total = 1 + 0 = 1 == needed(1) → Some(true)

        assert!(
            plugin.is_ready(&ssn),
            "is_ready must be true: 1 task, 1 snapshot executor → demand satisfied"
        );
        assert!(
            plugin.is_fulfilled(&ssn),
            "is_fulfilled must be true: 1 task, 1 snapshot executor → demand satisfied"
        );
    }

    /// With batch_size=2, a partial snapshot (1 of 2 needed executors) must not signal
    /// "ready".  AllocateAction must add one more executor to complete the gang.
    #[test]
    fn test_is_ready_partial_batch_still_allocates() {
        let ss = SnapShot::new();

        // 2 pending tasks, batch_size=2 → needed = (2/2)*2 = 2
        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        // Only 1 of 2 required executors is already in snapshot.
        let exec = create_test_executor("exec-1", Some("ssn-1"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();
        // incomplete_tasks=2, needed=2, allocated=1
        // total = 1+0 = 1 → 1 != 2 → not ready

        assert!(
            !plugin.is_ready(&ssn),
            "is_ready must be false: only 1 of 2 needed executors allocated"
        );
        assert!(
            !plugin.is_fulfilled(&ssn),
            "is_fulfilled must be false: only 1 of 2 needed executors in snapshot"
        );

        // Allocate one more → total = 1+1 = 2 == needed(2) → ready
        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node, ssn.clone());
        assert!(
            plugin.is_ready(&ssn),
            "is_ready must be true once the 2nd executor completes the gang"
        );
    }

    /// Regression for the "only 1 executor ever" bug with batch_size=1 and multiple tasks.
    ///
    /// With 3 pending tasks and 1 snapshot executor: `needed=3`, `total=1 ≠ 3 → Some(false)`.
    /// AllocateAction's pre-check does NOT fire.  It then allocates executors until
    /// `total == needed`, creating all 3 needed executors in one scheduling cycle.
    #[test]
    fn test_is_ready_batch_size_1_allocates_all_needed_executors() {
        let ss = SnapShot::new();

        // 3 pending tasks, batch_size=1 → needed = (3/1)*1 = 3
        let ssn = create_test_session_with_pending("ssn-multi", 1, 3);
        ss.add_session(ssn.clone()).unwrap();

        // 1 executor already in snapshot (e.g. Binding from previous cycle).
        let exec = create_test_executor("exec-1", Some("ssn-multi"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();
        // incomplete_tasks=3, needed=3, allocated=1 → total=1 → 1 != 3 → Some(false)

        assert!(
            !plugin.is_ready(&ssn),
            "pre-check must not fire: need 3 executors, only 1 in snapshot"
        );

        // AllocateAction allocates executor 2 → total=1+1=2, still short.
        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node.clone(), ssn.clone());
        assert!(
            !plugin.is_ready(&ssn),
            "still not ready after 1 new allocation: total=2 ≠ needed=3"
        );

        // AllocateAction allocates executor 3 → total=1+2=3 == needed(3) → ready.
        plugin.on_executor_allocate(node, ssn.clone());
        assert!(
            plugin.is_ready(&ssn),
            "is_ready must be true once all 3 needed executors are allocated"
        );
    }
}

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
use std::collections::binary_heap::BinaryHeap;
use std::collections::HashMap;

use crate::model::{
    ExecutorInfo, ExecutorInfoPtr, NodeInfo, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot,
    ALL_APPLICATION, ALL_EXECUTOR, ALL_NODE, OPEN_SESSION,
};
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::apis::{ExecutorState, ResourceRequirement, SessionID, TaskState};
use common::FlameError;

#[derive(Default, Clone)]
struct SSNInfo {
    pub id: SessionID,
    pub slots: u32,
    pub desired: f64,
    pub deserved: f64,
    pub allocated: f64,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
}

impl Eq for SSNInfo {}

impl PartialEq<Self> for SSNInfo {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl PartialOrd<Self> for SSNInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SSNInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.deserved < other.deserved {
            return Ordering::Greater;
        }

        if self.deserved > other.deserved {
            return Ordering::Less;
        }

        Ordering::Equal
    }
}

struct NInfo {
    pub name: String,
    pub allocatable: u32,
    pub allocated: f64,
}

impl Eq for NInfo {}

impl PartialEq<Self> for NInfo {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl PartialOrd<Self> for NInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.allocated < other.allocated {
            return Ordering::Greater;
        }

        if self.allocated > other.allocated {
            return Ordering::Less;
        }

        Ordering::Equal
    }
}

pub struct FairShare {
    ssn_map: HashMap<SessionID, SSNInfo>,
    node_map: HashMap<String, NInfo>,
    unit: ResourceRequirement,
}

impl FairShare {
    pub fn new_ptr() -> PluginPtr {
        Box::new(FairShare {
            ssn_map: HashMap::new(),
            node_map: HashMap::new(),
            unit: ResourceRequirement::default(),
        })
    }
}

impl Plugin for FairShare {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.unit = ss.unit.clone();

        let open_ssns = ss.find_sessions(OPEN_SESSION)?;

        let apps = ss.find_applications(ALL_APPLICATION)?;

        tracing::debug!(
            "There are {} open sessions, {} applications.",
            open_ssns.len(),
            apps.len()
        );

        for ssn in open_ssns.values() {
            // Sum pending + running *task counts* (not slot-weighted yet). Gang / batch_size is
            // defined in terms of tasks per batch; rounding must happen on task counts first,
            // then multiply by slots. Otherwise e.g. batch_size=2, slots=2, 1 pending task gives
            // slot demand 2, and floor(2/2)*2 = 2 wrongly allocates a full batch.
            let mut task_count = 0.0;
            for state in [TaskState::Pending, TaskState::Running] {
                if let Some(d) = ssn.tasks_status.get(&state) {
                    task_count += *d as f64;
                }
            }

            if let Some(app) = apps.get(&ssn.application) {
                let batch_size = ssn.batch_size.max(1) as f64;
                let batched_tasks = (task_count / batch_size).floor() * batch_size;
                let mut desired = batched_tasks * ssn.slots as f64;

                tracing::debug!(
                    "Session <{}>: task_count={}, batched_tasks={}, batch_size={}, slots={}, desired_slots={}",
                    ssn.id,
                    task_count,
                    batched_tasks,
                    ssn.batch_size,
                    ssn.slots,
                    desired
                );

                // Cap desired by session's max_instances (already includes app limit from session creation)
                if let Some(max_instances) = ssn.max_instances {
                    desired = desired.min((max_instances * ssn.slots) as f64);
                }

                // Ensure desired is at least min_instances * slots (minimum guarantee)
                let min_allocation = (ssn.min_instances * ssn.slots) as f64;
                desired = desired.max(min_allocation);

                self.ssn_map.insert(
                    ssn.id.clone(),
                    SSNInfo {
                        id: ssn.id.clone(),
                        desired,
                        deserved: min_allocation,
                        slots: ssn.slots,
                        min_instances: ssn.min_instances,
                        max_instances: ssn.max_instances,
                        batch_size: ssn.batch_size.max(1),
                        ..SSNInfo::default()
                    },
                );
            } else {
                tracing::warn!(
                    "Application <{}> not found for session <{}>.",
                    ssn.application,
                    ssn.id
                );
            }
        }

        let mut remaining_slots = 0.0;

        let nodes = ss.find_nodes(ALL_NODE)?;
        for node in nodes.values() {
            let allocatable = node.allocatable.to_slots(&self.unit);
            remaining_slots += allocatable as f64;
            self.node_map.insert(
                node.name.clone(),
                NInfo {
                    name: node.name.clone(),
                    allocatable,
                    allocated: 0.0,
                },
            );
        }

        // Reserve slots for guaranteed minimums before fair distribution
        for ssn in self.ssn_map.values() {
            let min_allocation = (ssn.min_instances * ssn.slots) as f64;
            remaining_slots -= min_allocation;
        }

        let executors = ss.find_executors(ALL_EXECUTOR)?;
        for exe in executors.values() {
            if let Some(node) = self.node_map.get_mut(&exe.node) {
                node.allocated += exe.slots as f64;
            } else {
                tracing::warn!("Node <{}> not found for executor <{}>.", exe.node, exe.id);
            }

            // Go through all the executors here (VOID, IDLE, BOUND, BINDING, UNBINDING, RELEASING, RELEASED)
            // If the executor is related to a session, add the slots to the session.

            if let Some(ssn_id) = exe.ssn_id.clone() {
                if let Some(ssn) = self.ssn_map.get_mut(&ssn_id) {
                    ssn.allocated += ssn.slots as f64;
                }
            }
        }

        let mut underused = BinaryHeap::from_iter(self.ssn_map.values_mut());
        loop {
            if remaining_slots < 0.001 {
                break;
            }

            if underused.is_empty() {
                break;
            }

            let ssn = underused
                .pop()
                .expect("failed to pop session: loop guard ensures non-empty");

            let batch_unit = (ssn.batch_size * ssn.slots) as f64;

            if ssn.deserved >= ssn.desired {
                continue;
            }

            if remaining_slots < batch_unit {
                continue;
            }

            let allocation = batch_unit.min(ssn.desired - ssn.deserved);
            ssn.deserved += allocation;
            remaining_slots -= allocation;

            if ssn.deserved < ssn.desired {
                underused.push(ssn);
            }
        }

        if tracing::enabled!(tracing::Level::DEBUG) {
            for ssn in self.ssn_map.values() {
                tracing::debug!(
                    "Session <{}>: slots <{}>, desired <{}>, deserved <{}>, allocated <{}>.",
                    ssn.id,
                    ssn.slots,
                    ssn.desired,
                    ssn.deserved,
                    ssn.allocated
                )
            }
        }

        Ok(())
    }

    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let ss1 = self.ssn_map.get(&s1.id);
        let ss2 = self.ssn_map.get(&s2.id);

        if ss1.is_none() || ss2.is_none() {
            return None;
        }

        let ss1 = ss1.expect("failed to get session info for s1: checked non-None above");
        let ss2 = ss2.expect("failed to get session info for s2: checked non-None above");

        let left = ss1.allocated * ss2.deserved;
        let right = ss2.allocated * ss1.deserved;

        if left < right {
            return Some(Ordering::Greater);
        }

        if left > right {
            return Some(Ordering::Less);
        }

        Some(Ordering::Equal)
    }

    fn node_order_fn(&self, s1: &NodeInfo, s2: &NodeInfo) -> Option<Ordering> {
        let n1 = self.node_map.get(&s1.name);
        let n2 = self.node_map.get(&s2.name);

        if n1.is_none() || n2.is_none() {
            return None;
        }

        let n1 = n1.expect("failed to get node info for n1: checked non-None above");
        let n2 = n2.expect("failed to get node info for n2: checked non-None above");

        let left = n1.allocated * n2.allocatable as f64;
        let right = n2.allocated * n1.allocatable as f64;

        if left < right {
            return Some(Ordering::Greater);
        }

        if left > right {
            return Some(Ordering::Less);
        }

        Some(Ordering::Equal)
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        self.ssn_map.get(&ssn.id).map(|ssn_info| {
            let batch_unit = (ssn_info.batch_size * ssn_info.slots) as f64;
            // Use floor() for defensive floating-point handling at batch boundaries
            let allocated_batches = (ssn_info.allocated / batch_unit).floor();
            let deserved_batches = (ssn_info.deserved / batch_unit).floor();
            let is_underused = allocated_batches < deserved_batches;
            tracing::debug!(
                "Fairshare is_underused for session <{}>: allocated={}, deserved={}, batch_unit={} => {}",
                ssn.id,
                ssn_info.allocated,
                ssn_info.deserved,
                batch_unit,
                is_underused
            );
            is_underused
        })
    }

    fn is_preemptible(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        self.ssn_map
            .get(&ssn.id)
            .map(|ssn| ssn.allocated - ssn.slots as f64 >= ssn.deserved)
    }

    fn is_available(&self, exec: &ExecutorInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        Some(ssn.slots == exec.slots)
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        self.node_map.get(&node.name).map(|node_info| {
            let is_allocatable =
                node_info.allocated + ssn.slots as f64 <= node_info.allocatable as f64;
            tracing::debug!(
                "Fairshare is_allocatable for node <{}>: allocated={}, allocatable={}, ssn_slots={} => {}",
                node.name,
                node_info.allocated,
                node_info.allocatable,
                ssn.slots,
                is_allocatable
            );
            is_allocatable
        })
    }

    fn is_reclaimable(&self, exec: &ExecutorInfoPtr) -> Option<bool> {
        match exec.ssn_id.clone() {
            Some(ssn_id) => self
                .ssn_map
                .get(&ssn_id)
                .map(|ssn| ssn.allocated - ssn.slots as f64 >= ssn.deserved),
            None => Some(true),
        }
    }

    fn on_executor_allocate(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated += ssn.slots as f64;
        }
        if let Some(n) = self.node_map.get_mut(&node.name) {
            n.allocated += ssn.slots as f64;
        }
    }

    fn on_executor_unallocate(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated -= ssn.slots as f64;
        }
        if let Some(n) = self.node_map.get_mut(&node.name) {
            n.allocated -= ssn.slots as f64;
        }
    }

    fn on_executor_pipeline(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated += ssn.slots as f64;
        }
        if let Some(n) = self.node_map.get_mut(&exec.node) {
            n.allocated += ssn.slots as f64;
        }
    }

    fn on_executor_discard(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated -= ssn.slots as f64;
        }
        if let Some(n) = self.node_map.get_mut(&exec.node) {
            n.allocated -= ssn.slots as f64;
        }
    }

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated += ssn.slots as f64;
        }
    }

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated -= ssn.slots as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppInfo, NodeInfo, SessionInfo};
    use chrono::Utc;
    use common::apis::{NodeState, SessionState};
    use std::sync::Arc;

    fn create_test_session(
        id: &str,
        batch_size: u32,
        slots: u32,
        pending_tasks: i32,
        min_instances: u32,
        max_instances: Option<u32>,
    ) -> Arc<SessionInfo> {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            slots,
            tasks_status: HashMap::from([(TaskState::Pending, pending_tasks)]),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances,
            max_instances,
            batch_size,
        })
    }

    fn create_test_node(name: &str, cpu: u64, memory: u64) -> Arc<NodeInfo> {
        Arc::new(NodeInfo {
            name: name.to_string(),
            allocatable: ResourceRequirement { cpu, memory },
            state: NodeState::Ready,
        })
    }

    fn create_test_app(name: &str) -> Arc<AppInfo> {
        Arc::new(AppInfo {
            name: name.to_string(),
            shim: common::apis::Shim::Host,
            max_instances: 0,
            delay_release: chrono::Duration::zero(),
        })
    }

    fn create_snapshot_with_sessions_and_nodes(
        sessions: Vec<Arc<SessionInfo>>,
        nodes: Vec<Arc<NodeInfo>>,
        unit: ResourceRequirement,
    ) -> SnapShot {
        let ss = SnapShot::new(unit);
        ss.add_application(create_test_app("test-app")).unwrap();
        for ssn in sessions {
            ss.add_session(ssn).unwrap();
        }
        for node in nodes {
            ss.add_node(node).unwrap();
        }
        ss
    }

    #[test]
    fn test_batch_aligned_distribution_basic() {
        let unit = ResourceRequirement {
            cpu: 1,
            memory: 1024,
        };
        let ssn_a = create_test_session("ssn-a", 4, 1, 8, 0, None);
        let ssn_b = create_test_session("ssn-b", 1, 1, 4, 0, None);
        let node = create_test_node("node-1", 10, 10240);

        let ss = create_snapshot_with_sessions_and_nodes(
            vec![ssn_a.clone(), ssn_b.clone()],
            vec![node],
            unit,
        );

        let mut fairshare = FairShare {
            ssn_map: HashMap::new(),
            node_map: HashMap::new(),
            unit: ResourceRequirement::default(),
        };
        fairshare.setup(&ss).unwrap();

        let ssn_a_info = fairshare.ssn_map.get("ssn-a").unwrap();
        let ssn_b_info = fairshare.ssn_map.get("ssn-b").unwrap();

        assert_eq!(ssn_a_info.deserved, 4.0);
        assert_eq!(ssn_b_info.deserved, 4.0);
    }

    #[test]
    fn test_batch_aligned_distribution_with_min_instances() {
        let unit = ResourceRequirement {
            cpu: 1,
            memory: 1024,
        };
        let ssn_a = create_test_session("ssn-a", 4, 1, 8, 4, None);
        let node = create_test_node("node-1", 10, 10240);

        let ss = create_snapshot_with_sessions_and_nodes(vec![ssn_a.clone()], vec![node], unit);

        let mut fairshare = FairShare {
            ssn_map: HashMap::new(),
            node_map: HashMap::new(),
            unit: ResourceRequirement::default(),
        };
        fairshare.setup(&ss).unwrap();

        let ssn_a_info = fairshare.ssn_map.get("ssn-a").unwrap();

        assert!(ssn_a_info.deserved >= 4.0);
        assert_eq!(ssn_a_info.deserved % 4.0, 0.0);
    }

    #[test]
    fn test_is_underused_batch_aware() {
        let unit = ResourceRequirement {
            cpu: 1,
            memory: 1024,
        };
        let ssn = create_test_session("ssn-1", 4, 1, 8, 0, None);
        let node = create_test_node("node-1", 10, 10240);

        let ss = create_snapshot_with_sessions_and_nodes(vec![ssn.clone()], vec![node], unit);

        let mut fairshare = FairShare {
            ssn_map: HashMap::new(),
            node_map: HashMap::new(),
            unit: ResourceRequirement::default(),
        };
        fairshare.setup(&ss).unwrap();

        assert!(fairshare.is_underused(&ssn).unwrap());

        if let Some(info) = fairshare.ssn_map.get_mut("ssn-1") {
            info.allocated = 4.0;
        }

        assert!(fairshare.is_underused(&ssn).unwrap());

        if let Some(info) = fairshare.ssn_map.get_mut("ssn-1") {
            info.allocated = 8.0;
        }

        assert!(!fairshare.is_underused(&ssn).unwrap());
    }

    #[test]
    fn test_batch_size_1_backward_compatible() {
        let unit = ResourceRequirement {
            cpu: 1,
            memory: 1024,
        };
        let ssn_a = create_test_session("ssn-a", 1, 1, 7, 0, None);
        let ssn_b = create_test_session("ssn-b", 1, 1, 5, 0, None);
        let node = create_test_node("node-1", 10, 10240);

        let ss = create_snapshot_with_sessions_and_nodes(
            vec![ssn_a.clone(), ssn_b.clone()],
            vec![node],
            unit,
        );

        let mut fairshare = FairShare {
            ssn_map: HashMap::new(),
            node_map: HashMap::new(),
            unit: ResourceRequirement::default(),
        };
        fairshare.setup(&ss).unwrap();

        let ssn_a_info = fairshare.ssn_map.get("ssn-a").unwrap();
        let ssn_b_info = fairshare.ssn_map.get("ssn-b").unwrap();

        assert!(ssn_a_info.deserved > 0.0);
        assert!(ssn_b_info.deserved > 0.0);
        assert!((ssn_a_info.deserved + ssn_b_info.deserved - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_remaining_slots_not_enough_for_batch() {
        let unit = ResourceRequirement {
            cpu: 1,
            memory: 1024,
        };
        let ssn = create_test_session("ssn-1", 5, 1, 10, 0, None);
        let node = create_test_node("node-1", 8, 8192);

        let ss = create_snapshot_with_sessions_and_nodes(vec![ssn.clone()], vec![node], unit);

        let mut fairshare = FairShare {
            ssn_map: HashMap::new(),
            node_map: HashMap::new(),
            unit: ResourceRequirement::default(),
        };
        fairshare.setup(&ss).unwrap();

        let ssn_info = fairshare.ssn_map.get("ssn-1").unwrap();

        assert_eq!(ssn_info.deserved, 5.0);
    }
}

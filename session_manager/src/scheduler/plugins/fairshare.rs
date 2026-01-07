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
            let mut desired = 0.0;
            for state in [TaskState::Pending, TaskState::Running] {
                if let Some(d) = ssn.tasks_status.get(&state) {
                    desired += *d as f64 * ssn.slots as f64;
                }
            }

            if let Some(app) = apps.get(&ssn.application) {
                desired = desired.min((app.max_instances * ssn.slots) as f64);

                self.ssn_map.insert(
                    ssn.id.clone(),
                    SSNInfo {
                        id: ssn.id.clone(),
                        desired,
                        slots: ssn.slots,
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

            let delta = remaining_slots / underused.len() as f64;
            let ssn = underused.pop().unwrap();

            if ssn.deserved + delta < ssn.desired {
                ssn.deserved += delta;
                remaining_slots -= delta;
                underused.push(ssn);
            } else {
                remaining_slots -= ssn.desired - ssn.deserved;
                ssn.deserved = ssn.desired;
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

        let ss1 = ss1.unwrap();
        let ss2 = ss2.unwrap();

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

        let n1 = n1.unwrap();
        let n2 = n2.unwrap();

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
        self.ssn_map
            .get(&ssn.id)
            .map(|ssn| ssn.allocated < ssn.deserved)
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
        self.node_map
            .get(&node.name)
            .map(|node| node.allocated + ssn.slots as f64 <= node.allocatable as f64)
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

    fn on_create_executor(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(ss) = self.ssn_map.get_mut(&ssn.id) {
            ss.allocated += ssn.slots as f64;
        } else {
            tracing::warn!("Session <{}> not found for node <{}>.", ssn.id, node.name);
        }

        if let Some(node) = self.node_map.get_mut(&node.name) {
            node.allocated += ssn.slots as f64;
        } else {
            tracing::warn!("Node <{}> not found for session <{}>.", node.name, ssn.id);
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

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

use crate::model::{
    ExecutorInfoPtr, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot, ALL_EXECUTOR, ALL_NODE,
    OPEN_SESSION,
};
use crate::scheduler::plugins::{compute_demand, Plugin, PluginPtr};
use common::apis::{ResourceRequirement, SessionID};
use common::FlameError;

/// Dominant Resource Fairness: sessions are ordered by their
/// dominant share, and a session holding more than its desired share is
/// re-balanced one executor at a time. Demand and instance bounds belong to MinMax.
#[derive(Default, Clone)]
pub(crate) struct DRFSessionInfo {
    pub unit: ResourceRequirement,
    pub demand: u32,
    pub min_instances: u32,
    pub allocated: ResourceRequirement,
    pub pipelined: ResourceRequirement,
    pub dominant: f64,
    /// Dominant share this session is entitled to: its demand, capped by a
    /// max-min fair split of the cluster among the sessions that want capacity.
    pub desired: f64,
}

#[derive(Default)]
pub struct DRFPlugin {
    pub(crate) total: ResourceRequirement,
    pub(crate) ssn_map: HashMap<SessionID, DRFSessionInfo>,
    pub(crate) node_allocations: HashMap<String, ResourceRequirement>,
}

impl DRFPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(DRFPlugin::default())
    }

    pub(crate) fn calculate_dominant_share(&self, allocated: &ResourceRequirement) -> f64 {
        let mut dominant = 0.0_f64;
        for (used, total) in [
            (allocated.cpu as f64, self.total.cpu as f64),
            (allocated.memory as f64, self.total.memory as f64),
            (allocated.gpu.max(0) as f64, self.total.gpu.max(0) as f64),
        ] {
            if total > 0.0 {
                dominant = dominant.max(used / total);
            } else if used > 0.0 {
                dominant = f64::MAX;
            }
        }
        dominant
    }

    fn update_share(&mut self, session_id: &SessionID) {
        let Some(allocated) = self.ssn_map.get(session_id).map(|i| i.allocated.clone()) else {
            return;
        };
        let share = self.calculate_dominant_share(&allocated);
        if let Some(info) = self.ssn_map.get_mut(session_id) {
            info.dominant = share;
        }
    }

    /// Dominant share of the session's whole demand; zero when the session
    /// requests no resource the cluster has, which DRF cannot divide.
    fn desire(&self, info: &DRFSessionInfo) -> f64 {
        let unit = self.calculate_dominant_share(&info.unit);
        if unit > 0.0 && unit < f64::MAX {
            unit * info.demand as f64
        } else {
            0.0
        }
    }

    /// Max-min fair division of the cluster's dominant share, capped by each
    /// session's demand. Demand is fixed for the cycle, so this runs at setup.
    fn update_desired(&mut self) {
        let mut wants = self
            .ssn_map
            .iter()
            .map(|(id, info)| (id.clone(), self.desire(info)))
            .collect::<Vec<_>>();
        wants.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let mut remaining = 1.0;
        let mut left = wants.iter().filter(|(_, want)| *want > 0.0).count();
        for (id, want) in wants {
            let desired = if want > 0.0 {
                let desired = want.min(remaining / left as f64);
                remaining -= desired;
                left -= 1;
                desired
            } else {
                0.0
            };
            if let Some(info) = self.ssn_map.get_mut(&id) {
                info.desired = desired;
            }
        }
    }

    /// After the slots-cleanup refactor, every session's `resreq` is populated
    /// by `resolve_session_resreq` server-side, so this is just a `.clone()` of
    /// the explicit value. The `expect` documents that invariant.
    fn get_session_resreq(&self, ssn: &SessionInfoPtr) -> ResourceRequirement {
        ssn.resreq
            .clone()
            .expect("SessionInfo.resreq must be populated by resolve_session_resreq")
    }

    fn release(allocated: &mut ResourceRequirement, unit: &ResourceRequirement) {
        allocated.cpu = allocated.cpu.saturating_sub(unit.cpu);
        allocated.memory = allocated.memory.saturating_sub(unit.memory);
        allocated.gpu = allocated.gpu.saturating_sub(unit.gpu);
    }
}

impl Plugin for DRFPlugin {
    fn name(&self) -> &'static str {
        "drf"
    }

    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_map.clear();
        self.node_allocations.clear();
        self.total = ResourceRequirement::default();

        let nodes = ss.find_nodes(ALL_NODE)?;
        for node in nodes.values() {
            self.total.cpu += node.allocatable.cpu;
            self.total.memory += node.allocatable.memory;
            self.total.gpu += node.allocatable.gpu;
            self.node_allocations
                .insert(node.name.clone(), ResourceRequirement::default());
        }

        let executors = ss.find_executors(ALL_EXECUTOR)?;
        let mut allocated: HashMap<SessionID, ResourceRequirement> = HashMap::new();
        for exec in executors.values() {
            // Creation reserves node capacity, including executors still starting
            // or departing. Pipelining existing capacity must not reserve it again.
            if exec.state != common::apis::ExecutorState::Released {
                if let Some(node_alloc) = self.node_allocations.get_mut(&exec.node) {
                    node_alloc.add(&exec.resreq);
                }
            }

            let Some(ssn_id) = &exec.ssn_id else {
                continue;
            };
            if exec.is_binding(ssn_id) && !exec.is_interrupted() {
                allocated
                    .entry(ssn_id.clone())
                    .or_default()
                    .add(&exec.resreq);
            }
        }

        for ssn in ss.find_sessions(OPEN_SESSION)?.values() {
            self.ssn_map.insert(
                ssn.id.clone(),
                DRFSessionInfo {
                    unit: self.get_session_resreq(ssn),
                    demand: compute_demand(ssn),
                    min_instances: ssn.min_instances,
                    allocated: allocated.remove(&ssn.id).unwrap_or_default(),
                    ..Default::default()
                },
            );
            self.update_share(&ssn.id);
        }
        self.update_desired();

        Ok(())
    }

    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let share = |ssn: &SessionInfo| self.ssn_map.get(&ssn.id).map_or(0.0, |i| i.dominant);
        // The scheduler's session queue is a max-heap on this order, so the
        // session with the smaller share must compare greater to be served first.
        share(s2).partial_cmp(&share(s1))
    }

    /// A session is overused when it would still hold its desired share after
    /// releasing one executor, so it never drops below what it is entitled to
    /// or below its `min_instances`.
    fn is_overused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let info = self.ssn_map.get(&ssn.id)?;
        if info.demand > 0 && self.desire(info) == 0.0 {
            // Requests nothing DRF can divide; MinMax's demand rule applies.
            return None;
        }
        if info.dominant == 0.0 {
            return None;
        }
        let mut remaining = info.allocated.clone();
        Self::release(&mut remaining, &info.unit);
        let kept = self.calculate_dominant_share(&remaining);
        let floor = info
            .desired
            .max(self.calculate_dominant_share(&info.unit.mul(info.min_instances)));
        (kept >= floor || (kept - floor).abs() <= 1e-9 * kept.max(floor)).then_some(true)
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        let node_alloc = self.node_allocations.get(&node.name)?;
        let ssn_resreq = self.get_session_resreq(ssn);

        let cpu_ok = node_alloc.cpu + ssn_resreq.cpu <= node.allocatable.cpu;
        let mem_ok = node_alloc.memory + ssn_resreq.memory <= node.allocatable.memory;
        let gpu_ok = if ssn_resreq.gpu > 0 {
            node_alloc.gpu + ssn_resreq.gpu <= node.allocatable.gpu
        } else {
            true
        };

        Some(cpu_ok && mem_ok && gpu_ok)
    }

    fn on_executor_create(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.allocated.add(&exec.resreq);
        }
        if let Some(node_alloc) = self.node_allocations.get_mut(&exec.node) {
            node_alloc.add(&exec.resreq);
        }
        self.update_share(&ssn.id);
    }

    fn on_executor_pipeline(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.pipelined.add(&exec.resreq);
        }
    }

    fn on_session_bind(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        let unit = self.get_session_resreq(&ssn);
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.allocated.add(&unit);
        }
        self.update_share(&ssn.id);
    }

    fn on_executor_interrupt(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            Self::release(&mut entry.allocated, &exec.resreq);
        }
        self.update_share(&ssn.id);
    }

    fn on_session_unbind(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        let unit = self.get_session_resreq(&ssn);
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            Self::release(&mut entry.allocated, &unit);
        }
        self.update_share(&ssn.id);
    }
}

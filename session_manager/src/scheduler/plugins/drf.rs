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
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::apis::{ResourceRequirement, SessionID, TaskState};
use common::FlameError;

#[derive(Default, Clone)]
struct DRFSessionInfo {
    pub id: SessionID,
    pub allocated: ResourceRequirement,
    pub pipelined: ResourceRequirement,
    pub dominant_share: f64,
}

pub struct DRFPlugin {
    total: ResourceRequirement,
    ssn_map: HashMap<SessionID, DRFSessionInfo>,
    node_allocations: HashMap<String, ResourceRequirement>,
}

impl DRFPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(DRFPlugin {
            total: ResourceRequirement::default(),
            ssn_map: HashMap::new(),
            node_allocations: HashMap::new(),
        })
    }

    fn calculate_dominant_share(&self, allocated: &ResourceRequirement) -> f64 {
        let mut dominant = 0.0_f64;

        if self.total.cpu > 0 {
            let share = allocated.cpu as f64 / self.total.cpu as f64;
            dominant = dominant.max(share);
        }

        if self.total.memory > 0 {
            let share = allocated.memory as f64 / self.total.memory as f64;
            dominant = dominant.max(share);
        }

        if self.total.gpu > 0 {
            let share = allocated.gpu as f64 / self.total.gpu as f64;
            dominant = dominant.max(share);
        } else if allocated.gpu > 0 {
            dominant = f64::MAX;
        }

        dominant
    }

    /// After the slots-cleanup refactor, every session's `resreq` is populated
    /// by `resolve_session_resreq` server-side, so this is just a `.clone()` of
    /// the explicit value. The `expect` documents that invariant.
    fn get_session_resreq(&self, ssn: &SessionInfoPtr) -> ResourceRequirement {
        ssn.resreq
            .clone()
            .expect("SessionInfo.resreq must be populated by resolve_session_resreq")
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

        tracing::debug!(
            "[DRF] Total cluster resources: cpu={}, memory={}, gpu={}",
            self.total.cpu,
            self.total.memory,
            self.total.gpu
        );

        let executors = ss.find_executors(ALL_EXECUTOR)?;
        for exec in executors.values() {
            if let Some(ssn_id) = &exec.ssn_id {
                if let Some(node_alloc) = self.node_allocations.get_mut(&exec.node) {
                    node_alloc.add(&exec.resreq);
                }
                let entry = self
                    .ssn_map
                    .entry(ssn_id.clone())
                    .or_insert_with(|| DRFSessionInfo {
                        id: ssn_id.clone(),
                        ..Default::default()
                    });
                entry.allocated.cpu += exec.resreq.cpu;
                entry.allocated.memory += exec.resreq.memory;
                entry.allocated.gpu += exec.resreq.gpu;
            }
        }

        let sessions = ss.find_sessions(OPEN_SESSION)?;
        for ssn in sessions.values() {
            let ssn_id = ssn.id.clone();
            let _ = self
                .ssn_map
                .entry(ssn_id.clone())
                .or_insert_with(|| DRFSessionInfo {
                    id: ssn.id.clone(),
                    ..Default::default()
                });
            let allocated_clone = self
                .ssn_map
                .get(&ssn_id)
                .map(|e| e.allocated.clone())
                .unwrap_or_default();
            let dominant_share = self.calculate_dominant_share(&allocated_clone);
            if let Some(entry) = self.ssn_map.get_mut(&ssn_id) {
                entry.dominant_share = dominant_share;
            }

            tracing::debug!(
                "[DRF] Session <{}>: allocated=({},{},{}), dominant_share={:.4}",
                ssn.id,
                allocated_clone.cpu,
                allocated_clone.memory,
                allocated_clone.gpu,
                dominant_share
            );
        }

        Ok(())
    }

    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let ds1 = self
            .ssn_map
            .get(&s1.id)
            .map(|s| s.dominant_share)
            .unwrap_or(0.0);
        let ds2 = self
            .ssn_map
            .get(&s2.id)
            .map(|s| s.dominant_share)
            .unwrap_or(0.0);

        ds1.partial_cmp(&ds2)
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let pending = ssn
            .tasks_status
            .get(&TaskState::Pending)
            .copied()
            .unwrap_or(0);

        if pending == 0 {
            return Some(false);
        }

        let ssn_info = self.ssn_map.get(&ssn.id)?;
        let num_sessions = self.ssn_map.len().max(1) as f64;
        let fair_share = 1.0 / num_sessions;

        Some(ssn_info.dominant_share < fair_share)
    }

    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let ssn_info = self.ssn_map.get(&ssn.id)?;
        let mut supplied = ssn_info.allocated.clone();
        supplied.add(&ssn_info.pipelined);
        let num_sessions = self.ssn_map.len().max(1) as f64;
        Some(self.calculate_dominant_share(&supplied) >= 1.0 / num_sessions)
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

    fn on_executor_pipeline(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.pipelined.add(&exec.resreq);
        }

        // Void executors are deliberately excluded from setup's node usage.
        // Pipeline them here so they reserve capacity exactly once for the
        // current cycle. Executors already owned by a session were counted by
        // setup and must not be charged again while they unbind.
        if exec.ssn_id.is_none() {
            if let Some(node_alloc) = self.node_allocations.get_mut(&exec.node) {
                node_alloc.add(&exec.resreq);
            }
        }
    }

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {
        let ssn_resreq = self.get_session_resreq(&ssn);

        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.allocated.cpu += ssn_resreq.cpu;
            entry.allocated.memory += ssn_resreq.memory;
            entry.allocated.gpu += ssn_resreq.gpu;
        }
        if let Some(entry) = self.ssn_map.get(&ssn.id) {
            let ds = self.calculate_dominant_share(&entry.allocated);
            if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
                entry.dominant_share = ds;
            }
        }
    }

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        let ssn_resreq = self.get_session_resreq(&ssn);

        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.allocated.cpu = entry.allocated.cpu.saturating_sub(ssn_resreq.cpu);
            entry.allocated.memory = entry.allocated.memory.saturating_sub(ssn_resreq.memory);
            entry.allocated.gpu = entry.allocated.gpu.saturating_sub(ssn_resreq.gpu);
        }
        if let Some(entry) = self.ssn_map.get(&ssn.id) {
            let ds = self.calculate_dominant_share(&entry.allocated);
            if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
                entry.dominant_share = ds;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;

    use common::apis::{ExecutorState, SessionState, Shim};

    fn rr(cpu: u64) -> ResourceRequirement {
        ResourceRequirement {
            cpu,
            memory: cpu,
            gpu: 0,
        }
    }

    fn session() -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: "session-1".to_string(),
            application: "app".to_string(),
            tasks_status: [(TaskState::Pending, 1)].into_iter().collect(),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: Some(rr(1)),
            retry_count: 0,
        })
    }

    fn void_executor(id: &str) -> ExecutorInfoPtr {
        Arc::new(crate::model::ExecutorInfo {
            id: id.to_string(),
            node: "node-1".to_string(),
            resreq: rr(1),
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Void,
        })
    }

    #[test]
    fn pipeline_accounts_void_capacity_without_changing_allocated_ownership() {
        let ssn = session();
        let mut plugin = DRFPlugin {
            total: rr(4),
            ssn_map: HashMap::from([(
                ssn.id.clone(),
                DRFSessionInfo {
                    id: ssn.id.clone(),
                    ..Default::default()
                },
            )]),
            node_allocations: HashMap::from([("node-1".to_string(), rr(0))]),
        };

        for i in 0..4 {
            plugin.on_executor_pipeline(void_executor(&format!("exec-{i}")), ssn.clone());
        }

        let info = plugin.ssn_map.get(&ssn.id).unwrap();
        assert_eq!(info.allocated, rr(0));
        assert_eq!(info.pipelined, rr(4));
        assert_eq!(plugin.node_allocations.get("node-1"), Some(&rr(4)));
        assert_eq!(plugin.is_underused(&ssn), Some(true));
        assert_eq!(plugin.is_ready(&ssn), Some(true));
    }
}

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
            if !exec.is_binding(ssn_id) || exec.is_interrupted() {
                continue;
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

        let sessions = ss.find_sessions(OPEN_SESSION)?;
        self.ssn_map.retain(|id, _| sessions.contains_key(id));
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

    fn is_overused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let info = self.ssn_map.get(&ssn.id)?;
        let minimum = self.get_session_resreq(ssn).mul(ssn.min_instances);
        let minimum_share = self.calculate_dominant_share(&minimum);
        let fair_share = 1.0 / self.ssn_map.len().max(1) as f64;
        (info.dominant_share > fair_share.max(minimum_share)).then_some(true)
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
        if let Some(entry) = self.ssn_map.get(&ssn.id) {
            let share = self.calculate_dominant_share(&entry.allocated);
            if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
                entry.dominant_share = share;
            }
        }
    }

    fn on_executor_pipeline(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Reserve a future share while waiting for existing capacity to be released.
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.pipelined.add(&exec.resreq);
        }
    }

    fn on_session_bind(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
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

    fn on_executor_interrupt(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
            entry.allocated.cpu = entry.allocated.cpu.saturating_sub(exec.resreq.cpu);
            entry.allocated.memory = entry.allocated.memory.saturating_sub(exec.resreq.memory);
            entry.allocated.gpu = entry.allocated.gpu.saturating_sub(exec.resreq.gpu);
        }
        if let Some(entry) = self.ssn_map.get(&ssn.id) {
            let share = self.calculate_dominant_share(&entry.allocated);
            if let Some(entry) = self.ssn_map.get_mut(&ssn.id) {
                entry.dominant_share = share;
            }
        }
    }

    fn on_session_unbind(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
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
            batch_size: 1,
            id: "session-1".to_string(),
            application: "app".to_string(),
            tasks_status: [(TaskState::Pending, 1)].into_iter().collect(),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            priority: 0,
            resreq: Some(rr(1)),
            retry_count: 0,
            task_index: HashMap::new(),
        })
    }

    fn void_executor(id: &str) -> ExecutorInfoPtr {
        Arc::new(crate::model::ExecutorInfo {
            id: id.to_string(),
            node: "node-1".to_string(),
            resreq: rr(1),
            shim: Shim::Host,
            application: String::new(),
            task_id: None,
            signal: None,
            ssn_id: None,
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::Void,
            attributes: Default::default(),
        })
    }

    #[test]
    fn overuse_balances_each_dominant_resource_and_preserves_minimum() {
        for resource in [
            ResourceRequirement {
                cpu: 1,
                ..Default::default()
            },
            ResourceRequirement {
                memory: 1,
                ..Default::default()
            },
            ResourceRequirement {
                gpu: 1,
                ..Default::default()
            },
        ] {
            let ssn = Arc::new(SessionInfo {
                min_instances: 2,
                tasks_status: [(TaskState::Pending, 10)].into_iter().collect(),
                resreq: Some(resource.clone()),
                ..session().as_ref().clone()
            });
            let mut total = resource.clone();
            total.add(&resource);
            total.add(&resource);
            total.add(&resource);
            let mut allocated = resource.clone();
            allocated.add(&resource);
            allocated.add(&resource);
            let mut plugin = DRFPlugin {
                total,
                ssn_map: HashMap::from([
                    (
                        ssn.id.clone(),
                        DRFSessionInfo {
                            allocated,
                            ..Default::default()
                        },
                    ),
                    ("other".into(), DRFSessionInfo::default()),
                ]),
                node_allocations: HashMap::new(),
            };
            plugin.ssn_map.get_mut(&ssn.id).unwrap().dominant_share = 0.75;
            assert_eq!(plugin.is_overused(&ssn), Some(true));
            let interrupted = Arc::new(crate::model::ExecutorInfo {
                resreq: resource.clone(),
                ..void_executor("interrupted").as_ref().clone()
            });
            plugin.on_executor_interrupt(interrupted.clone(), ssn.clone());
            assert_eq!(plugin.is_overused(&ssn), None);
            plugin.on_session_bind(interrupted, ssn.clone());
            assert_eq!(plugin.is_overused(&ssn), Some(true));
            plugin.on_session_unbind(void_executor("one"), ssn.clone());
            assert_eq!(plugin.is_overused(&ssn), None);
            let minimum = Arc::new(SessionInfo {
                min_instances: 3,
                ..ssn.as_ref().clone()
            });
            plugin.on_session_bind(void_executor("one"), ssn.clone());
            assert_eq!(plugin.is_overused(&minimum), None);
            plugin.ssn_map.remove("other");
            assert_eq!(plugin.is_overused(&ssn), None);
        }
    }

    #[test]
    fn overuse_excludes_departures_and_closed_sessions_after_refresh() {
        use crate::scheduler::plugins::{PluginManager, PluginsOptions};

        let snapshot = SnapShot::new();
        snapshot
            .add_node(Arc::new(crate::model::NodeInfo {
                name: "node-1".into(),
                allocatable: rr(4),
                state: common::apis::NodeState::Ready,
            }))
            .unwrap();
        let ssn = Arc::new(SessionInfo {
            tasks_status: [(TaskState::Pending, 10)].into_iter().collect(),
            ..session().as_ref().clone()
        });
        snapshot.add_session(ssn.clone()).unwrap();
        snapshot
            .add_session(Arc::new(SessionInfo {
                id: "other".into(),
                ..ssn.as_ref().clone()
            }))
            .unwrap();
        snapshot
            .add_session(Arc::new(SessionInfo {
                id: "closed".into(),
                state: SessionState::Closed,
                ..ssn.as_ref().clone()
            }))
            .unwrap();
        for (id, owner, state) in [
            ("one", ssn.id.as_str(), ExecutorState::Bound),
            ("two", ssn.id.as_str(), ExecutorState::Bound),
            ("three", ssn.id.as_str(), ExecutorState::Binding),
            ("departing", ssn.id.as_str(), ExecutorState::Unbinding),
            ("closed-executor", "closed", ExecutorState::Bound),
        ] {
            snapshot
                .add_executor(Arc::new(crate::model::ExecutorInfo {
                    ssn_id: Some(owner.into()),
                    state,
                    ..void_executor(id).as_ref().clone()
                }))
                .unwrap();
        }
        let options = PluginsOptions {
            policies: vec!["drf".into()],
            ..Default::default()
        };
        let manager = PluginManager::setup(&snapshot, &options).unwrap();
        // MinMax abstains because demand exceeds three active bindings; DRF approves.
        assert!(manager.is_overused(&ssn).unwrap());
        let departing = snapshot.find_executors(ALL_EXECUTOR).unwrap()["one"].clone();
        manager
            .on_executor_interrupt(departing.clone(), ssn.clone())
            .unwrap();
        assert!(!manager.is_overused(&ssn).unwrap());
        snapshot
            .update_executor_state(departing, ExecutorState::Unbinding)
            .unwrap();
        let refreshed = PluginManager::setup(&snapshot, &options).unwrap();
        assert!(!refreshed.is_overused(&ssn).unwrap());
        // A deferred interrupt must exclude the same allocation before the
        // worker has transitioned its lifecycle state to Unbinding.
        let original = snapshot.find_executors(ALL_EXECUTOR).unwrap()["one"].clone();
        snapshot
            .update_executor(Arc::new(crate::model::ExecutorInfo {
                state: ExecutorState::Bound,
                signal: Some(common::apis::Signal::Interrupt),
                ..original.as_ref().clone()
            }))
            .unwrap();
        let interrupted = PluginManager::setup(&snapshot, &options).unwrap();
        assert!(!interrupted.is_overused(&ssn).unwrap());
    }

    #[test]
    fn interrupted_executor_capacity_is_accounted_without_session_share() {
        let snapshot = SnapShot::new();
        let node = Arc::new(crate::model::NodeInfo {
            name: "node-1".into(),
            allocatable: rr(1),
            state: common::apis::NodeState::Ready,
        });
        snapshot.add_node(node.clone()).unwrap();
        let ssn = session();
        snapshot.add_session(ssn.clone()).unwrap();
        snapshot
            .add_executor(Arc::new(crate::model::ExecutorInfo {
                state: ExecutorState::Bound,
                signal: Some(common::apis::Signal::Interrupt),
                ssn_id: Some(ssn.id.clone()),
                ..void_executor("interrupted").as_ref().clone()
            }))
            .unwrap();
        let mut plugin = DRFPlugin {
            total: rr(0),
            ssn_map: HashMap::new(),
            node_allocations: HashMap::new(),
        };
        plugin.setup(&snapshot).unwrap();
        assert_eq!(snapshot.find_executors(ALL_EXECUTOR).unwrap().len(), 1);
        assert_eq!(plugin.node_allocations[&node.name], rr(1));
        assert_eq!(plugin.ssn_map[&ssn.id].allocated, rr(0));
        assert_eq!(plugin.is_allocatable(&node, &ssn), Some(false));
    }

    #[test]
    fn readiness_preserves_fair_share_ceiling_when_demand_is_larger() {
        let session = Arc::new(SessionInfo {
            tasks_status: [(TaskState::Pending, 10)].into_iter().collect(),
            ..session().as_ref().clone()
        });
        let plugin = DRFPlugin {
            total: rr(4),
            ssn_map: HashMap::from([
                (
                    session.id.clone(),
                    DRFSessionInfo {
                        allocated: rr(2),
                        ..Default::default()
                    },
                ),
                ("other-session".into(), DRFSessionInfo::default()),
            ]),
            node_allocations: HashMap::new(),
        };
        assert_eq!(plugin.is_ready(&session), Some(true));
    }

    #[test]
    fn creation_allocates_capacity_without_pipelining() {
        let ssn = session();
        let mut plugin = DRFPlugin {
            total: rr(4),
            ssn_map: HashMap::from([(ssn.id.clone(), DRFSessionInfo::default())]),
            node_allocations: HashMap::from([("node-1".into(), rr(0))]),
        };
        for i in 0..4 {
            plugin.on_executor_create(void_executor(&format!("new-{i}")), ssn.clone());
        }
        let info = &plugin.ssn_map[&ssn.id];
        assert_eq!(info.allocated, rr(4));
        assert_eq!(info.pipelined, rr(0));
        assert_eq!(info.dominant_share, 1.0);
        assert_eq!(plugin.node_allocations["node-1"], rr(4));
        assert_eq!(plugin.is_underused(&ssn), Some(false));
        assert_eq!(plugin.is_ready(&ssn), Some(true));
    }

    #[test]
    fn pipeline_reserves_future_share_without_allocating_more_node_capacity() {
        let snapshot = SnapShot::new();
        snapshot
            .add_node(Arc::new(crate::model::NodeInfo {
                name: "node-1".into(),
                allocatable: rr(4),
                state: common::apis::NodeState::Ready,
            }))
            .unwrap();
        let ssn = session();
        snapshot.add_session(ssn.clone()).unwrap();
        let mut departing = Vec::new();
        for i in 0..4 {
            let exec = Arc::new(crate::model::ExecutorInfo {
                state: ExecutorState::Unbinding,
                ..void_executor(&format!("departing-{i}")).as_ref().clone()
            });
            snapshot.add_executor(exec.clone()).unwrap();
            departing.push(exec);
        }
        let mut plugin = DRFPlugin {
            total: rr(0),
            ssn_map: HashMap::new(),
            node_allocations: HashMap::new(),
        };
        plugin.setup(&snapshot).unwrap();
        assert_eq!(plugin.node_allocations["node-1"], rr(4));
        for exec in departing {
            plugin.on_executor_pipeline(exec, ssn.clone());
        }
        let info = &plugin.ssn_map[&ssn.id];
        assert_eq!(info.allocated, rr(0));
        assert_eq!(info.pipelined, rr(4));
        assert_eq!(info.dominant_share, 0.0);
        assert_eq!(plugin.node_allocations["node-1"], rr(4));
        assert_eq!(plugin.is_underused(&ssn), Some(true));
        assert_eq!(plugin.is_ready(&ssn), Some(true));
    }

    #[test]
    fn retained_idle_executor_blocks_cross_application_overcommit() {
        let snapshot = SnapShot::new();
        let node = Arc::new(crate::model::NodeInfo {
            name: "node-1".to_string(),
            allocatable: rr(1),
            state: common::apis::NodeState::Ready,
        });
        snapshot.add_node(node.clone()).unwrap();

        let retained = Arc::new(crate::model::ExecutorInfo {
            id: "retained-app-a".to_string(),
            application: "app-a".to_string(),
            node: node.name.clone(),
            resreq: rr(1),
            state: ExecutorState::Idle,
            ..Default::default()
        });
        snapshot.add_executor(retained).unwrap();

        let pending = Arc::new(SessionInfo {
            id: "session-app-b".to_string(),
            application: "app-b".to_string(),
            tasks_status: [(TaskState::Pending, 1)].into_iter().collect(),
            state: SessionState::Open,
            resreq: Some(rr(1)),
            ..Default::default()
        });
        snapshot.add_session(pending.clone()).unwrap();

        let mut plugin = DRFPlugin {
            total: ResourceRequirement::default(),
            ssn_map: HashMap::new(),
            node_allocations: HashMap::new(),
        };
        plugin.setup(&snapshot).unwrap();

        assert_eq!(plugin.node_allocations.get(&node.name), Some(&rr(1)));
        assert_eq!(plugin.is_allocatable(&node, &pending), Some(false));
        assert_eq!(plugin.ssn_map.get(&pending.id).unwrap().allocated, rr(0));
    }

    #[test]
    fn pipelining_unowned_unbinding_executor_does_not_double_charge_node() {
        let snapshot = SnapShot::new();
        let node = Arc::new(crate::model::NodeInfo {
            name: "node-1".to_string(),
            allocatable: rr(2),
            state: common::apis::NodeState::Ready,
        });
        snapshot.add_node(node.clone()).unwrap();

        let unbinding = Arc::new(crate::model::ExecutorInfo {
            id: "unbinding".to_string(),
            node: node.name.clone(),
            resreq: rr(1),
            ssn_id: None,
            state: ExecutorState::Unbinding,
            ..Default::default()
        });
        snapshot.add_executor(unbinding.clone()).unwrap();

        let pending = session();
        snapshot.add_session(pending.clone()).unwrap();
        let mut plugin = DRFPlugin {
            total: ResourceRequirement::default(),
            ssn_map: HashMap::new(),
            node_allocations: HashMap::new(),
        };
        plugin.setup(&snapshot).unwrap();
        assert_eq!(plugin.node_allocations.get(&node.name), Some(&rr(1)));

        plugin.on_executor_pipeline(unbinding, pending);

        assert_eq!(plugin.node_allocations.get(&node.name), Some(&rr(1)));
    }
}

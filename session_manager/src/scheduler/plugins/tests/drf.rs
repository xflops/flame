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
use std::sync::Arc;

use chrono::Utc;

use super::super::drf::{DRFPlugin, DRFSessionInfo};
use super::super::Plugin;
use crate::model::{
    ExecutorInfoPtr, NodeInfo, SessionInfo, SessionInfoPtr, SnapShot, ALL_EXECUTOR,
};
use common::apis::{ExecutorState, ResourceRequirement, SessionState, Shim, Signal, TaskState};

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

fn requested(id: &str, demand: i32, unit: ResourceRequirement) -> SessionInfoPtr {
    Arc::new(SessionInfo {
        id: id.into(),
        tasks_status: [(TaskState::Pending, demand)].into_iter().collect(),
        resreq: Some(unit),
        ..session().as_ref().clone()
    })
}

fn idle_session(id: &str) -> SessionInfoPtr {
    requested(id, 0, rr(1))
}

fn bound(id: &str, ssn: &SessionInfoPtr) -> ExecutorInfoPtr {
    Arc::new(crate::model::ExecutorInfo {
        state: ExecutorState::Bound,
        ssn_id: Some(ssn.id.clone()),
        ..void_executor(id).as_ref().clone()
    })
}

/// A 4-slot node, the sessions, and `held` executors bound to each.
fn cluster(sessions: &[(SessionInfoPtr, usize)]) -> DRFPlugin {
    let snapshot = SnapShot::new();
    snapshot
        .add_node(Arc::new(crate::model::NodeInfo {
            name: "node-1".into(),
            allocatable: rr(4),
            state: common::apis::NodeState::Ready,
            shims: vec![common::apis::Shim::Host],
        }))
        .unwrap();
    for (ssn, held) in sessions {
        snapshot.add_session(ssn.clone()).unwrap();
        for i in 0..*held {
            snapshot
                .add_executor(bound(&format!("{}-{i}", ssn.id), ssn))
                .unwrap();
        }
    }
    let mut plugin = DRFPlugin::default();
    plugin.setup(&snapshot).unwrap();
    plugin
}

#[test]
fn sessions_are_ordered_by_dominant_share() {
    let (a, b) = (requested("a", 4, rr(1)), requested("b", 4, rr(1)));
    let plugin = cluster(&[(a.clone(), 3), (b.clone(), 1)]);
    assert_eq!(plugin.ssn_order_fn(&b, &a), Some(Ordering::Greater));
    assert_eq!(plugin.ssn_order_fn(&a, &a), Some(Ordering::Equal));
}

#[test]
fn dominant_share_uses_the_largest_resource_ratio() {
    let ssn = requested(
        "a",
        1,
        ResourceRequirement {
            cpu: 1,
            memory: 3,
            gpu: 0,
        },
    );
    let mut plugin = cluster(&[(ssn.clone(), 0)]);
    plugin.total = ResourceRequirement {
        cpu: 4,
        memory: 4,
        gpu: 0,
    };
    assert_share(
        plugin.calculate_dominant_share(&ssn.resreq.clone().unwrap()),
        0.75,
    );
    let gpu = ResourceRequirement {
        cpu: 0,
        memory: 0,
        gpu: 1,
    };
    assert_eq!(plugin.calculate_dominant_share(&gpu), f64::MAX);
}

fn assert_share(actual: f64, expected: f64) {
    assert!((actual - expected).abs() <= 1e-8, "{actual} != {expected}");
}

#[test]
fn surplus_is_rebalanced_to_a_session_that_wants_capacity() {
    let (a, b) = (requested("a", 8, rr(1)), requested("b", 8, rr(1)));
    let plugin = cluster(&[(a.clone(), 4), (b.clone(), 0)]);
    assert_eq!(plugin.is_overused(&a), Some(true));
    assert_eq!(plugin.is_overused(&b), None);
}

#[test]
fn rebalance_stops_at_an_equal_split() {
    let (a, b) = (requested("a", 8, rr(1)), requested("b", 8, rr(1)));
    assert_eq!(
        cluster(&[(a.clone(), 3), (b.clone(), 1)]).is_overused(&a),
        Some(true)
    );
    let plugin = cluster(&[(a.clone(), 2), (b.clone(), 2)]);
    assert_eq!(plugin.is_overused(&a), None);
    assert_eq!(plugin.is_overused(&b), None);
}

#[test]
fn nothing_is_rebalanced_without_a_contender() {
    let (a, idle) = (requested("a", 8, rr(1)), idle_session("idle"));
    assert_eq!(cluster(&[(a.clone(), 4), (idle, 0)]).is_overused(&a), None);
    assert_eq!(cluster(&[(a.clone(), 4)]).is_overused(&a), None);
}

#[test]
fn a_session_at_its_demand_is_not_a_contender() {
    let a = requested("a", 8, rr(1));
    let capped = Arc::new(SessionInfo {
        max_instances: Some(1),
        ..requested("capped", 8, rr(1)).as_ref().clone()
    });
    assert_eq!(
        cluster(&[(a.clone(), 3), (capped, 1)]).is_overused(&a),
        None
    );
}

#[test]
fn resource_free_sessions_do_not_rebalance() {
    let a = requested("a", 8, rr(1));
    let free = requested("free", 8, rr(0));
    assert_eq!(cluster(&[(a.clone(), 4), (free, 0)]).is_overused(&a), None);
}

#[test]
fn minimum_instances_are_kept() {
    let warm = Arc::new(SessionInfo {
        min_instances: 3,
        ..requested("warm", 0, rr(1)).as_ref().clone()
    });
    let busy = requested("busy", 8, rr(1));
    assert_eq!(
        cluster(&[(warm.clone(), 3), (busy.clone(), 1)]).is_overused(&warm),
        None
    );
    assert_eq!(
        cluster(&[(warm.clone(), 4), (busy, 0)]).is_overused(&warm),
        Some(true)
    );
}

#[test]
fn interrupted_executors_are_not_rebalanced_again() {
    let (a, b) = (requested("a", 8, rr(1)), requested("b", 8, rr(1)));
    let mut plugin = cluster(&[(a.clone(), 3), (b, 1)]);
    assert_eq!(plugin.is_overused(&a), Some(true));
    plugin.on_executor_interrupt(bound("a-0", &a), a.clone());
    assert_eq!(plugin.is_overused(&a), None);
}

#[test]
fn dispatch_pops_the_session_with_the_smallest_share_first() {
    use crate::scheduler::plugins::{PluginManager, PluginsOptions};
    use stdng::collections::{BinaryHeap, Cmp};

    struct ByPlugins(Arc<PluginManager>);
    impl Cmp<SessionInfoPtr> for ByPlugins {
        fn cmp(&self, a: &SessionInfoPtr, b: &SessionInfoPtr) -> Ordering {
            self.0.ssn_order_fn(a, b)
        }
    }

    let (full, empty) = (requested("full", 8, rr(1)), requested("empty", 8, rr(1)));
    let snapshot = SnapShot::new();
    snapshot
        .add_node(Arc::new(crate::model::NodeInfo {
            name: "node-1".into(),
            allocatable: rr(4),
            state: common::apis::NodeState::Ready,
            shims: vec![common::apis::Shim::Host],
        }))
        .unwrap();
    for ssn in [&full, &empty] {
        snapshot.add_session(ssn.clone()).unwrap();
    }
    for i in 0..3 {
        snapshot
            .add_executor(bound(&format!("f-{i}"), &full))
            .unwrap();
    }
    let manager = PluginManager::setup(
        &snapshot,
        &PluginsOptions {
            policies: vec!["drf".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let mut heap = BinaryHeap::new(ByPlugins(manager));
    heap.push(full.clone());
    heap.push(empty.clone());
    assert_eq!(heap.pop().unwrap().id, empty.id);
    assert_eq!(heap.pop().unwrap().id, full.id);
}

#[test]
fn allocation_events_refresh_the_actual_share() {
    let ssn = requested("events", 4, rr(1));
    let mut plugin = cluster(&[(ssn.clone(), 0)]);
    plugin.on_executor_create(void_executor("created"), ssn.clone());
    assert_share(plugin.ssn_map[&ssn.id].dominant, 0.25);
    plugin.on_session_bind(void_executor("retained"), ssn.clone());
    assert_share(plugin.ssn_map[&ssn.id].dominant, 0.5);
    plugin.on_executor_pipeline(void_executor("pipelined"), ssn.clone());
    assert_share(plugin.ssn_map[&ssn.id].dominant, 0.5);
    plugin.on_session_unbind(void_executor("retained"), ssn.clone());
    assert_share(plugin.ssn_map[&ssn.id].dominant, 0.25);
    plugin.on_executor_interrupt(void_executor("created"), ssn.clone());
    assert_share(plugin.ssn_map[&ssn.id].dominant, 0.0);
}

#[test]
fn interrupted_executor_capacity_is_accounted_without_session_share() {
    let snapshot = SnapShot::new();
    let node = Arc::new(crate::model::NodeInfo {
        name: "node-1".into(),
        allocatable: rr(1),
        state: common::apis::NodeState::Ready,
        shims: vec![common::apis::Shim::Host],
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
    let mut plugin = DRFPlugin::default();
    plugin.setup(&snapshot).unwrap();
    assert_eq!(snapshot.find_executors(ALL_EXECUTOR).unwrap().len(), 1);
    assert_eq!(plugin.node_allocations[&node.name], rr(1));
    assert_eq!(plugin.ssn_map[&ssn.id].allocated, rr(0));
    assert_eq!(plugin.is_allocatable(&node, &ssn), Some(false));
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
    assert_eq!(info.dominant, 1.0);
    assert_eq!(plugin.node_allocations["node-1"], rr(4));
}

#[test]
fn pipeline_reserves_future_share_without_allocating_more_node_capacity() {
    let snapshot = SnapShot::new();
    snapshot
        .add_node(Arc::new(crate::model::NodeInfo {
            name: "node-1".into(),
            allocatable: rr(4),
            state: common::apis::NodeState::Ready,
            shims: vec![common::apis::Shim::Host],
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
    let mut plugin = DRFPlugin::default();
    plugin.setup(&snapshot).unwrap();
    assert_eq!(plugin.node_allocations["node-1"], rr(4));
    for exec in departing {
        plugin.on_executor_pipeline(exec, ssn.clone());
    }
    let info = &plugin.ssn_map[&ssn.id];
    assert_eq!(info.allocated, rr(0));
    assert_eq!(info.pipelined, rr(4));
    assert_eq!(info.dominant, 0.0);
    assert_eq!(plugin.node_allocations["node-1"], rr(4));
}

#[test]
fn retained_idle_executor_blocks_cross_application_overcommit() {
    let snapshot = SnapShot::new();
    let node = Arc::new(crate::model::NodeInfo {
        name: "node-1".to_string(),
        allocatable: rr(1),
        state: common::apis::NodeState::Ready,
        shims: vec![common::apis::Shim::Host],
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

    let mut plugin = DRFPlugin::default();
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
        shims: vec![common::apis::Shim::Host],
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
    let mut plugin = DRFPlugin::default();
    plugin.setup(&snapshot).unwrap();
    assert_eq!(plugin.node_allocations.get(&node.name), Some(&rr(1)));

    plugin.on_executor_pipeline(unbinding, pending);

    assert_eq!(plugin.node_allocations.get(&node.name), Some(&rr(1)));
}

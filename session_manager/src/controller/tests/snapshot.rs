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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use chrono::{Duration, Utc};
use rpc::flame::v1 as rpc;
use stdng::lock_ptr;

use crate::controller::snapshot::{
    AppInfo, ExecutorInfo, ExecutorInfoPtr, NodeFilter, NodeInfo, SessionInfo, SessionInfoPtr,
    SnapShot,
};
use crate::storage;
use common::apis::{
    ApplicationAttributes, ApplicationFilter, ApplicationState, Executor, ExecutorFilter,
    ExecutorState, Node, NodeState, ResourceRequirement, Session, SessionAttributes, SessionFilter,
    SessionState, Shim, Signal, Task, TaskOptions, TaskState, ALL_EXECUTOR, BOUND_EXECUTOR,
    IDLE_EXECUTOR, OPEN_SESSION, READY_SESSION, VOID_EXECUTOR,
};
use common::ctx::{FlameCluster, FlameClusterContext};

fn test_context() -> FlameClusterContext {
    FlameClusterContext {
        cluster: FlameCluster {
            storage: "none".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn create_session_attr(id: &str) -> SessionAttributes {
    SessionAttributes {
        id: id.to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 1,
        max_instances: None,
        priority: 0,
        resreq: Some(ResourceRequirement::default()),
        ..Default::default()
    }
}

fn create_test_node(name: &str) -> Node {
    Node {
        name: name.to_string(),
        state: NodeState::Ready,
        ..Default::default()
    }
}

fn create_app_attr() -> ApplicationAttributes {
    ApplicationAttributes::default()
}

#[tokio::test]
async fn returns_empty_snapshot_initially() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();

    let sessions = lock_ptr!(snapshot.sessions).unwrap();
    let executors = lock_ptr!(snapshot.executors).unwrap();
    let nodes = lock_ptr!(snapshot.nodes).unwrap();

    assert!(sessions.is_empty());
    assert!(executors.is_empty());
    assert!(nodes.is_empty());
}

#[tokio::test]
async fn includes_all_sessions() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();
    storage
        .register_application("test-app".into(), create_app_attr())
        .await
        .unwrap();

    for i in 0..3 {
        let attr = create_session_attr(&format!("snap-ssn-{}", i));
        storage.create_session(attr).await.unwrap();
    }

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();
    let sessions = lock_ptr!(snapshot.sessions).unwrap();

    assert_eq!(sessions.len(), 3);
}

#[tokio::test]
async fn includes_all_nodes() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();
    storage
        .register_application("test-app".into(), create_app_attr())
        .await
        .unwrap();

    for i in 0..2 {
        let node = create_test_node(&format!("snap-node-{}", i));
        storage.register_node(&node).await.unwrap();
    }

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();
    let nodes = lock_ptr!(snapshot.nodes).unwrap();

    assert_eq!(nodes.len(), 2);
}

#[tokio::test]
async fn includes_all_executors() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();
    storage
        .register_application("test-app".into(), create_app_attr())
        .await
        .unwrap();

    let attr = create_session_attr("exec-snap-ssn");
    storage.create_session(attr).await.unwrap();

    let node = create_test_node("exec-snap-node");
    storage.register_node(&node).await.unwrap();

    for _ in 0..2 {
        storage
            .create_executor("exec-snap-node".to_string(), "exec-snap-ssn".to_string())
            .await
            .unwrap();
    }

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();
    let executors = lock_ptr!(snapshot.executors).unwrap();

    assert_eq!(executors.len(), 2);
}

#[tokio::test]
async fn includes_all_applications() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();

    let attr = create_app_attr();
    storage
        .register_application("snap-app-1".to_string(), attr.clone())
        .await
        .unwrap();
    storage
        .register_application("snap-app-2".to_string(), attr)
        .await
        .unwrap();

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();
    let applications = lock_ptr!(snapshot.applications).unwrap();

    assert_eq!(applications.len(), 2);
}

#[tokio::test]
async fn snapshot_reflects_state_changes() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();
    storage
        .register_application("test-app".into(), create_app_attr())
        .await
        .unwrap();

    let attr = create_session_attr("state-snap-ssn");
    storage.create_session(attr).await.unwrap();
    storage
        .close_session("state-snap-ssn".to_string())
        .await
        .unwrap();

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();
    let sessions = lock_ptr!(snapshot.sessions).unwrap();

    let ssn_info = sessions.values().next().unwrap();
    assert_eq!(ssn_info.state, SessionState::Closed);
}

#[tokio::test]
async fn task_index_contains_only_non_terminal_tasks_with_affinity() {
    let ctx = test_context();
    let storage = storage::new_ptr(&ctx).await.unwrap();
    storage
        .register_application("test-app".into(), create_app_attr())
        .await
        .unwrap();
    storage
        .create_session(create_session_attr("task-index-ssn"))
        .await
        .unwrap();

    let pending_key = bytes::Bytes::from_static(b"pending");
    let pending = storage
        .create_task(
            "task-index-ssn".to_string(),
            None,
            Some(TaskOptions {
                affinity: [pending_key.clone()].into_iter().collect(),
            }),
        )
        .await
        .unwrap();
    let running = storage
        .create_task(
            "task-index-ssn".to_string(),
            None,
            Some(TaskOptions {
                affinity: [bytes::Bytes::from_static(b"running")]
                    .into_iter()
                    .collect(),
            }),
        )
        .await
        .unwrap();
    let finished = storage
        .create_task("task-index-ssn".to_string(), None, None)
        .await
        .unwrap();
    let session = storage
        .get_session_ptr("task-index-ssn".to_string())
        .unwrap();
    let running_ptr = storage.get_task_ptr(running.gid()).unwrap();
    storage
        .update_task_state(session.clone(), running_ptr, TaskState::Running, None)
        .await
        .unwrap();
    let finished_ptr = storage.get_task_ptr(finished.gid()).unwrap();
    storage
        .update_task_state(session, finished_ptr, TaskState::Succeed, None)
        .await
        .unwrap();

    let snapshot = crate::controller::new_ptr(storage.clone())
        .snapshot()
        .unwrap();
    let sessions = lock_ptr!(snapshot.sessions).unwrap();
    let session = sessions.get("task-index-ssn").unwrap();
    let indexed = session.task_index.get(&TaskState::Pending).unwrap();

    assert_eq!(indexed.len(), 1);
    let task = indexed.get(&pending.id).unwrap();
    assert_eq!(task.state, TaskState::Pending);
    assert_eq!(task.affinity, [pending_key].into_iter().collect());
    assert_eq!(
        session.task_index.get(&TaskState::Running).unwrap().len(),
        1
    );
    assert!(!session.task_index.contains_key(&TaskState::Succeed));
}

/// Helper to create a test executor with given resreq.
fn create_test_executor(
    id: &str,
    resreq: ResourceRequirement,
    state: ExecutorState,
) -> ExecutorInfoPtr {
    Arc::new(ExecutorInfo {
        id: id.to_string(),
        node: "test-node".to_string(),
        resreq,
        shim: Shim::Host,
        application: String::new(),
        task_id: None,
        ssn_id: None,
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state,
        signal: None,
        attributes: HashSet::new(),
    })
}

/// Helper to create a test session with given resreq.
fn create_test_session(
    id: &str,
    resreq: Option<ResourceRequirement>,
    state: SessionState,
) -> SessionInfoPtr {
    Arc::new(SessionInfo {
        batch_size: 1,
        id: id.to_string(),
        application: "test-app".to_string(),
        tasks_status: HashMap::from([(TaskState::Pending, 1)]),
        creation_time: Utc::now(),
        completion_time: None,
        state,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq,
        retry_count: 0,
        task_index: HashMap::new(),
    })
}

/// Default per-slot unit kept for backward-compatible test scaffolding.
fn unit_rr() -> ResourceRequirement {
    ResourceRequirement {
        cpu: 1,
        memory: 1024,
        gpu: 0,
    }
}

/// Scale `unit_rr` by `n`. Mirrors the old `slots × unit` arithmetic the
/// pre-cleanup tests relied on.
fn slots_rr(n: u32) -> ResourceRequirement {
    unit_rr().mul(n)
}

#[test]
fn application_filter_maps_rpc_state() {
    let filter = ApplicationFilter::try_from(rpc::ListApplicationsRequest {
        state: Some(rpc::ApplicationState::Disabled as i32),
        paging: None,
        ..Default::default()
    })
    .unwrap();

    assert_eq!(filter.state, Some(ApplicationState::Disabled));
    assert_eq!(filter.paging, Some(common::apis::Paging::default()));
}

#[test]
fn application_filter_rejects_unknown_rpc_state() {
    assert!(ApplicationFilter::try_from(rpc::ListApplicationsRequest {
        state: Some(99),
        paging: None,
        ..Default::default()
    })
    .is_err());
}

#[test]
fn session_filter_maps_rpc_fields() {
    let filter = SessionFilter::try_from(rpc::ListSessionsRequest {
        application: Some("test-app".to_string()),
        state: Some(rpc::SessionState::Open as i32),
        ids: vec![],
        paging: None,
    })
    .unwrap();

    assert_eq!(filter.application.as_deref(), Some("test-app"));
    assert_eq!(filter.state, Some(SessionState::Open));
    assert_eq!(filter.ids, None);
    assert_eq!(filter.paging, Some(common::apis::Paging::default()));
}

#[test]
fn session_filter_maps_rpc_ids() {
    let filter = SessionFilter::try_from(rpc::ListSessionsRequest {
        application: None,
        state: None,
        ids: vec!["ssn-1".to_string(), "ssn-2".to_string()],
        paging: None,
    })
    .unwrap();

    assert_eq!(
        filter.ids,
        Some(vec!["ssn-1".to_string(), "ssn-2".to_string()])
    );
    assert_eq!(filter.paging, Some(common::apis::Paging::default()));
}

#[test]
fn session_filter_rejects_unknown_rpc_state() {
    assert!(SessionFilter::try_from(rpc::ListSessionsRequest {
        application: None,
        state: Some(99),
        ids: vec![],
        paging: None,
    })
    .is_err());
}

#[test]
fn executor_rpc_round_trip_preserves_application() {
    let executor = Executor {
        application: "test-app".to_string(),
        ..Default::default()
    };

    let rpc_executor = rpc::Executor::from(&executor);
    assert_eq!(rpc_executor.spec.as_ref().unwrap().application, "test-app");
    assert_eq!(Executor::from(&rpc_executor).application, "test-app");
}

#[test]
fn executor_info_preserves_latest_updated_timestamp() {
    let timestamp = Utc::now() - Duration::minutes(5);
    let executor = Executor {
        latest_updated_timestamp: timestamp,
        ..Default::default()
    };

    assert_eq!(
        ExecutorInfo::from(&executor).latest_updated_timestamp,
        timestamp
    );
}

#[test]
fn session_info_ready_uses_retry_limit() {
    let mut session = create_test_session("ssn-ready", Some(unit_rr()), SessionState::Open)
        .as_ref()
        .clone();

    session.retry_count = 1;
    assert!(session.is_ready(2));

    session.retry_count = 2;
    assert!(!session.is_ready(2));
}

#[test]
fn session_info_from_session_copies_retry_count() {
    let session = Session {
        id: "ssn-1".to_string(),
        application: "test-app".to_string(),
        retry_count: 7,
        ..Default::default()
    };

    let info = SessionInfo::try_from(&session).unwrap();

    assert_eq!(info.retry_count, 7);
}

#[test]
fn session_info_counts_terminal_tasks_without_retaining_their_records() {
    let mut session = Session::default();
    for (id, state) in [
        (1, TaskState::Pending),
        (2, TaskState::Running),
        (3, TaskState::Succeed),
        (4, TaskState::Failed),
        (5, TaskState::Cancelled),
    ] {
        session
            .update_task(&Task {
                id,
                state,
                version: 1,
                ..Default::default()
            })
            .unwrap();
    }
    assert_eq!(session.tasks.len(), 2);

    let info = SessionInfo::try_from(&session).unwrap();

    for state in [
        TaskState::Pending,
        TaskState::Running,
        TaskState::Succeed,
        TaskState::Failed,
        TaskState::Cancelled,
    ] {
        assert_eq!(info.tasks_status.get(&state), Some(&1));
    }
    assert_eq!(info.task_index.len(), 2);
    assert!(!info.task_index.contains_key(&TaskState::Succeed));
}

/// Test that SnapShot correctly filters executors by state.
#[test]
fn test_snapshot_find_executors_by_state() {
    let ss = SnapShot::new();

    // Add executors with different states
    let exec_idle1 = create_test_executor("exec-idle-1", slots_rr(2), ExecutorState::Idle);
    let exec_idle2 = create_test_executor("exec-idle-2", slots_rr(2), ExecutorState::Idle);
    let exec_bound = create_test_executor("exec-bound", slots_rr(2), ExecutorState::Bound);
    let exec_void = create_test_executor("exec-void", slots_rr(2), ExecutorState::Void);

    ss.add_executor(exec_idle1.clone()).unwrap();
    ss.add_executor(exec_idle2.clone()).unwrap();
    ss.add_executor(exec_bound.clone()).unwrap();
    ss.add_executor(exec_void.clone()).unwrap();

    // Find only idle executors
    let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
    assert_eq!(idle_execs.len(), 2);
    assert!(idle_execs.contains_key("exec-idle-1"));
    assert!(idle_execs.contains_key("exec-idle-2"));

    // Find only bound executors
    let bound_execs = ss.find_executors(BOUND_EXECUTOR).unwrap();
    assert_eq!(bound_execs.len(), 1);
    assert!(bound_execs.contains_key("exec-bound"));

    // Find only void executors
    let void_execs = ss.find_executors(VOID_EXECUTOR).unwrap();
    assert_eq!(void_execs.len(), 1);
    assert!(void_execs.contains_key("exec-void"));

    // Find all executors
    let all_execs = ss.find_executors(ALL_EXECUTOR).unwrap();
    assert_eq!(all_execs.len(), 4);
}

/// Test that SnapShot correctly filters sessions by state.
#[test]
fn test_snapshot_find_sessions_by_state() {
    let ss = SnapShot::new();

    // Add sessions with different states
    let ssn_open1 = create_test_session("ssn-open-1", Some(slots_rr(2)), SessionState::Open);
    let ssn_open2 = create_test_session("ssn-open-2", Some(slots_rr(2)), SessionState::Open);
    let ssn_closed = create_test_session("ssn-closed", Some(slots_rr(2)), SessionState::Closed);

    ss.add_session(ssn_open1.clone()).unwrap();
    ss.add_session(ssn_open2.clone()).unwrap();
    ss.add_session(ssn_closed.clone()).unwrap();

    // Find only open sessions
    let open_ssns = ss.find_sessions(OPEN_SESSION).unwrap();
    assert_eq!(open_ssns.len(), 2);
    assert!(open_ssns.contains_key("ssn-open-1"));
    assert!(open_ssns.contains_key("ssn-open-2"));

    // Find all sessions
    let all_ssns = ss.find_sessions(None).unwrap();
    assert_eq!(all_ssns.len(), 3);
}

#[test]
fn test_snapshot_find_ready_sessions() {
    let ss = SnapShot::new_with_session_retry_limits(2);

    let ready_open = create_test_session("ssn-ready", Some(slots_rr(2)), SessionState::Open);
    let mut not_ready_open =
        create_test_session("ssn-not-ready", Some(slots_rr(2)), SessionState::Open)
            .as_ref()
            .clone();
    not_ready_open.retry_count = 2;
    let closed_ready =
        create_test_session("ssn-closed-ready", Some(slots_rr(2)), SessionState::Closed);

    ss.add_session(ready_open.clone()).unwrap();
    ss.add_session(Arc::new(not_ready_open)).unwrap();
    ss.add_session(closed_ready.clone()).unwrap();

    let ready_ssns = ss.find_sessions(READY_SESSION).unwrap();

    assert_eq!(ready_ssns.len(), 1);
    assert!(ready_ssns.contains_key("ssn-ready"));
}

#[test]
fn test_snapshot_node_is_ready_only_when_its_state_is_ready() {
    let ss = SnapShot::new();
    for (name, state) in [
        ("ready", NodeState::Ready),
        ("draining", NodeState::Unknown),
        ("dead", NodeState::NotReady),
    ] {
        ss.add_node(Arc::new(NodeInfo {
            name: name.to_string(),
            state,
            ..Default::default()
        }))
        .unwrap();
    }

    assert!(ss.get_node("ready").unwrap().unwrap().is_ready());
    assert!(!ss.get_node("draining").unwrap().unwrap().is_ready());
    assert!(!ss.get_node("dead").unwrap().unwrap().is_ready());
    assert!(ss.get_node("unknown").unwrap().is_none());
}

#[test]
fn test_snapshot_suspending_and_suspended_sessions_are_not_ready() {
    let ss = SnapShot::new();
    for (id, state) in [
        ("ssn-open", SessionState::Open),
        ("ssn-suspending", SessionState::Suspending),
        ("ssn-suspended", SessionState::Suspended),
    ] {
        ss.add_session(create_test_session(id, Some(slots_rr(2)), state))
            .unwrap();
    }

    // Dispatch and Allocate read READY_SESSION, DRF reads OPEN_SESSION.
    for filter in [READY_SESSION, OPEN_SESSION] {
        let found = ss.find_sessions(filter).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found.contains_key("ssn-open"));
    }
}

/// Test that update_executor_state correctly updates the exec_index.
#[test]
fn test_snapshot_update_executor_state() {
    let ss = SnapShot::new();
    let previous_timestamp = Utc::now() - Duration::minutes(5);

    // Add an idle executor
    let exec = Arc::new(ExecutorInfo {
        application: "test-app".to_string(),
        attributes: HashSet::from([Bytes::from_static(b"key")]),
        latest_updated_timestamp: previous_timestamp,
        ..(*create_test_executor("exec-1", slots_rr(2), ExecutorState::Idle)).clone()
    });
    ss.add_executor(exec.clone()).unwrap();

    // Verify it's in the idle index
    let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
    assert_eq!(idle_execs.len(), 1);

    // Update state to Bound
    ss.update_executor_state(exec.clone(), ExecutorState::Bound)
        .unwrap();

    // Verify it's no longer in idle index
    let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
    assert_eq!(idle_execs.len(), 0);

    // Verify it's now in bound index
    let bound_execs = ss.find_executors(BOUND_EXECUTOR).unwrap();
    assert_eq!(bound_execs.len(), 1);
    let updated = bound_execs.get("exec-1").unwrap();
    assert_eq!(updated.application, "test-app");
    assert!(updated.latest_updated_timestamp > previous_timestamp);
    assert_eq!(
        updated.attributes,
        HashSet::from([Bytes::from_static(b"key")])
    );
}

/// Test pipelined_executors filters by resreq equality correctly.
#[test]
fn test_snapshot_pipelined_executors_filters_by_resreq() {
    let ss = SnapShot::new();

    // Register the application so `pipelined_executors` can look up its shim.
    ss.add_application(Arc::new(AppInfo {
        name: "test-app".to_string(),
        state: ApplicationState::Enabled,
        shim: Shim::Host,
        max_instances: 0,
        delay_release: chrono::Duration::zero(),
    }))
    .unwrap();

    // Executors sized by different resreq values.
    let exec_r2_idle = create_test_executor("exec-r2-idle", slots_rr(2), ExecutorState::Idle);
    let exec_r4_idle = create_test_executor("exec-r4-idle", slots_rr(4), ExecutorState::Idle);
    let exec_r2_void = create_test_executor("exec-r2-void", slots_rr(2), ExecutorState::Void);
    let exec_r2_bound = create_test_executor("exec-r2-bound", slots_rr(2), ExecutorState::Bound);

    ss.add_executor(exec_r2_idle.clone()).unwrap();
    ss.add_executor(exec_r4_idle.clone()).unwrap();
    ss.add_executor(exec_r2_void.clone()).unwrap();
    ss.add_executor(exec_r2_bound.clone()).unwrap();

    // Session asks for resreq equivalent to "2 × unit".
    let ssn = create_test_session("ssn-1", Some(slots_rr(2)), SessionState::Open);

    // Get pipelined executors (should only include idle and void with matching resreq).
    let pipelined = ss.pipelined_executors(ssn).unwrap();

    // Should include exec-r2-idle and exec-r2-void (resreq matches and state is idle/void).
    // Should NOT include exec-r4-idle (wrong resreq) or exec-r2-bound (wrong state).
    assert_eq!(pipelined.len(), 2);

    let ids: Vec<&str> = pipelined.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&"exec-r2-idle"));
    assert!(ids.contains(&"exec-r2-void"));
    assert!(!ids.contains(&"exec-r4-idle"));
    assert!(!ids.contains(&"exec-r2-bound"));
}

/// Test that empty filters return empty results.
#[test]
fn test_snapshot_empty_filters() {
    let ss = SnapShot::new();

    // Add some executors
    let exec = create_test_executor("exec-1", slots_rr(2), ExecutorState::Idle);
    ss.add_executor(exec).unwrap();

    // Filter by state that doesn't exist
    let releasing_execs = ss
        .find_executors(Some(ExecutorFilter {
            state: Some(ExecutorState::Releasing),
            ids: None,
            node: None,
            paging: None,
            application: None,
            session_id: None,
        }))
        .unwrap();
    assert_eq!(releasing_execs.len(), 0);

    // Filter by ID that doesn't exist
    let nonexistent = ss
        .find_executors(Some(ExecutorFilter {
            state: None,
            ids: Some(vec!["nonexistent".to_string()]),
            node: None,
            paging: None,
            application: None,
            session_id: None,
        }))
        .unwrap();
    assert_eq!(nonexistent.len(), 0);
}

#[test]
fn interrupted_executors_remain_visible_in_actual_state_indexes() {
    let snapshot = SnapShot::new();
    snapshot
        .add_executor(create_test_executor(
            "executor",
            slots_rr(2),
            ExecutorState::Bound,
        ))
        .unwrap();
    let states = [
        ExecutorState::Bound,
        ExecutorState::Unbinding,
        ExecutorState::Idle,
    ];
    for state in states {
        let signal = (state != ExecutorState::Idle).then_some(Signal::Interrupt);
        let exec = Arc::new(ExecutorInfo {
            signal,
            ..create_test_executor("executor", slots_rr(2), state)
                .as_ref()
                .clone()
        });
        // Repeated updates preserve a single entry and remove old state indexes.
        for _ in 0..2 {
            snapshot.update_executor(exec.clone()).unwrap();
            let all = snapshot.find_executors(ALL_EXECUTOR).unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[&exec.id].state, state);
            assert_eq!(all[&exec.id].signal, signal);
            for indexed_state in states {
                let indexed = snapshot
                    .find_executors(Some(ExecutorFilter {
                        state: Some(indexed_state),
                        ids: None,
                        node: None,
                        paging: None,
                        application: None,
                        session_id: None,
                    }))
                    .unwrap();
                assert_eq!(indexed.len(), usize::from(indexed_state == state));
            }
        }
    }
}

/// Test that delete_executor removes from both main map and index.
#[test]
fn test_snapshot_delete_executor() {
    let ss = SnapShot::new();

    let exec = create_test_executor("exec-1", slots_rr(2), ExecutorState::Idle);
    ss.add_executor(exec.clone()).unwrap();

    // Verify it exists
    let all_execs = ss.find_executors(ALL_EXECUTOR).unwrap();
    assert_eq!(all_execs.len(), 1);

    let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
    assert_eq!(idle_execs.len(), 1);

    // Delete it
    ss.delete_executor(exec).unwrap();

    // Verify it's gone from both
    let all_execs = ss.find_executors(ALL_EXECUTOR).unwrap();
    assert_eq!(all_execs.len(), 0);

    let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
    assert_eq!(idle_execs.len(), 0);
}

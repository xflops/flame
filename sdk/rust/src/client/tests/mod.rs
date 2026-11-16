/*
Copyright 2023 The Flame Authors.
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

use super::*;
use crate::message::FlameMessage;
use bytes::Bytes;
use chrono::TimeZone;
use serde_derive::{Deserialize, Serialize};
use stdng::new_ptr;

#[test]
fn node_conversion_preserves_gpu_capacity() {
    let response = rpc::Node {
        status: Some(rpc::NodeStatus {
            capacity: Some(rpc::ResourceRequirement {
                cpu: 192,
                memory: 2 * 1024_u64.pow(4),
                gpu: 8,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let node = Node::from(&response);
    assert_eq!(node.cpu, 192);
    assert_eq!(node.memory, 2 * 1024_u64.pow(4));
    assert_eq!(node.gpu, 8);
    assert_eq!(Node::from(response).gpu, 8);
    assert_eq!(serde_json::to_value(node).unwrap()["gpu"], 8);
}

#[test]
fn node_conversion_defaults_missing_capacity_to_zero() {
    let node = Node::from(rpc::Node::default());
    assert_eq!(node.cpu, 0);
    assert_eq!(node.memory, 0);
    assert_eq!(node.gpu, 0);
}

#[test]
fn node_json_without_gpu_remains_compatible() {
    let node: Node = serde_json::from_value(serde_json::json!({
        "name": "node-1",
        "hostname": "node-1",
        "state": "Ready",
        "cpu": 16,
        "memory": 32 * 1024_u64.pow(3),
        "arch": "x86_64",
        "os": "linux"
    }))
    .unwrap();
    assert_eq!(node.gpu, 0);
    assert!(node.kernel_version.is_empty());
    assert!(node.os_image.is_empty());
    assert!(node.addresses.is_empty());
    assert!(node.network_interfaces.is_empty());
    assert!(node.pci_devices.is_empty());
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct TestCommonData {
    value: String,
}

impl crate::message::FlameMessage for TestCommonData {
    fn encode(&self) -> Result<Bytes, FlameError> {
        serde_json::to_vec(self)
            .map(Bytes::from)
            .map_err(|e| FlameError::Internal(e.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<Self, FlameError> {
        serde_json::from_slice(bytes).map_err(|e| FlameError::InvalidConfig(e.to_string()))
    }
}

fn test_task(id: &str, state: TaskState) -> Task {
    Task {
        id: id.to_string(),
        ssn_id: "ssn-1".to_string(),
        state,
        input: None,
        output: None,
        affinity: HashSet::new(),
        events: Vec::new(),
    }
}

#[tokio::test]
async fn connection_shares_watch_manager_across_session_handles() {
    let channel = Endpoint::from_static("http://127.0.0.1:0").connect_lazy();
    let connection = Connection {
        channel: channel.clone(),
        watch_managers: Arc::default(),
    };
    let response = rpc::Session {
        metadata: Some(rpc::Metadata {
            id: "ssn-1".to_string(),
            ..Default::default()
        }),
        spec: Some(rpc::SessionSpec {
            application: "app".to_string(),
            ..Default::default()
        }),
        status: Some(rpc::SessionStatus {
            state: rpc::SessionState::Open as i32,
            ..Default::default()
        }),
    };
    let first = connection
        .attach_session(&response, FlameClient::new(channel.clone()))
        .unwrap();
    let second = connection
        .clone()
        .attach_session(&response, FlameClient::new(channel))
        .unwrap();
    assert!(lock_ptr!(connection.watch_managers).unwrap().is_empty());
    let first_manager = first.connection.as_ref().unwrap().watch_manager(&first.id);
    let second_manager = second
        .connection
        .as_ref()
        .unwrap()
        .watch_manager(&second.id);
    assert!(Arc::ptr_eq(&first_manager, &second_manager));
    assert!(second.client.is_some());
    assert!(!Arc::ptr_eq(
        &first_manager,
        &connection.watch_manager(&"ssn-2".to_string())
    ));
}

#[tokio::test]
async fn concurrent_session_handles_share_one_watch_manager() {
    let connection = Connection {
        channel: Endpoint::from_static("http://127.0.0.1:0").connect_lazy(),
        watch_managers: Arc::default(),
    };
    let managers = (0..16)
        .map(|_| {
            let connection = connection.clone();
            std::thread::spawn(move || connection.watch_manager(&"ssn-1".to_string()))
        })
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert!(managers
        .iter()
        .all(|manager| Arc::ptr_eq(manager, &managers[0])));
}

#[tokio::test]
async fn connection_retains_watch_manager_until_removed() {
    let connection = Connection {
        channel: Endpoint::from_static("http://127.0.0.1:0").connect_lazy(),
        watch_managers: Arc::default(),
    };
    let live = connection.watch_manager(&"ssn-1".to_string());
    let weak = Arc::downgrade(&live);
    drop(live);
    assert_eq!(lock_ptr!(connection.watch_managers).unwrap().len(), 1);
    assert!(weak.upgrade().is_some());
    let manager = lock_ptr!(connection.watch_managers)
        .unwrap()
        .remove("ssn-1")
        .unwrap();
    assert!(Arc::ptr_eq(&manager, &weak.upgrade().unwrap()));
    drop(manager);
    assert!(weak.upgrade().is_none());
}

struct ErrorRecorder(MutexPtr<Vec<String>>);

impl TaskInformer for ErrorRecorder {
    fn on_update(&mut self, _task: Task) {}

    fn on_error(&mut self, error: FlameError) {
        lock_ptr!(self.0).unwrap().push(error.to_string());
    }
}

#[tokio::test]
async fn watch_registration_error_reaches_informer() {
    let channel = Endpoint::from_static("http://127.0.0.1:0").connect_lazy();
    let connection = Connection {
        channel: channel.clone(),
        watch_managers: Arc::default(),
    };
    let session = Session {
        client: Some(FlameClient::new(channel)),
        connection: Some(connection.clone()),
        id: "ssn-1".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        creation_time: Utc::now(),
        state: SessionState::Open,
        pending: 0,
        running: 0,
        succeed: 0,
        failed: 0,
        events: Vec::new(),
        tasks: None,
        priority: 0,
        resreq: None,
    };
    let (requests, request_rx) = mpsc::channel(1);
    drop(request_rx);
    let stream = tokio::spawn(std::future::pending::<()>());
    connection
        .watch_manager(&session.id)
        .state
        .lock()
        .await
        .stream = Some(WatchStream {
        requests,
        task: stream.abort_handle(),
    });
    let errors = new_ptr(Vec::new());
    let informer: TaskInformerPtr = new_ptr(ErrorRecorder(errors.clone()));

    assert!(session
        .watch_task("ssn-1".to_string(), "task-1".to_string(), informer)
        .await
        .is_err());
    assert_eq!(lock_ptr!(errors).unwrap().len(), 1);
    stream.abort();
}

#[tokio::test]
async fn shared_watch_routes_terminal_tasks_independently() {
    let manager = WatchManager::default();
    let (first_tx, mut first_rx) = watch::channel(None);
    let (second_tx, mut second_rx) = watch::channel(None);
    {
        let mut state = manager.state.lock().await;
        state.task_watcher.insert("first".to_string(), first_tx);
        state.task_watcher.insert("second".to_string(), second_tx);
    }

    manager
        .deliver(test_task("first", TaskState::Running))
        .await;
    assert_eq!(
        next_watch_update(&mut first_rx)
            .await
            .unwrap()
            .unwrap()
            .state,
        TaskState::Running
    );
    manager
        .deliver(test_task("second", TaskState::Succeed))
        .await;
    assert_eq!(
        next_watch_update(&mut second_rx).await.unwrap().unwrap().id,
        "second"
    );
    manager.deliver(test_task("first", TaskState::Failed)).await;
    assert_eq!(
        next_watch_update(&mut first_rx)
            .await
            .unwrap()
            .unwrap()
            .state,
        TaskState::Failed
    );
    assert!(manager.state.lock().await.task_watcher.is_empty());
}

#[tokio::test]
async fn shared_watch_registers_each_waiter_for_fresh_frontend_snapshot() {
    let manager = Arc::new(WatchManager::default());
    let (requests, mut request_rx) = mpsc::channel(2);
    manager.state.lock().await.stream = Some(WatchStream {
        requests,
        task: tokio::spawn(std::future::pending::<()>()).abort_handle(),
    });
    let client = FlameClient::new(Endpoint::from_static("http://127.0.0.1:0").connect_lazy());
    let mut first = manager
        .register(client.clone(), "ssn-1".to_string(), "task-1".to_string())
        .await
        .unwrap();
    manager
        .deliver(test_task("task-1", TaskState::Running))
        .await;
    let mut second = manager
        .register(client, "ssn-1".to_string(), "task-1".to_string())
        .await
        .unwrap();
    assert_eq!(request_rx.try_recv().unwrap().task_id, "task-1");
    assert_eq!(request_rx.try_recv().unwrap().task_id, "task-1");
    assert!(request_rx.try_recv().is_err());
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(20),
        next_watch_update(&mut second)
    )
    .await
    .is_err());

    // A later frontend snapshot, rather than the locally cached Running
    // state, is delivered to the second waiter.
    manager
        .deliver(test_task("task-1", TaskState::Pending))
        .await;
    assert_eq!(
        next_watch_update(&mut second).await.unwrap().unwrap().state,
        TaskState::Pending
    );

    manager
        .deliver(test_task("task-1", TaskState::Succeed))
        .await;
    assert_eq!(
        next_watch_update(&mut first).await.unwrap().unwrap().state,
        TaskState::Succeed
    );
    assert_eq!(
        next_watch_update(&mut second).await.unwrap().unwrap().state,
        TaskState::Succeed
    );
    assert!(manager.state.lock().await.task_watcher.is_empty());
}

#[tokio::test]
async fn shared_watch_coalesces_updates_but_preserves_terminal_state() {
    let manager = WatchManager::default();
    let (reply, mut receiver) = watch::channel(None);
    manager
        .state
        .lock()
        .await
        .task_watcher
        .insert("first".to_string(), reply);

    for _ in 0..1000 {
        manager
            .deliver(test_task("first", TaskState::Running))
            .await;
    }
    manager
        .deliver(test_task("first", TaskState::Succeed))
        .await;
    assert_eq!(
        next_watch_update(&mut receiver)
            .await
            .unwrap()
            .unwrap()
            .state,
        TaskState::Succeed
    );
    assert!(next_watch_update(&mut receiver).await.is_none());
    assert!(manager.state.lock().await.task_watcher.is_empty());
}

#[tokio::test]
async fn shared_watch_removes_dropped_receiver_on_completion() {
    let manager = WatchManager::default();
    let (reply, receiver) = watch::channel(None);
    manager
        .state
        .lock()
        .await
        .task_watcher
        .insert("first".to_string(), reply);
    drop(receiver);
    manager
        .deliver(test_task("first", TaskState::Running))
        .await;
    assert_eq!(manager.state.lock().await.task_watcher.len(), 1);
    manager
        .deliver(test_task("first", TaskState::Succeed))
        .await;
    assert!(manager.state.lock().await.task_watcher.is_empty());
}

#[tokio::test]
async fn closing_shared_watch_fails_all_task_watchers() {
    let manager = WatchManager::default();
    let (reply, mut receiver) = watch::channel(None);
    let watch = tokio::spawn(std::future::pending::<()>());
    let (requests, _request_rx) = mpsc::channel(1);
    {
        let mut state = manager.state.lock().await;
        state.stream = Some(WatchStream {
            requests,
            task: watch.abort_handle(),
        });
        state.task_watcher.insert("first".to_string(), reply);
    }
    manager.close().await;
    assert!(next_watch_update(&mut receiver).await.unwrap().is_err());
    assert!(manager.state.lock().await.stream.is_none());
    assert!(watch.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn stopped_shared_watch_errors_waiters_and_allows_replacement() {
    let manager = WatchManager::default();
    let (reply, mut receiver) = watch::channel(None);
    manager
        .state
        .lock()
        .await
        .task_watcher
        .insert("first".to_string(), reply);

    manager
        .finish(FlameError::Network("watch stream disconnected".to_string()))
        .await;
    assert!(matches!(
        next_watch_update(&mut receiver).await,
        Some(Err(FlameError::Network(message))) if message == "watch stream disconnected"
    ));
    let state = manager.state.lock().await;
    assert!(state.stream.is_none());
    assert!(state.task_watcher.is_empty());
}

#[tokio::test]
async fn failed_suspend_and_resume_keep_shared_watch_open() {
    let channel = Endpoint::from_static("http://127.0.0.1:0").connect_lazy();
    let connection = Connection {
        channel: channel.clone(),
        watch_managers: Arc::default(),
    };
    let manager = connection.watch_manager(&"ssn-1".to_string());
    let timeout = std::time::Duration::from_secs(3);
    assert!(
        tokio::time::timeout(timeout, connection.suspend_session("ssn-1"))
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        tokio::time::timeout(timeout, connection.resume_session("ssn-1"))
            .await
            .unwrap()
            .is_err()
    );
    assert!(lock_ptr!(connection.watch_managers)
        .unwrap()
        .contains_key("ssn-1"));
    assert!(Arc::ptr_eq(
        &manager,
        &connection.watch_manager(&"ssn-1".to_string())
    ));
}

#[tokio::test]
async fn session_suspend_without_connection_is_an_error() {
    let session = Session {
        client: None,
        connection: None,
        id: "ssn-1".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        creation_time: Utc::now(),
        state: SessionState::Open,
        pending: 0,
        running: 0,
        succeed: 0,
        failed: 0,
        events: Vec::new(),
        tasks: None,
        priority: 0,
        resreq: None,
    };
    assert!(session.suspend().await.is_err());
    assert!(session.resume().await.is_err());
}

#[tokio::test]
async fn failed_session_close_keeps_shared_watch_open() {
    let channel = Endpoint::from_static("http://127.0.0.1:0").connect_lazy();
    let connection = Connection {
        channel: channel.clone(),
        watch_managers: Arc::default(),
    };
    let session = Session {
        client: Some(FlameClient::new(channel)),
        connection: Some(connection.clone()),
        id: "ssn-1".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        creation_time: Utc::now(),
        state: SessionState::Open,
        pending: 0,
        running: 0,
        succeed: 0,
        failed: 0,
        events: Vec::new(),
        tasks: None,
        priority: 0,
        resreq: None,
    };
    let (reply, mut receiver) = watch::channel(None);
    let manager = connection.watch_manager(&session.id);
    manager
        .state
        .lock()
        .await
        .task_watcher
        .insert("first".to_string(), reply);

    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), session.close())
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(manager.state.lock().await.task_watcher.len(), 1);
    manager
        .deliver(test_task("first", TaskState::Succeed))
        .await;
    assert_eq!(
        next_watch_update(&mut receiver)
            .await
            .unwrap()
            .unwrap()
            .state,
        TaskState::Succeed
    );
}

#[test]
fn session_options_generate_default_attributes() {
    let attrs = SessionOptions::new("model-app")
        .min_instances(1)
        .batch_size(2)
        .priority(7)
        .resreq("cpu=4,mem=16g")
        .into_session_attributes()
        .unwrap();

    assert!(attrs.id.starts_with("model-app-"));
    assert_eq!(attrs.application, "model-app");
    assert_eq!(attrs.min_instances, 1);
    assert_eq!(attrs.batch_size, 2);
    assert_eq!(attrs.priority, 7);
    let resreq = attrs.resreq.unwrap();
    assert_eq!(resreq.cpu, 4);
    assert_eq!(resreq.memory, 16 * 1024 * 1024 * 1024);
}

#[test]
fn resource_requirement_parse_rejects_malformed_input() {
    for input in ["cpu=abc", "mem=bogus", "gpu=", "foo=1", "cpu=1,"] {
        assert!(
            ResourceRequirement::parse(input).is_err(),
            "expected invalid resreq to fail: {input}"
        );
    }
}

#[test]
fn resource_requirement_parse_accepts_binary_memory_suffixes() {
    for (input, expected) in [
        ("cpu=1,mem=16Gi,gpu=0", 16 * 1024_u64.pow(3)),
        ("mem=2T", 2 * 1024_u64.pow(4)),
        ("mem=2Ti", 2 * 1024_u64.pow(4)),
        ("mem=1PB", 1024_u64.pow(5)),
        ("mem=1Pi", 1024_u64.pow(5)),
    ] {
        let resreq = ResourceRequirement::parse(input).unwrap();
        assert_eq!(resreq.memory, expected, "input: {input}");
    }
}

#[test]
fn session_options_encode_typed_common_data() {
    let common_data = TestCommonData {
        value: "ctx".to_string(),
    };

    let attrs = SessionOptions::new("model-app")
        .id("ssn-1")
        .common_data(&common_data)
        .unwrap()
        .into_session_attributes()
        .unwrap();

    assert_eq!(attrs.id, "ssn-1");
    let decoded = TestCommonData::decode(&attrs.common_data.unwrap()).unwrap();
    assert_eq!(decoded, common_data);
}

#[tokio::test]
async fn task_future_await_returns_task_result() {
    let payload = TestCommonData {
        value: "done".to_string(),
    };
    let mut task = test_task("task-1", TaskState::Succeed);
    task.output = Some(payload.encode().unwrap());
    let task_future = TaskFuture {
        task_id: "task-1".to_string(),
        future: Box::pin(async move { TaskResult::<TestCommonData>::from_task(task) }),
    };

    assert_eq!(task_future.id(), "task-1");
    let result = task_future.await.unwrap();
    assert_eq!(result.task_id, "task-1");
    assert_eq!(result.session_id, "ssn-1");
    assert!(result.is_succeed());
    assert_eq!(result.output.unwrap().value, "done");
    assert_eq!(result.error_code, None);
    assert_eq!(result.error_message, None);
}

#[tokio::test]
async fn task_handle_await_returns_success_output() {
    let payload = TestCommonData {
        value: "done".to_string(),
    };
    let mut task = test_task("task-1", TaskState::Succeed);
    task.output = Some(payload.encode().unwrap());
    let task_future = TaskFuture {
        task_id: "task-1".to_string(),
        future: Box::pin(async move { TaskResult::<TestCommonData>::from_task(task) }),
    };

    let handle = TaskHandle::from(task_future);

    assert_eq!(handle.id(), "task-1");
    let output = handle.await.unwrap().unwrap();
    assert_eq!(output.value, "done");
}

#[tokio::test]
async fn task_handle_await_returns_error_for_failed_task() {
    let mut task = test_task("task-2", TaskState::Failed);
    task.events.push(Event {
        code: 42,
        message: Some("remote failure".to_string()),
        creation_time: Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap(),
    });
    let task_future = TaskFuture {
        task_id: "task-2".to_string(),
        future: Box::pin(async move { TaskResult::<TestCommonData>::from_task(task) }),
    };

    let err = TaskHandle::from(task_future).await.unwrap_err();

    assert!(err.to_string().contains("task <task-2>"));
    assert!(err.to_string().contains("code <42>"));
    assert!(err.to_string().contains("remote failure"));
}

#[test]
fn task_result_records_failed_task_error_details() {
    let mut task = test_task("task-2", TaskState::Failed);
    task.events.push(Event {
        code: 42,
        message: Some("remote failure".to_string()),
        creation_time: Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap(),
    });

    let result = TaskResult::<TestCommonData>::from_task(task).unwrap();

    assert_eq!(result.task_id, "task-2");
    assert_eq!(result.session_id, "ssn-1");
    assert!(result.is_failed());
    assert!(result.output.is_none());
    assert_eq!(result.error_code, Some(42));
    assert_eq!(result.error_message.as_deref(), Some("remote failure"));
}

/// Regression test for `Session::try_from` creation_time parsing.
///
/// The wire protocol carries `creation_time` as Unix milliseconds (see
/// `common/src/apis/to_rpc.rs` which calls `.timestamp_millis()`). The SDK
/// must parse it as milliseconds without an extra `* 1000` multiplication.
///
/// Previously the SDK multiplied by 1000 again, producing a `DateTime`
/// roughly 1000× off (year ~57000 instead of the expected year). This test
/// locks the correct behavior.
#[test]
fn session_try_from_parses_creation_time_as_millis() {
    let expected = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
    let wire_millis = expected.timestamp_millis();

    let rpc_session = rpc::Session {
        metadata: Some(rpc::Metadata {
            id: "ssn-1".to_string(),
            name: String::new(),
        }),
        spec: Some(rpc::SessionSpec {
            batch_size: 1,
            application: "app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            priority: 0,
            resreq: None,
            tokens: Default::default(),
        }),
        status: Some(rpc::SessionStatus {
            state: rpc::SessionState::Open as i32,
            creation_time: wire_millis,
            completion_time: None,
            pending: 0,
            running: 0,
            succeed: 0,
            failed: 0,
            cancelled: 0,
            events: vec![],
        }),
    };

    let parsed = Session::try_from(&rpc_session).expect("Session::try_from should succeed");
    assert_eq!(parsed.creation_time, expected);
}

#[test]
fn session_try_from_extracts_events() {
    let when = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
    let rpc_session = rpc::Session {
        metadata: Some(rpc::Metadata {
            id: "ssn-1".to_string(),
            name: String::new(),
        }),
        spec: Some(rpc::SessionSpec {
            batch_size: 1,
            application: "app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            priority: 0,
            resreq: None,
            tokens: Default::default(),
        }),
        status: Some(rpc::SessionStatus {
            state: rpc::SessionState::Open as i32,
            creation_time: when.timestamp_millis(),
            completion_time: None,
            pending: 0,
            running: 0,
            succeed: 0,
            failed: 0,
            cancelled: 0,
            events: vec![rpc::Event {
                code: 1001,
                message: Some("bind failed".to_string()),
                creation_time: when.timestamp_millis(),
            }],
        }),
    };

    let parsed = Session::try_from(&rpc_session).expect("Session::try_from should succeed");

    assert_eq!(parsed.events.len(), 1);
    assert_eq!(parsed.events[0].code, 1001);
    assert_eq!(parsed.events[0].message.as_deref(), Some("bind failed"));
}

/// Verifies that `From<rpc::ResourceRequirement> for ResourceRequirement`
/// preserves all fields when converting from the wire type into the SDK type.
#[test]
fn resource_requirement_from_rpc_preserves_fields() {
    let rpc_rr = rpc::ResourceRequirement {
        cpu: 4,
        memory: 16 * 1024 * 1024 * 1024,
        gpu: 2,
    };
    let sdk_rr = ResourceRequirement::from(rpc_rr);
    assert_eq!(sdk_rr.cpu, 4);
    assert_eq!(sdk_rr.memory, 16 * 1024 * 1024 * 1024);
    assert_eq!(sdk_rr.gpu, 2);
}

#[test]
fn executor_try_from_preserves_application() {
    let rpc_executor = rpc::Executor {
        metadata: Some(rpc::Metadata {
            id: "executor-1".to_string(),
            name: String::new(),
        }),
        spec: Some(rpc::ExecutorSpec {
            node: "node-1".to_string(),
            resreq: Some(rpc::ResourceRequirement::default()),
            shim: rpc::Shim::Host as i32,
            application: "app-1".to_string(),
        }),
        status: Some(rpc::ExecutorStatus {
            state: rpc::ExecutorState::ExecutorIdle as i32,
            session_id: None,
        }),
    };

    let executor = Executor::try_from(&rpc_executor).unwrap();

    assert_eq!(executor.application, "app-1");
}

/// Verifies that `Session::try_from` extracts the optional `resreq` from
/// the RPC `SessionSpec` so the CLI can display per-session resource
/// requirements end-to-end.
#[test]
fn session_try_from_extracts_resreq() {
    let when = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
    let rpc_session = rpc::Session {
        metadata: Some(rpc::Metadata {
            id: "ssn-1".to_string(),
            name: String::new(),
        }),
        spec: Some(rpc::SessionSpec {
            batch_size: 1,
            application: "app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            priority: 0,
            resreq: Some(rpc::ResourceRequirement {
                cpu: 8,
                memory: 32 * 1024 * 1024 * 1024,
                gpu: 1,
            }),
            tokens: Default::default(),
        }),
        status: Some(rpc::SessionStatus {
            state: rpc::SessionState::Open as i32,
            creation_time: when.timestamp_millis(),
            completion_time: None,
            pending: 0,
            running: 0,
            succeed: 0,
            failed: 0,
            cancelled: 0,
            events: vec![],
        }),
    };

    let parsed = Session::try_from(&rpc_session).expect("Session::try_from should succeed");
    let resreq = parsed.resreq.expect("resreq should be populated");
    assert_eq!(resreq.cpu, 8);
    assert_eq!(resreq.memory, 32 * 1024 * 1024 * 1024);
    assert_eq!(resreq.gpu, 1);
}

/// Regression test for `Application::try_from` creation_time parsing.
/// See `session_try_from_parses_creation_time_as_millis` for context.
#[test]
fn application_try_from_parses_creation_time_as_millis() {
    let expected = Utc.with_ymd_and_hms(2026, 5, 8, 10, 0, 0).unwrap();
    let wire_millis = expected.timestamp_millis();

    let rpc_app = rpc::Application {
        metadata: Some(rpc::Metadata {
            id: String::new(),
            name: "app-1".to_string(),
        }),
        spec: Some(rpc::ApplicationSpec {
            shim: rpc::Shim::Host as i32,
            description: None,
            labels: vec![],
            image: None,
            command: None,
            arguments: vec![],
            environments: vec![],
            working_directory: None,
            max_instances: None,
            delay_release: None,
            schema: None,
            url: None,
            installer: None,
        }),
        status: Some(rpc::ApplicationStatus {
            state: rpc::ApplicationState::Enabled as i32,
            creation_time: wire_millis,
        }),
    };

    let parsed = Application::try_from(&rpc_app).expect("Application::try_from should succeed");
    assert_eq!(parsed.creation_time, expected);
}

mod node;
mod session_filter;

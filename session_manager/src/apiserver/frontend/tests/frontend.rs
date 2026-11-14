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

#[tokio::test]
async fn list_tasks_streams_all_active_tasks_then_finished_history() {
    use common::apis::{TaskResult, TaskState};

    for backend in ["none", "fs"] {
        let dir = tempfile::tempdir().unwrap();
        let url = match backend {
            "none" => "none".to_string(),
            _ => format!("fs://{}", dir.path().join("data").display()),
        };
        let (storage, controller, ssn_id) = task_test_controller(url).await;
        for index in 0..2003 {
            let task = controller
                .create_task(ssn_id.clone(), None, None)
                .await
                .unwrap();
            if index % 2 == 0 {
                let task_ptr = storage.get_task_ptr(task.gid()).unwrap();
                storage
                    .update_task_result(
                        storage.get_session_ptr(ssn_id.clone()).unwrap(),
                        task_ptr,
                        TaskResult {
                            state: TaskState::Succeed,
                            output: Some(bytes::Bytes::from_static(b"stored result")),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                assert!(storage.get_task_ptr(task.gid()).is_err());
            }
        }
        let flame = Flame {
            controller: controller.clone(),
            cluster_default_resreq: None,
        };
        let mut stream = Frontend::list_tasks(
            &flame,
            Request::new(ListTasksRequest {
                session_id: ssn_id.clone(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        let mut ids = Vec::new();
        while let Some(task) = stream.next().await {
            let task = task.unwrap();
            let id = task.metadata.as_ref().unwrap().id.parse::<i64>().unwrap();
            if id % 2 == 1 {
                assert_eq!(
                    task.spec.as_ref().unwrap().output.as_deref(),
                    Some(b"stored result".as_slice())
                );
            }
            ids.push(id);
        }
        let active_ids = ids
            .iter()
            .take(1001)
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(active_ids, (2..=2002).step_by(2).collect(), "{backend}");
        let unique_ids = ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), unique_ids.len(), "duplicate tasks for {backend}");
        assert_eq!(ids.len(), 2003, "{backend}");
        let finished_ids = ids
            .iter()
            .skip(1001)
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(finished_ids, (1..=2003).step_by(2).collect(), "{backend}");
    }
}

#[tokio::test]
async fn none_listing_includes_dequeued_pending_tasks() {
    let (storage, controller, ssn_id) = task_test_controller("none".to_string()).await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    {
        let session = storage.get_session_ptr(ssn_id.clone()).unwrap();
        let mut session = stdng::lock_ptr!(session).unwrap();
        assert!(session.pop_pending_task().is_some());
    }
    let flame = Flame {
        controller,
        cluster_default_resreq: None,
    };
    let mut stream = Frontend::list_tasks(
        &flame,
        Request::new(ListTasksRequest { session_id: ssn_id }),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        stream.next().await.unwrap().unwrap().metadata.unwrap().id,
        task.id.to_string()
    );
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn none_finished_task_remains_readable_from_get_watch_and_list() {
    let (storage, controller, ssn_id) = task_test_controller("none".to_string()).await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    storage
        .update_task_result(
            storage.get_session_ptr(ssn_id.clone()).unwrap(),
            storage.get_task_ptr(task.gid()).unwrap(),
            apis::TaskResult {
                state: apis::TaskState::Succeed,
                output: Some(bytes::Bytes::from_static(b"retained result")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(storage.get_task_ptr(task.gid()).is_err());
    let history = controller.get_task(ssn_id.clone(), task.id).await.unwrap();
    assert_eq!(history.state, apis::TaskState::Succeed);
    assert_eq!(
        history.output,
        Some(bytes::Bytes::from_static(b"retained result"))
    );
    assert!(!history.events.is_empty());
    let listed = controller.list_tasks(ssn_id.clone()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].gid(), task.gid());
    assert_eq!(listed[0].output, history.output);
    let session = controller.get_session(ssn_id.clone()).unwrap();
    assert_eq!(session.task_counts.succeed, 1);
    assert!(session.tasks.is_empty());

    let (requests_tx, requests) = watch_requests(&ssn_id, task.id);
    let (tx, mut rx) = mpsc::channel(1);
    let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
    let reported = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        reported.spec.unwrap().output.as_deref(),
        Some(b"retained result".as_slice())
    );
    let status = reported.status.unwrap();
    assert_eq!(status.state, rpc::TaskState::Succeed as i32);
    assert_eq!(status.events.len(), history.events.len());
    {
        let _requests_tx = requests_tx;
    }
    watcher.await.unwrap().unwrap();
}

#[tokio::test]
async fn late_watch_reads_evicted_terminal_result_and_events() {
    use common::apis::{TaskResult, TaskState};

    for backend in ["none", "fs"] {
        let dir = tempfile::tempdir().unwrap();
        let url = match backend {
            "none" => "none".to_string(),
            _ => format!("fs://{}", dir.path().join("data").display()),
        };
        let (storage, controller, ssn_id) = task_test_controller(url).await;
        let task = controller
            .create_task(ssn_id.clone(), None, None)
            .await
            .unwrap();
        storage
            .update_task_result(
                storage.get_session_ptr(ssn_id.clone()).unwrap(),
                storage.get_task_ptr(task.gid()).unwrap(),
                TaskResult {
                    state: TaskState::Failed,
                    message: Some("persisted failure".into()),
                    output: Some(bytes::Bytes::from_static(b"failure output")),
                },
            )
            .await
            .unwrap();
        assert!(storage.get_task_ptr(task.gid()).is_err());
        let (requests_tx, requests) = watch_requests(&ssn_id, task.id);
        let (tx, mut rx) = mpsc::channel(1);
        let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
        let reported = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            reported.spec.unwrap().output.as_deref(),
            Some(b"failure output".as_slice())
        );
        let status = reported.status.unwrap();
        assert_eq!(status.state, rpc::TaskState::Failed as i32);
        assert!(status
            .events
            .iter()
            .any(|event| event.message.as_deref() == Some("persisted failure")));
        // Ending registrations lets the stream finish after the terminal response.
        {
            let _requests_tx = requests_tx;
        }
        watcher.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn watch_tasks_omits_event_history_until_terminal_state() {
    let (controller, ssn_id) = watch_test_controller().await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    assert!(!controller
        .get_task(ssn_id.clone(), task.id)
        .await
        .unwrap()
        .events
        .is_empty());

    let (_requests_tx, requests) = watch_requests(&ssn_id, task.id);
    let (tx, mut rx) = mpsc::channel(1);
    let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
    let reported = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let status = reported.status.unwrap();
    assert_eq!(status.state, rpc::TaskState::Pending as i32);
    assert!(status.events.is_empty());
    drop(rx);
    watcher.await.unwrap().unwrap();
}

#[tokio::test]
async fn watch_tasks_includes_failed_task_event_message() {
    use common::apis::{TaskGID, TaskResult, TaskState};

    let (storage, controller, ssn_id) = watch_test_storage_and_controller().await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    let ssn_ptr = storage.get_session_ptr(ssn_id.clone()).unwrap();
    let task_ptr = storage
        .get_task_ptr(TaskGID {
            ssn_id: ssn_id.clone(),
            task_id: task.id,
        })
        .unwrap();
    storage
        .update_task_result(
            ssn_ptr,
            task_ptr,
            TaskResult {
                state: TaskState::Failed,
                message: Some("task failed on worker".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let (_requests_tx, requests) = watch_requests(&ssn_id, task.id);
    let (tx, mut rx) = mpsc::channel(1);
    let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
    let reported = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let status = reported.status.unwrap();
    assert_eq!(status.state, rpc::TaskState::Failed as i32);
    assert!(status
        .events
        .iter()
        .any(|event| { event.message.as_deref() == Some("task failed on worker") }));
    drop(rx);
    watcher.await.unwrap().unwrap();
}

#[tokio::test]
async fn watch_tasks_rejects_unknown_task() {
    let (controller, ssn_id) = watch_test_controller().await;
    let (_requests_tx, requests) = watch_requests(&ssn_id, 999);
    let (tx, _rx) = mpsc::channel(1);
    let result = watch_task_stream(controller, requests, tx).await;
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn watch_tasks_rejects_empty_registration_stream() {
    let (controller, _ssn_id) = watch_test_controller().await;
    let (_requests_tx, requests_rx) = mpsc::channel::<Result<WatchTaskRequest, Status>>(1);
    drop(_requests_tx);
    let (tx, _rx) = mpsc::channel(1);
    let result = watch_task_stream(controller, ReceiverStream::new(requests_rx), tx).await;
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn delete_node_rpc_removes_stale_nodes_and_reports_rejections() {
    let (storage, controller, session_id) = task_test_controller("none".into()).await;
    let flame = test_flame(controller.clone());
    for (name, state) in [
        ("old-worker", apis::NodeState::NotReady),
        ("ready-worker", apis::NodeState::Ready),
        ("occupied-worker", apis::NodeState::Unknown),
    ] {
        storage
            .register_node(&apis::Node {
                name: name.into(),
                state,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    controller
        .create_executor("occupied-worker".into(), session_id)
        .await
        .unwrap();
    for (name, code) in [
        ("", tonic::Code::InvalidArgument),
        ("missing-worker", tonic::Code::NotFound),
        ("ready-worker", tonic::Code::InvalidArgument),
        ("occupied-worker", tonic::Code::InvalidArgument),
    ] {
        let error = Frontend::delete_node(
            &flame,
            Request::new(DeleteNodeRequest { name: name.into() }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), code);
    }
    let result = Frontend::delete_node(
        &flame,
        Request::new(DeleteNodeRequest {
            name: "old-worker".into(),
        }),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(result.return_code, 0);
    let listed = Frontend::list_nodes(&flame, Request::new(ListNodesRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.nodes.len(), 2);
    let error = Frontend::get_node(
        &flame,
        Request::new(GetNodeRequest {
            name: "old-worker".into(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn suspend_and_resume_session_round_trip() {
    let (controller, session_id) = watch_test_controller().await;
    let flame = test_flame(controller);

    let suspended = flame
        .suspend_session(Request::new(SuspendSessionRequest {
            session_id: session_id.clone(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        suspended.status.unwrap().state,
        rpc::SessionState::Suspending as i32
    );
    // No executor serves the session, so the scheduler completes the suspend.
    flame
        .controller
        .suspend_session(session_id.clone())
        .await
        .unwrap();

    let resumed = flame
        .resume_session(Request::new(ResumeSessionRequest { session_id }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        resumed.status.unwrap().state,
        rpc::SessionState::Open as i32
    );
}

#[tokio::test]
async fn suspend_session_maps_errors() {
    let (controller, session_id) = watch_test_controller().await;
    let flame = test_flame(controller);

    let status = flame
        .suspend_session(Request::new(SuspendSessionRequest {
            session_id: "missing".to_string(),
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::NotFound);

    flame
        .controller
        .close_session(session_id.clone())
        .await
        .unwrap();
    assert!(flame
        .suspend_session(Request::new(SuspendSessionRequest { session_id }))
        .await
        .is_err());
}

#[tokio::test]
async fn watch_tasks_rejects_registration_after_session_close() {
    let (controller, ssn_id) = watch_test_controller().await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    controller.close_session(ssn_id.clone()).await.unwrap();

    let (_requests_tx, requests) = watch_requests(&ssn_id, task.id);
    let (tx, _rx) = mpsc::channel(1);
    let result = watch_task_stream(controller, requests, tx).await;
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::NotFound);
    assert_eq!(status.message(), "session is closed");
}

#[tokio::test]
async fn watch_tasks_reports_terminal_state_then_closes_on_session_close() {
    let (controller, ssn_id) = watch_test_controller().await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    let (_requests_tx, requests) = watch_requests(&ssn_id, task.id);
    let (tx, mut rx) = mpsc::channel(2);
    let watcher = tokio::spawn(watch_task_stream(controller.clone(), requests, tx));
    rx.recv().await.unwrap().unwrap();

    controller.close_session(ssn_id.clone()).await.unwrap();
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal.status.unwrap().state,
        rpc::TaskState::Cancelled as i32
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
        .await
        .unwrap()
        .unwrap();
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::NotFound);
    assert_eq!(status.message(), "session task watch closed");
}

#[tokio::test]
async fn watch_tasks_reports_only_registered_ids_on_one_stream() {
    let (controller, ssn_id) = watch_test_controller().await;
    let first = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    let second = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    let _unregistered = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();

    let (requests_tx, requests) = watch_requests(&ssn_id, first.id);
    let (tx, mut rx) = mpsc::channel(2);
    let watcher = tokio::spawn(watch_task_stream(controller.clone(), requests, tx));
    let initial = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(initial.metadata.unwrap().id, first.id.to_string());
    assert_eq!(
        initial.status.unwrap().state,
        rpc::TaskState::Pending as i32
    );
    // The second task exists in the session but has not been registered.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .is_err()
    );

    requests_tx
        .send(Ok(watch_request(&ssn_id, first.id)))
        .await
        .unwrap();
    let repeated_snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(repeated_snapshot.metadata.unwrap().id, first.id.to_string());
    assert_eq!(
        repeated_snapshot.status.unwrap().state,
        rpc::TaskState::Pending as i32
    );

    requests_tx
        .send(Ok(watch_request(&ssn_id, second.id)))
        .await
        .unwrap();
    drop(requests_tx);
    let second_initial = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second_initial.metadata.unwrap().id, second.id.to_string());
    assert_eq!(
        second_initial.status.unwrap().state,
        rpc::TaskState::Pending as i32
    );

    controller.close_session(ssn_id.clone()).await.unwrap();
    let mut completed = HashSet::new();
    for _ in 0..2 {
        let reported = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            reported.status.unwrap().state,
            rpc::TaskState::Cancelled as i32
        );
        completed.insert(reported.metadata.unwrap().id);
    }
    assert_eq!(
        completed,
        HashSet::from([first.id.to_string(), second.id.to_string()])
    );
    let status = watcher.await.unwrap().unwrap_err();
    assert_eq!(status.code(), tonic::Code::NotFound);
    assert_eq!(status.message(), "session task watch closed");
}

#[tokio::test]
async fn watch_tasks_stops_when_response_stream_is_dropped() {
    let (controller, ssn_id) = watch_test_controller().await;
    let task = controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    let (_requests_tx, requests) = watch_requests(&ssn_id, task.id);
    let (tx, rx) = mpsc::channel(1);
    let watcher = tokio::spawn(watch_task_stream(controller, requests, tx));
    tokio::task::yield_now().await;
    drop(rx);
    tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
        .await
        .expect("watch must stop after response stream closes")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn watch_tasks_stops_before_first_request_when_response_stream_is_dropped() {
    let (controller, _ssn_id) = watch_test_controller().await;
    let (_requests_tx, requests_rx) = mpsc::channel::<Result<WatchTaskRequest, Status>>(1);
    let (tx, rx) = mpsc::channel(1);
    let watcher = tokio::spawn(watch_task_stream(
        controller,
        ReceiverStream::new(requests_rx),
        tx,
    ));
    drop(rx);
    tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
        .await
        .expect("watch must stop before its first request when response stream closes")
        .unwrap()
        .unwrap();
}

#[test]
fn explicit_resreq_returned_verbatim() {
    let explicit = rr(2, 2 * 1024 * 1024 * 1024, 1);
    let cluster_default = Some(rr(4, 8 * 1024 * 1024 * 1024, 0));
    let res = resolve_session_resreq(Some(explicit.clone()), cluster_default.as_ref());
    assert_eq!(
        res, explicit,
        "explicit resreq must be returned verbatim and override the cluster default"
    );
}

#[test]
fn cluster_default_used_when_explicit_unset() {
    let cluster_default = rr(4, 8 * 1024 * 1024 * 1024, 2);
    let res = resolve_session_resreq(None, Some(&cluster_default));
    assert_eq!(
        res, cluster_default,
        "should clone the cluster default when no explicit resreq is supplied"
    );
}

#[test]
fn hardcoded_fallback_when_no_cluster_default() {
    let res = resolve_session_resreq(None, None);
    assert_eq!(
        res, DEFAULT_FALLBACK_RESREQ,
        "should yield the hardcoded fallback (cpu=1, mem=1GiB, gpu=0)"
    );
}

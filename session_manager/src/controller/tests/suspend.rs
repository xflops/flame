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
use tokio::time::sleep;

const NODE: &str = "suspend-node";

async fn memory_controller() -> ControllerPtr {
    let config = FlameClusterContext {
        cluster: FlameCluster {
            storage: "none".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = crate::storage::new_ptr(&config).await.unwrap();
    let controller = new_ptr(storage);
    controller
        .register_application("test-app".to_string(), create_test_application())
        .await
        .unwrap();
    controller
        .storage()
        .register_node(&create_test_node(NODE))
        .await
        .unwrap();
    controller
}

async fn open(controller: &ControllerPtr, ssn_id: &str) {
    controller
        .create_session(create_test_session_attr(ssn_id))
        .await
        .unwrap();
}

/// Assigns a new executor to the session; `bound` also completes the bind.
async fn assign(controller: &ControllerPtr, ssn_id: &str, bound: bool) -> String {
    let executor = controller
        .create_executor(NODE.to_string(), ssn_id.to_string())
        .await
        .unwrap();
    controller.register_executor(&executor).await.unwrap();
    controller
        .bind_session(executor.id.clone(), ssn_id.to_string())
        .await
        .unwrap();
    if bound {
        controller
            .bind_executor_completed(executor.id.clone(), None, None)
            .await
            .unwrap();
    }
    executor.id
}

fn state_of(controller: &ControllerPtr, ssn_id: &str) -> SessionState {
    controller
        .get_session(ssn_id.to_string())
        .unwrap()
        .status
        .state
}

/// The scheduler must never see an executor naming a session it cannot find.
fn assert_snapshot_consistent(controller: &ControllerPtr) {
    let snapshot = controller.storage().snapshot().unwrap();
    for executor in &snapshot.executors {
        if let Some(ssn_id) = executor.ssn_id.as_ref() {
            assert!(
                snapshot.sessions.iter().any(|ssn| &ssn.id == ssn_id),
                "executor <{}> references session <{}> missing from the snapshot",
                executor.id,
                ssn_id
            );
        }
    }
}

fn succeed() -> TaskResult {
    TaskResult {
        state: TaskState::Succeed,
        output: None,
        message: None,
    }
}

async fn unbind(controller: &ControllerPtr, executor_id: &str) {
    controller
        .unbind_executor(executor_id.to_string())
        .await
        .unwrap();
    controller
        .unbind_executor_completed(executor_id.to_string())
        .await
        .unwrap();
}

#[tokio::test]
async fn suspend_without_executor_completes_in_suspend_session() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;

    let session = controller
        .signal_session("ssn".into(), Signal::Suspend)
        .await
        .unwrap();
    assert!(session.is_suspending());
    assert_eq!(controller.storage().snapshot().unwrap().sessions.len(), 1);

    controller.suspend_session("ssn".into()).await.unwrap();
    assert_eq!(state_of(&controller, "ssn"), SessionState::Suspended);
    assert!(controller.storage().snapshot().unwrap().sessions.is_empty());
    // Completing again, or an unknown/open session, changes nothing.
    controller.suspend_session("ssn".into()).await.unwrap();
    assert_eq!(state_of(&controller, "ssn"), SessionState::Suspended);

    let session = controller.resume_session("ssn".into()).await.unwrap();
    assert!(session.is_open());
    controller.suspend_session("ssn".into()).await.unwrap();
    assert_eq!(state_of(&controller, "ssn"), SessionState::Open);
}

#[tokio::test]
async fn suspend_unbinds_bound_executor_then_suspends() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;
    let executor_id = assign(&controller, "ssn", true).await;

    let waiting = {
        let controller = controller.clone();
        let executor_id = executor_id.clone();
        tokio::spawn(async move { controller.launch_task(executor_id).await })
    };
    sleep(Duration::from_millis(20)).await;

    let session = controller
        .signal_session("ssn".into(), Signal::Suspend)
        .await
        .unwrap();
    assert!(session.is_suspending());
    assert!(waiting.await.unwrap().unwrap().is_none());
    assert_snapshot_consistent(&controller);

    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    assert_snapshot_consistent(&controller);

    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    let executor = controller.get_executor(executor_id).unwrap();
    assert_eq!(executor.state, ExecutorState::Idle);
    assert_eq!(executor.ssn_id, None);
    assert_snapshot_consistent(&controller);
}

#[tokio::test]
async fn suspend_lets_running_task_finish() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;
    let executor_id = assign(&controller, "ssn", true).await;
    let task = controller
        .create_task("ssn".into(), None, None)
        .await
        .unwrap();
    let launched = controller.launch_task(executor_id.clone()).await.unwrap();
    assert_eq!(launched.unwrap().id, task.id);

    controller
        .signal_session("ssn".into(), Signal::Suspend)
        .await
        .unwrap();
    controller
        .complete_task(executor_id.clone(), succeed(), None)
        .await
        .unwrap();
    assert_eq!(
        controller
            .get_task("ssn".into(), task.id)
            .await
            .unwrap()
            .state,
        TaskState::Succeed
    );
    // The executor still serves the session until it unbinds.

    assert!(controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    unbind(&controller, &executor_id).await;
    assert_snapshot_consistent(&controller);
}

#[tokio::test]
async fn tasks_submitted_while_suspended_run_after_resume() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;
    let executor_id = assign(&controller, "ssn", true).await;
    controller
        .signal_session("ssn".into(), Signal::Suspend)
        .await
        .unwrap();

    // Suspending: accepted, never handed out.
    let first = controller
        .create_task("ssn".into(), None, None)
        .await
        .unwrap();
    assert!(controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    unbind(&controller, &executor_id).await;
    controller.suspend_session("ssn".into()).await.unwrap();
    assert_eq!(state_of(&controller, "ssn"), SessionState::Suspended);

    // Suspended: accepted, and the session takes no executor.
    let second = controller
        .create_task("ssn".into(), None, None)
        .await
        .unwrap();
    assert!(matches!(
        controller
            .bind_session(executor_id.clone(), "ssn".into())
            .await,
        Err(FlameError::InvalidState(_))
    ));
    for id in [first.id, second.id] {
        assert_eq!(
            controller.get_task("ssn".into(), id).await.unwrap().state,
            TaskState::Pending
        );
    }

    controller.resume_session("ssn".into()).await.unwrap();
    controller
        .bind_session(executor_id.clone(), "ssn".into())
        .await
        .unwrap();
    controller
        .bind_executor_completed(executor_id.clone(), None, None)
        .await
        .unwrap();
    let task = controller
        .launch_task(executor_id)
        .await
        .unwrap()
        .expect("pending task runs after resume");
    assert_eq!(task.id, first.id);
}

#[tokio::test]
async fn suspend_requires_an_open_session() {
    let controller = memory_controller().await;
    open(&controller, "closed").await;
    controller.close_session("closed".into()).await.unwrap();
    assert!(matches!(
        controller
            .signal_session("closed".into(), Signal::Suspend)
            .await,
        Err(FlameError::InvalidState(_))
    ));

    open(&controller, "suspended").await;
    controller
        .signal_session("suspended".into(), Signal::Suspend)
        .await
        .unwrap();
    assert!(matches!(
        controller
            .signal_session("suspended".into(), Signal::Suspend)
            .await,
        Err(FlameError::InvalidState(_))
    ));

    open(&controller, "suspending").await;
    assign(&controller, "suspending", true).await;
    controller
        .signal_session("suspending".into(), Signal::Suspend)
        .await
        .unwrap();
    assert!(matches!(
        controller
            .signal_session("suspending".into(), Signal::Suspend)
            .await,
        Err(FlameError::InvalidState(_))
    ));
    assert!(matches!(
        controller.resume_session("suspending".into()).await,
        Err(FlameError::InvalidState(_))
    ));
    assert!(matches!(
        controller.resume_session("closed".into()).await,
        Err(FlameError::InvalidState(_))
    ));
}

#[tokio::test]
async fn resume_of_open_session_is_a_no_op() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;
    let session = controller.resume_session("ssn".into()).await.unwrap();
    assert!(session.is_open());
}

#[tokio::test]
async fn binding_executor_delays_suspend_until_it_unbinds() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;
    let executor_id = assign(&controller, "ssn", false).await;

    let session = controller
        .signal_session("ssn".into(), Signal::Suspend)
        .await
        .unwrap();
    assert!(session.is_suspending());
    // A refused bind hands the executor back with no session.
    assert!(controller
        .wait_for_session(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    assert_snapshot_consistent(&controller);

    controller
        .bind_executor_completed(executor_id.clone(), None, None)
        .await
        .unwrap();
    assert!(controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    unbind(&controller, &executor_id).await;
    assert_snapshot_consistent(&controller);
}

#[tokio::test]
async fn suspend_session_ignores_a_session_that_is_not_suspending() {
    let controller = memory_controller().await;

    // Resumed or closed after the scheduler's check: nothing to complete.
    open(&controller, "open").await;
    controller.suspend_session("open".into()).await.unwrap();
    assert_eq!(state_of(&controller, "open"), SessionState::Open);

    open(&controller, "suspended").await;
    controller
        .signal_session("suspended".into(), Signal::Suspend)
        .await
        .unwrap();
    controller
        .suspend_session("suspended".into())
        .await
        .unwrap();
    controller
        .suspend_session("suspended".into())
        .await
        .unwrap();
    assert_eq!(state_of(&controller, "suspended"), SessionState::Suspended);

    open(&controller, "closed").await;
    controller.close_session("closed".into()).await.unwrap();
    controller.suspend_session("closed".into()).await.unwrap();
    assert_eq!(state_of(&controller, "closed"), SessionState::Closed);
}

#[tokio::test]
async fn close_wins_over_suspending() {
    let controller = memory_controller().await;
    open(&controller, "ssn").await;
    assign(&controller, "ssn", true).await;
    controller
        .signal_session("ssn".into(), Signal::Suspend)
        .await
        .unwrap();

    controller.close_session("ssn".into()).await.unwrap();
    controller.suspend_session("ssn".into()).await.unwrap();
    assert_eq!(state_of(&controller, "ssn"), SessionState::Closed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_never_pairs_a_suspended_session_with_its_executor() {
    let controller = memory_controller().await;
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Sessions and executors exist before the reader starts, so only the
    // suspend changes what the snapshot shows.
    let mut pairs = Vec::new();
    for index in 0..20 {
        let ssn_id = format!("ssn-{index}");
        open(&controller, &ssn_id).await;
        pairs.push((ssn_id.clone(), assign(&controller, &ssn_id, true).await));
    }
    let reader = {
        let controller = controller.clone();
        let done = done.clone();
        tokio::spawn(async move {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                assert_snapshot_consistent(&controller);
                tokio::task::yield_now().await;
            }
        })
    };

    for (ssn_id, executor_id) in pairs {
        controller
            .signal_session(ssn_id.clone(), Signal::Suspend)
            .await
            .unwrap();
        unbind(&controller, &executor_id).await;
        controller.suspend_session(ssn_id.clone()).await.unwrap();
        assert_eq!(state_of(&controller, &ssn_id), SessionState::Suspended);
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.await.unwrap();
}

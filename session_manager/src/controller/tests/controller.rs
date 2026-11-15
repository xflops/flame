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

#[test]
fn task_options_reject_invalid_affinity_keys() {
    assert!(Controller::validate_task_options(&TaskOptions {
        affinity: std::collections::HashSet::from([bytes::Bytes::new()]),
    })
    .is_err());
    assert!(Controller::validate_task_options(&TaskOptions {
        affinity: std::collections::HashSet::from([bytes::Bytes::from(vec![0; 257])]),
    })
    .is_err());
}

#[test]
fn executor_attributes_reject_invalid_keys_before_indexing() {
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: vec![vec![]],
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: vec![vec![0; 257]],
            ..Default::default()
        })
        .is_err()
    );

    let attributes = Controller::validate_executor_attributes(rpc::ExecutorAttributes {
        publish: vec![b"kv-cache-key".to_vec(), b"kv-cache-key".to_vec()],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        attributes.publish,
        HashSet::from([bytes::Bytes::from_static(b"kv-cache-key")])
    );
    assert!(attributes.retract.is_empty());

    let byte_boundary = (0_u16..256)
        .map(|index| {
            let mut value = index.to_be_bytes().to_vec();
            value.resize(Controller::MAX_EXECUTOR_ATTRIBUTE_BYTES, 0);
            value
        })
        .collect::<Vec<_>>();
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: byte_boundary.clone(),
            ..Default::default()
        })
        .is_ok()
    );

    let mut over_byte_limit = byte_boundary;
    over_byte_limit.push(b"overflow".to_vec());
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: over_byte_limit,
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn executor_attribute_retractions_are_validated_with_the_whole_update() {
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            retract: vec![vec![]],
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            retract: vec![vec![0; 257]],
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: vec![b"x".to_vec()],
            retract: vec![b"x".to_vec()],
        })
        .is_err()
    );
    let too_many = (0..=Controller::MAX_EXECUTOR_ATTRIBUTES)
        .map(|index| format!("key-{index}").into_bytes())
        .collect::<Vec<_>>();
    assert!(
        Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: vec![b"ok".to_vec()],
            retract: too_many,
        })
        .is_err()
    );

    let update = Controller::validate_executor_attributes(rpc::ExecutorAttributes {
        publish: vec![b"x".to_vec()],
        retract: vec![b"y".to_vec(), b"y".to_vec()],
    })
    .unwrap();
    assert_eq!(update.publish.len(), 1);
    assert_eq!(update.retract.len(), 1);
}

#[tokio::test]
async fn new_tasks_and_idle_executors_wake_scheduler() {
    use tokio::time::Duration;

    let controller = new_ptr(create_test_storage().await);
    controller
        .register_application("test-app".to_string(), create_test_application())
        .await
        .unwrap();

    controller
        .create_session(create_test_session_attr("wake-session"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            controller.wait_for_scheduler_event(Duration::from_secs(60)),
        )
        .await
        .is_err(),
        "a session without tasks must not wake the scheduler"
    );

    controller
        .create_task("wake-session".to_string(), None, None)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(100),
        controller.wait_for_scheduler_event(Duration::from_secs(60)),
    )
    .await
    .expect("new task should wake scheduler");

    controller
        .storage()
        .register_node(&create_test_node("wake-node"))
        .await
        .unwrap();
    let executor = controller
        .create_executor("wake-node".to_string(), "wake-session".to_string())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            controller.wait_for_scheduler_event(Duration::from_secs(60)),
        )
        .await
        .is_err(),
        "scheduler-owned executor creation must not wake the scheduler"
    );

    controller.register_executor(&executor).await.unwrap();
    tokio::time::timeout(
        Duration::from_millis(100),
        controller.wait_for_scheduler_event(Duration::from_secs(60)),
    )
    .await
    .expect("newly Idle executor should wake scheduler");

    controller
        .bind_session(executor.id.clone(), "wake-session".to_string())
        .await
        .unwrap();
    controller
        .bind_executor_completed(executor.id.clone(), None, None)
        .await
        .unwrap();
    controller
        .unbind_executor(executor.id.clone())
        .await
        .unwrap();
    controller
        .unbind_executor_completed(executor.id.clone())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(100),
        controller.wait_for_scheduler_event(Duration::from_secs(60)),
    )
    .await
    .expect("executor becoming Idle after unbind should wake scheduler");
}

#[tokio::test]
async fn unregister_missing_executor_succeeds() {
    let controller = new_ptr(create_test_storage().await);
    controller
        .unregister_executor("missing-executor".into())
        .await
        .unwrap();
    assert!(matches!(
        controller.get_executor("missing-executor".into()),
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn unregister_executor_is_idempotent() {
    let controller = new_ptr(create_test_storage().await);
    let id = create_binding_executor(&controller, "unregister-idempotent").await;
    controller.unregister_executor(id.clone()).await.unwrap();
    controller.unregister_executor(id.clone()).await.unwrap();
    assert!(matches!(
        controller.get_executor(id),
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn missing_active_tasks_allow_executor_cleanup_without_changing_memory_history() {
    for operation in ["launch", "complete", "unregister"] {
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        let (controller, executor_id, task) = completed_task_with_stale_executor_in(storage).await;
        assert!(matches!(
            controller.storage().get_task_ptr(task.gid()),
            Err(FlameError::NotFound(_))
        ));
        let before = controller
            .get_task(task.ssn_id.clone(), task.id)
            .await
            .unwrap();
        match operation {
            "launch" => assert!(controller
                .launch_task(executor_id.clone())
                .await
                .unwrap()
                .is_none()),
            "complete" => controller
                .complete_task(
                    executor_id.clone(),
                    TaskResult {
                        state: TaskState::Failed,
                        output: Some(bytes::Bytes::from_static(b"late result")),
                        message: None,
                    },
                    None,
                )
                .await
                .unwrap(),
            "unregister" => {
                let mut executor = controller.get_executor(executor_id.clone()).unwrap();
                executor.state = ExecutorState::Releasing;
                controller
                    .storage()
                    .update_executor(&executor)
                    .await
                    .unwrap();
                controller
                    .unregister_executor(executor_id.clone())
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        if operation == "unregister" {
            assert!(matches!(
                controller.get_executor(executor_id),
                Err(FlameError::NotFound(_))
            ));
        } else {
            assert_eq!(controller.get_executor(executor_id).unwrap().task_id, None);
        }
        let after = controller
            .get_task(task.ssn_id.clone(), task.id)
            .await
            .unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.version, before.version);
        assert_eq!(after.output, before.output);
        assert_eq!(after.events.len(), before.events.len());
        let session = controller.get_session(task.ssn_id).unwrap();
        assert_eq!(session.task_counts.succeed, 1);
        assert_eq!(session.task_counts.failed, 0);
        assert!(session.tasks.is_empty());
    }
}

#[tokio::test]
async fn terminal_cleanup_rejects_ownership_changed_during_lookup() {
    let (controller, executor_id, task) = completed_task_with_stale_executor().await;
    let executor = controller.storage().get_executor_ptr(executor_id).unwrap();
    for (session, task_id) in [
        (task.ssn_id.clone(), task.id + 1),
        ("a-new-session".to_string(), task.id),
    ] {
        {
            let mut executor = lock_ptr!(executor).unwrap();
            executor.ssn_id = Some(session.clone());
            executor.task_id = Some(task_id);
        }

        let result = Controller::unassign_task(&executor, &task.gid());

        assert!(matches!(result, Err(FlameError::InvalidState(_))));
        let current = lock_ptr!(executor).unwrap();
        assert_eq!(current.ssn_id.as_ref(), Some(&session));
        assert_eq!(current.task_id, Some(task_id));
    }
}

#[tokio::test]
async fn launch_skips_an_assigned_task_on_cache_miss() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());
    let executor_id = create_binding_executor(&controller, "missed-task-session").await;
    controller
        .bind_executor_completed(executor_id.clone(), None, None)
        .await
        .unwrap();
    let task = controller
        .create_task("missed-task-session".into(), None, None)
        .await
        .unwrap();
    controller.launch_task(executor_id.clone()).await.unwrap();
    {
        let session = storage.get_session_ptr(task.ssn_id.clone()).unwrap();
        let mut session = lock_ptr!(session).unwrap();
        session.tasks.remove(&task.id);
        for index in session.tasks_index.values_mut() {
            index.remove(&task.id);
        }
    }

    assert!(controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    assert_eq!(controller.get_executor(executor_id).unwrap().task_id, None);
    assert_eq!(
        storage
            .get_task_metadata(task.ssn_id.clone(), task.id)
            .await
            .unwrap()
            .state,
        TaskState::Running
    );
    assert!(matches!(
        storage.get_task_ptr(task.gid()),
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn launch_does_not_replay_an_evicted_terminal_task() {
    let (controller, executor_id, task) = completed_task_with_stale_executor().await;

    assert!(controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    assert_eq!(controller.get_executor(executor_id).unwrap().task_id, None);
    assert!(matches!(
        controller.storage().get_task_ptr(task.gid()),
        Err(FlameError::NotFound(_))
    ));
    assert_eq!(
        controller
            .get_task(task.ssn_id, task.id)
            .await
            .unwrap()
            .state,
        TaskState::Succeed
    );
}

#[tokio::test]
async fn late_completion_keeps_the_committed_terminal_result() {
    let (controller, executor_id, task) = completed_task_with_stale_executor().await;
    let before = controller
        .get_task(task.ssn_id.clone(), task.id)
        .await
        .unwrap();

    controller
        .complete_task(
            executor_id.clone(),
            TaskResult {
                state: TaskState::Failed,
                output: Some(bytes::Bytes::from_static(b"late result")),
                message: Some("late failure".into()),
            },
            None,
        )
        .await
        .unwrap();

    let after = controller
        .get_task(task.ssn_id.clone(), task.id)
        .await
        .unwrap();
    assert_eq!(after.state, TaskState::Succeed);
    assert_eq!(after.version, before.version);
    assert_eq!(after.output, before.output);
    assert_eq!(after.events.len(), before.events.len());
    assert_eq!(controller.get_executor(executor_id).unwrap().task_id, None);
    let session = controller.get_session(task.ssn_id).unwrap();
    assert_eq!(session.task_counts.succeed, 1);
    assert_eq!(session.task_counts.failed, 0);
}

#[tokio::test]
async fn late_terminal_completion_preserves_unbinding_state_and_clears_ownership() {
    let (controller, executor_id, task) = completed_task_with_stale_executor().await;
    let mut executor = controller.get_executor(executor_id.clone()).unwrap();
    executor.state = ExecutorState::Unbinding;
    controller
        .storage()
        .update_executor(&executor)
        .await
        .unwrap();

    controller
        .complete_task(
            executor_id.clone(),
            TaskResult {
                state: TaskState::Succeed,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

    let executor = controller.get_executor(executor_id).unwrap();
    assert_eq!(executor.state, ExecutorState::Unbinding);
    assert_eq!(executor.ssn_id, None);
    assert_eq!(executor.task_id, None);
    assert_eq!(
        controller
            .get_task(task.ssn_id, task.id)
            .await
            .unwrap()
            .state,
        TaskState::Succeed
    );
}

#[tokio::test]
async fn unregister_keeps_an_evicted_terminal_task_successful() {
    let (controller, executor_id, task) = completed_task_with_stale_executor().await;
    let mut executor = controller.get_executor(executor_id.clone()).unwrap();
    executor.state = ExecutorState::Releasing;
    controller
        .storage()
        .update_executor(&executor)
        .await
        .unwrap();

    controller
        .unregister_executor(executor_id.clone())
        .await
        .unwrap();

    assert!(matches!(
        controller.get_executor(executor_id),
        Err(FlameError::NotFound(_))
    ));
    assert_eq!(
        controller
            .get_task(task.ssn_id.clone(), task.id)
            .await
            .unwrap()
            .state,
        TaskState::Succeed
    );
    let session = controller.get_session(task.ssn_id).unwrap();
    assert_eq!(session.task_counts.succeed, 1);
    assert_eq!(session.task_counts.failed, 0);
}

#[tokio::test]
async fn minimum_binding_waits_for_tasks_and_session_close() {
    use tokio::time::{timeout, Duration};
    let (controller, executor_id) = bound_task_waiter(1).await;
    let waiting = controller.launch_task(executor_id.clone());
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());
    assert_eq!(
        controller.get_executor(executor_id.clone()).unwrap().state,
        ExecutorState::Bound
    );
    controller
        .create_task("task-wait-session".into(), None, None)
        .await
        .unwrap();
    let task = timeout(Duration::from_millis(100), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        controller
            .get_task("task-wait-session".into(), task.id)
            .await
            .unwrap()
            .state,
        TaskState::Running
    );
    controller
        .complete_task(
            executor_id.clone(),
            TaskResult {
                state: TaskState::Succeed,
                output: None,
                message: None,
            },
            None,
        )
        .await
        .unwrap();

    let waiting = controller.launch_task(executor_id.clone());
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());
    controller
        .close_session("task-wait-session".into())
        .await
        .unwrap();
    assert!(timeout(Duration::from_millis(100), waiting)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    assert_eq!(
        controller.get_executor(executor_id.clone()).unwrap().state,
        ExecutorState::Unbinding
    );
}

#[tokio::test]
async fn closed_task_subscription_releases_minimum_waiter() {
    use tokio::time::{timeout, Duration};
    let (controller, executor_id) = bound_task_waiter(1).await;
    let waiting = controller.launch_task(executor_id.clone());
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());
    controller
        .notifier
        .tasks
        .remove(&"task-wait-session".into())
        .unwrap();
    assert!(timeout(Duration::from_millis(100), waiting)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    assert_eq!(
        controller.get_executor(executor_id.clone()).unwrap().state,
        ExecutorState::Unbinding
    );
}

#[tokio::test]
async fn lagged_executor_wakeups_keep_task_waiter_until_interrupted() {
    use tokio::time::{timeout, Duration};
    let (controller, executor_id) = bound_task_waiter(0).await;
    let waiting = controller.launch_task(executor_id.clone());
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());

    // Overflow the bounded wakeup queue without polling the waiter.
    for _ in 0..4096 {
        controller
            .notifier
            .executors
            .notify(&executor_id, executor_id.clone())
            .unwrap();
    }
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());
    controller
        .interrupt_executor(executor_id.clone())
        .await
        .unwrap();
    assert!(timeout(Duration::from_millis(100), waiting)
        .await
        .unwrap()
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn scheduler_unbind_wakes_task_wait_and_preserves_pending_work() {
    use tokio::time::{timeout, Duration};
    let (controller, executor_id) = bound_task_waiter(0).await;
    let waiting = controller.launch_task(executor_id.clone());
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(20), &mut waiting)
        .await
        .is_err());
    controller
        .interrupt_executor(executor_id.clone())
        .await
        .unwrap();
    let task = controller
        .create_task("task-wait-session".into(), None, None)
        .await
        .unwrap();
    assert!(timeout(Duration::from_millis(100), waiting)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    assert_eq!(
        controller
            .get_task("task-wait-session".into(), task.id)
            .await
            .unwrap()
            .state,
        TaskState::Pending
    );
    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    assert_eq!(
        controller.get_executor(executor_id.clone()).unwrap().state,
        ExecutorState::Idle
    );
}

#[tokio::test]
async fn interrupt_during_task_launch_preserves_completion_and_stops_next_poll() {
    let (controller, executor_id) = bound_task_waiter(0).await;
    let task = controller
        .create_task("task-wait-session".into(), None, None)
        .await
        .unwrap();
    let executor = controller
        .storage
        .get_executor_ptr(executor_id.clone())
        .unwrap();
    let session = controller
        .storage
        .get_session_ptr("task-wait-session".into())
        .unwrap();
    let task_ptr = controller
        .wait_for_task(&session, &executor)
        .await
        .unwrap()
        .unwrap();
    let state = executors::from(controller.storage.clone(), executor.clone()).unwrap();
    // Reproduce Shuffle interrupting after Pending is popped but before
    // launch_task persists Running and publishes executor task ownership.
    controller
        .interrupt_executor(executor_id.clone())
        .await
        .unwrap();
    controller
        .interrupt_executor(executor_id.clone())
        .await
        .unwrap();
    assert_eq!(lock_ptr!(executor).unwrap().state, ExecutorState::Bound);
    state.launch_task(session, task_ptr).await.unwrap().unwrap();
    // Completion retry must still recover this task before handling the signal.
    assert_eq!(
        controller
            .launch_task(executor_id.clone())
            .await
            .unwrap()
            .unwrap()
            .id,
        task.id
    );
    controller
        .complete_task(
            executor_id.clone(),
            TaskResult {
                state: TaskState::Succeed,
                output: None,
                message: None,
            },
            Some(rpc::ExecutorAttributes {
                publish: vec![b"completed-key".to_vec()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        controller
            .get_task("task-wait-session".into(), task.id)
            .await
            .unwrap()
            .state,
        TaskState::Succeed
    );
    assert!(controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .is_none());
    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    let leaving = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(leaving.state, ExecutorState::Unbinding);
    assert!(leaving.is_interrupted());
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    let idle = controller.get_executor(executor_id).unwrap();
    assert_eq!(idle.state, ExecutorState::Idle);
    assert!(!idle.is_interrupted());
}

#[tokio::test]
async fn executor_accumulates_attributes_and_retains_them_on_unbind() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    let ssn_id = "executor-attributes-unbind".to_string();
    let executor_id = create_binding_executor(&controller, &ssn_id).await;

    controller
        .bind_executor_completed(
            executor_id.clone(),
            Some(FlameResult {
                return_code: 0,
                message: None,
            }),
            Some(rpc::ExecutorAttributes {
                publish: vec![b"kv-cache-key".to_vec()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let executor = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(executor.application, "test-app");
    assert_eq!(
        executor.attributes,
        HashSet::from([bytes::Bytes::from_static(b"kv-cache-key")])
    );

    controller
        .update_executor_attributes(&executor_id, ExecutorAttributeUpdate::default())
        .unwrap();
    assert_eq!(
        controller
            .get_executor(executor_id.clone())
            .unwrap()
            .attributes,
        HashSet::from([bytes::Bytes::from_static(b"kv-cache-key")])
    );
    controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .unwrap();
    controller
        .complete_task(
            executor_id.clone(),
            TaskResult {
                state: TaskState::Succeed,
                message: None,
                output: None,
            },
            Some(rpc::ExecutorAttributes {
                publish: vec![b"second-key".to_vec()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();

    let expected_attributes = HashSet::from([
        bytes::Bytes::from_static(b"kv-cache-key"),
        bytes::Bytes::from_static(b"second-key"),
    ]);
    assert_eq!(
        controller
            .get_executor(executor_id.clone())
            .unwrap()
            .attributes,
        expected_attributes
    );

    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();

    let executor = controller.get_executor(executor_id.clone()).unwrap();
    assert_eq!(executor.application, "test-app");
    assert_eq!(executor.attributes, expected_attributes);

    let snapshot = controller.snapshot().unwrap();
    let executor = snapshot
        .find_executors(None)
        .unwrap()
        .remove(&executor_id)
        .unwrap();
    assert_eq!(executor.application, "test-app");
    assert_eq!(executor.attributes, expected_attributes);

    controller
        .release_executor(executor_id.clone())
        .await
        .unwrap();
    controller
        .unregister_executor(executor_id.clone())
        .await
        .unwrap();
    assert!(controller.get_executor(executor_id).is_err());
}

#[tokio::test]
async fn executor_retracts_attributes_and_keeps_the_rest_on_unbind() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    let ssn_id = "executor-attributes-retract".to_string();
    let executor_id = create_binding_executor(&controller, &ssn_id).await;
    let key = |value: &'static [u8]| bytes::Bytes::from_static(value);

    controller
        .bind_executor_completed(
            executor_id.clone(),
            Some(FlameResult {
                return_code: 0,
                message: None,
            }),
            Some(rpc::ExecutorAttributes {
                publish: vec![b"x".to_vec(), b"y".to_vec()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();

    // An invalid update leaves the previous state unchanged.
    controller
        .create_task(ssn_id.clone(), None, None)
        .await
        .unwrap();
    controller
        .launch_task(executor_id.clone())
        .await
        .unwrap()
        .unwrap();
    let succeed = || TaskResult {
        state: TaskState::Succeed,
        message: None,
        output: None,
    };
    assert!(controller
        .complete_task(
            executor_id.clone(),
            succeed(),
            Some(rpc::ExecutorAttributes {
                publish: vec![b"z".to_vec()],
                retract: vec![b"x".to_vec(), vec![]],
            }),
        )
        .await
        .is_err());
    assert_eq!(
        controller
            .get_executor(executor_id.clone())
            .unwrap()
            .attributes,
        HashSet::from([key(b"x"), key(b"y")])
    );

    // Retracting X (and an absent key) keeps Y; an empty update is a no-op.
    controller
        .complete_task(
            executor_id.clone(),
            succeed(),
            Some(rpc::ExecutorAttributes {
                retract: vec![b"x".to_vec(), b"absent".to_vec()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        controller
            .get_executor(executor_id.clone())
            .unwrap()
            .attributes,
        HashSet::from([key(b"y")])
    );
    controller
        .update_executor_attributes(&executor_id, ExecutorAttributeUpdate::default())
        .unwrap();

    // The retraction survives an ordinary unbind.
    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();
    assert_eq!(
        controller
            .get_executor(executor_id.clone())
            .unwrap()
            .attributes,
        HashSet::from([key(b"y")])
    );

    // A replacement executor starts without the retired executor's keys.
    let replacement_ssn = "executor-attributes-replacement";
    controller
        .create_session(create_test_session_attr(replacement_ssn))
        .await
        .unwrap();
    let replacement = controller
        .create_executor("bind-node".to_string(), replacement_ssn.to_string())
        .await
        .unwrap();
    controller.register_executor(&replacement).await.unwrap();
    assert_ne!(replacement.id, executor_id);
    assert!(controller
        .get_executor(replacement.id)
        .unwrap()
        .attributes
        .is_empty());
}

#[tokio::test]
async fn unregister_application_only_disables_application() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    let ssn_id = "unregister-releases-idle";
    let executor_id = create_binding_executor(&controller, ssn_id).await;

    controller
        .bind_executor_completed(
            executor_id.clone(),
            Some(FlameResult {
                return_code: 0,
                message: None,
            }),
            None,
        )
        .await
        .unwrap();
    controller.close_session(ssn_id.to_string()).await.unwrap();
    controller
        .unbind_executor(executor_id.clone())
        .await
        .unwrap();
    controller
        .unbind_executor_completed(executor_id.clone())
        .await
        .unwrap();

    assert_eq!(
        controller.get_executor(executor_id.clone()).unwrap().state,
        ExecutorState::Idle
    );

    controller
        .unregister_application("test-app".to_string())
        .await
        .unwrap();

    assert_eq!(
        controller.get_executor(executor_id).unwrap().state,
        ExecutorState::Idle
    );
    assert_eq!(
        controller
            .get_application("test-app".to_string())
            .await
            .unwrap()
            .state,
        ApplicationState::Disabled
    );
}

#[tokio::test]
async fn gang_min_and_max_instances_are_its_size_or_not_set() {
    let controller = new_ptr(create_test_storage().await);
    controller
        .register_application(
            "limited-app".to_string(),
            ApplicationAttributes {
                max_instances: 4,
                ..ApplicationAttributes::default()
            },
        )
        .await
        .unwrap();
    let session = |id: &str, batch_size, min_instances, max_instances| SessionAttributes {
        id: id.to_string(),
        application: "limited-app".to_string(),
        batch_size,
        min_instances,
        max_instances,
        ..SessionAttributes::default()
    };

    for attributes in [
        session("min-below-batch", 3, 2, None),
        session("min-above-batch", 3, 4, None),
        session("max-below-batch", 3, 0, Some(2)),
        session("max-above-batch", 3, 0, Some(4)),
        session("zero-max-gang", 3, 0, Some(0)),
        session("app-max-below-batch", 5, 0, None),
        session("max-below-min", 1, 3, Some(2)),
    ] {
        let id = attributes.id.clone();
        assert!(
            matches!(
                controller.create_session(attributes.clone()).await,
                Err(FlameError::InvalidConfig(_))
            ),
            "{id} must be rejected by create_session"
        );
        assert!(
            matches!(
                controller.open_session(id.clone(), Some(attributes)).await,
                Err(FlameError::InvalidConfig(_))
            ),
            "{id} must be rejected by open_session"
        );
    }

    // A gang is its size, or not kept or limited at all; a session without a gang is free.
    for attributes in [
        session("gang-only", 3, 0, None),
        session("gang-warm", 3, 3, None),
        session("gang-exact", 4, 4, Some(4)),
        session("gang-limited", 3, 0, Some(3)),
        session("default-batch", 0, 0, None),
        session("min-and-max", 1, 2, Some(5)),
        session("zero-max", 1, 0, Some(0)),
    ] {
        controller.create_session(attributes).await.unwrap();
    }
}

#[tokio::test]
async fn disabled_application_rejects_create_and_open() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage);
    controller
        .register_application("disabled-app".to_string(), ApplicationAttributes::default())
        .await
        .unwrap();

    let attributes = SessionAttributes {
        id: "disabled-session".to_string(),
        application: "disabled-app".to_string(),
        ..SessionAttributes::default()
    };
    controller.create_session(attributes.clone()).await.unwrap();
    controller
        .close_session(attributes.id.clone())
        .await
        .unwrap();
    controller
        .unregister_application("disabled-app".to_string())
        .await
        .unwrap();

    assert!(matches!(
        controller
            .create_session(SessionAttributes {
                id: "new-disabled-session".to_string(),
                ..attributes.clone()
            })
            .await,
        Err(FlameError::InvalidState(_))
    ));
    assert!(matches!(
        controller.open_session(attributes.id.clone(), None).await,
        Err(FlameError::InvalidState(_))
    ));
    assert!(matches!(
        controller
            .open_session(
                "missing-disabled-session".to_string(),
                Some(SessionAttributes {
                    id: "missing-disabled-session".to_string(),
                    ..attributes
                })
            )
            .await,
        Err(FlameError::InvalidState(_))
    ));
}

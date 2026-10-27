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

use std::collections::HashSet;
use std::sync::Arc;

use stdng::{logs::TraceFn, trace_fn};

use chrono::{DateTime, Duration, Utc};

use crate::model::{
    ExecutorInfo, SessionFilter, SnapShot, ALL_EXECUTOR, BOUND_EXECUTOR, IDLE_EXECUTOR,
};
use crate::scheduler::actions::{Action, ActionPtr};
use crate::scheduler::ctx::Context;

use common::apis::SessionState;
use common::FlameError;

pub struct ShuffleAction {}

fn idle_executor_expired(snapshot: &SnapShot, executor: &ExecutorInfo) -> Result<bool, FlameError> {
    let Some(application) = snapshot.get_application(&executor.application)? else {
        return Ok(true);
    };
    let delay_release = application.delay_release;
    let idle_delay = delay_release.max(Duration::zero()) * 2;
    let idle_age = Utc::now().signed_duration_since(executor.latest_updated_timestamp);
    Ok(idle_age < Duration::zero() || idle_age >= idle_delay)
}

impl ShuffleAction {
    pub fn new_ptr() -> ActionPtr {
        Arc::new(ShuffleAction {})
    }
}

#[async_trait::async_trait]
impl Action for ShuffleAction {
    async fn execute(&self, ctx: &mut Context) -> Result<(), FlameError> {
        trace_fn!("ShuffleAction::execute");
        let ss = ctx.snapshot.clone();

        // Complete the suspends: a Suspending session no executor serves becomes Suspended. Binds run
        // in this same loop, so the snapshot cannot miss one; a stale snapshot only over-counts executors.
        let suspending =
            ss.find_sessions(Some(SessionFilter::by_state(SessionState::Suspending)))?;
        if !suspending.is_empty() {
            let serving: HashSet<_> = ss
                .find_executors(ALL_EXECUTOR)?
                .values()
                .filter_map(|executor| executor.ssn_id.clone())
                .collect();
            for session in suspending.values() {
                if serving.contains(&session.id) {
                    continue;
                }
                if let Err(e) = ctx.suspend_session(session).await {
                    tracing::warn!(
                        "Failed to complete suspend of session <{}>: {e}",
                        session.id
                    );
                }
            }
        }

        // Reclaim surplus bindings even when no recipient currently needs them.
        // Interrupt requests take effect after any in-flight task completes.
        for executor in ss.find_executors(BOUND_EXECUTOR)?.values() {
            if !ctx.is_reclaimable(executor)? {
                continue;
            }
            let Some(session_id) = &executor.ssn_id else {
                continue;
            };
            let session = ss.get_session(session_id)?;
            if ctx.is_overused(&session)? {
                ctx.interrupt_executor(executor, &session).await?;
            }
        }

        // Release retained Idle executors after the application's grace period,
        // so DAS can reuse them without allowing indefinite resource starvation.
        let idle_execs = ss.find_executors(IDLE_EXECUTOR)?;
        for exec in idle_execs.values() {
            if idle_executor_expired(&ss, exec)? {
                ctx.release_executor(exec).await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AppInfo;
    use common::apis::{
        ApplicationAttributes, ExecutorState, FlameResult, Node, NodeState, ResourceRequirement,
        SessionAttributes, Signal, BIND_RESULT_OK,
    };
    use common::ctx::{FlameCluster, FlameClusterContext};

    fn idle_executor(updated_at: DateTime<Utc>) -> ExecutorInfo {
        ExecutorInfo {
            application: "app".to_string(),
            latest_updated_timestamp: updated_at,
            ..Default::default()
        }
    }

    fn snapshot(delay_release: Option<Duration>) -> SnapShot {
        let snapshot = SnapShot::new();
        if let Some(delay_release) = delay_release {
            snapshot
                .add_application(Arc::new(AppInfo {
                    name: "app".to_string(),
                    delay_release,
                    ..Default::default()
                }))
                .unwrap();
        }
        snapshot
    }

    #[test]
    fn idle_retention_honors_delay() {
        let now = Utc::now();
        let configured_delay = Duration::minutes(1);
        let snapshot = snapshot(Some(configured_delay));

        assert!(
            !idle_executor_expired(&snapshot, &idle_executor(now - Duration::seconds(90)),)
                .unwrap()
        );
        assert!(
            idle_executor_expired(&snapshot, &idle_executor(now - Duration::seconds(121)),)
                .unwrap()
        );
    }

    #[test]
    fn idle_retention_releases_immediately_without_positive_delay() {
        let now = Utc::now();
        let executor = idle_executor(now);

        assert!(idle_executor_expired(&snapshot(Some(Duration::zero())), &executor).unwrap());
        assert!(idle_executor_expired(&snapshot(Some(Duration::seconds(-1))), &executor).unwrap());
    }

    #[test]
    fn idle_retention_releases_executor_for_unregistered_application() {
        let executor = idle_executor(Utc::now());

        assert!(idle_executor_expired(&snapshot(None), &executor).unwrap());
    }

    #[test]
    fn idle_retention_expires_after_clock_rollback() {
        let now = Utc::now();
        let executor = idle_executor(now + Duration::seconds(1));

        assert!(idle_executor_expired(&snapshot(Some(Duration::minutes(1))), &executor,).unwrap());
    }

    #[tokio::test]
    async fn shuffle_trims_surplus_without_recipient_and_preserves_minimum() {
        let (controller, ids) = bound_executors(3, 1).await;
        for _ in 0..3 {
            let mut context = shuffle_context(controller.clone());
            ShuffleAction {}.execute(&mut context).await.unwrap();
            let executors = controller.list_executors().unwrap();
            assert_eq!(executors.len(), ids.len());
            assert_eq!(
                executors
                    .iter()
                    .filter(|e| e.state == ExecutorState::Bound && !e.is_interrupted())
                    .count(),
                1
            );
            assert_eq!(
                executors
                    .iter()
                    .filter(|e| e.state == ExecutorState::Bound && e.is_interrupted())
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn shuffle_signals_inflight_tasks_without_blocking_completion() {
        let (controller, ids) = bound_executors(3, 0).await;
        // Snapshot the idle bindings before the worker receives a new task.
        let mut context = shuffle_context(controller.clone());
        controller
            .create_task("session".into(), None, None)
            .await
            .unwrap();
        let task = controller
            .launch_task(ids[0].clone())
            .await
            .unwrap()
            .unwrap();
        ShuffleAction {}.execute(&mut context).await.unwrap();
        let busy = controller.get_executor(ids[0].clone()).unwrap();
        assert_eq!(busy.state, ExecutorState::Bound);
        assert_eq!(busy.task_id, Some(task.id));
        assert_eq!(busy.signal, Some(Signal::Interrupt));
        for id in &ids[1..] {
            let interrupted = controller.get_executor(id.clone()).unwrap();
            assert_eq!(interrupted.state, ExecutorState::Bound);
            assert_eq!(interrupted.signal, Some(Signal::Interrupt));
        }
        controller
            .complete_task(
                ids[0].clone(),
                common::apis::TaskResult {
                    state: common::apis::TaskState::Succeed,
                    output: None,
                    message: None,
                },
                Some(rpc::flame::v1::ExecutorAttributes {
                    publish: vec![b"task-completed".to_vec()],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(controller
            .launch_task(ids[0].clone())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn shuffle_completes_a_suspend_no_executor_serves() {
        let (controller, ids) = bound_executors(0, 0).await;
        assert!(ids.is_empty());
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();

        let mut context = shuffle_context(controller.clone());
        ShuffleAction {}.execute(&mut context).await.unwrap();

        assert!(controller
            .get_session("session".into())
            .unwrap()
            .is_suspended());
    }

    /// One scheduler cycle: a fresh snapshot, then the shuffle action.
    async fn cycle(controller: &crate::controller::ControllerPtr) {
        let mut context = shuffle_context(controller.clone());
        ShuffleAction {}.execute(&mut context).await.unwrap();
    }

    fn state_of(controller: &crate::controller::ControllerPtr) -> SessionState {
        controller
            .get_session("session".into())
            .unwrap()
            .status
            .state
    }

    fn succeed() -> common::apis::TaskResult {
        common::apis::TaskResult {
            state: common::apis::TaskState::Succeed,
            output: None,
            message: None,
        }
    }

    async fn unbind(controller: &crate::controller::ControllerPtr, id: &str) {
        controller.unbind_executor(id.to_string()).await.unwrap();
        controller
            .unbind_executor_completed(id.to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shuffle_completes_a_suspend_once_the_executor_unbinds() {
        let (controller, ids) = bound_executors(1, 0).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);

        // Unbinding still references the session.
        controller.unbind_executor(ids[0].clone()).await.unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);

        controller
            .unbind_executor_completed(ids[0].clone())
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspended);
    }

    #[tokio::test]
    async fn shuffle_completes_a_suspend_after_a_failed_bind() {
        let (controller, _) = bound_executors(0, 0).await;
        let id = new_executor(&controller, false).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);

        controller
            .bind_session_completed(
                id.clone(),
                Some(FlameResult {
                    return_code: common::apis::BIND_RESULT_UNKNOWN_FAILED,
                    message: Some("boom".to_string()),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            controller.get_executor(id).unwrap().state,
            ExecutorState::Unbinding
        );
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspended);
    }

    #[tokio::test]
    async fn shuffle_completes_a_suspend_after_the_executor_unregisters() {
        let (controller, ids) = bound_executors(1, 0).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();

        // Orphan cleanup moves the executor to Releasing, then the executor manager unregisters it.
        let mut executor = controller.get_executor(ids[0].clone()).unwrap();
        executor.state = ExecutorState::Releasing;
        controller
            .storage()
            .update_executor(&executor)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);

        controller
            .unregister_executor(ids[0].clone())
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspended);
    }

    #[tokio::test]
    async fn shuffle_completes_a_suspend_after_node_loss() {
        let (controller, _) = bound_executors(1, 0).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);

        // What the node shutdown does: delete the node's executors.
        let executors = controller.list_executors().unwrap();
        controller
            .storage()
            .delete_executors(&executors)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspended);
    }

    #[tokio::test(start_paused = true)]
    async fn shuffle_keeps_suspending_behind_a_long_task_until_it_unbinds() {
        let (controller, ids) = bound_executors(1, 0).await;
        let task = controller
            .create_task("session".into(), None, None)
            .await
            .unwrap();
        controller.launch_task(ids[0].clone()).await.unwrap();
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();

        // Far beyond the former 10 minute deadline: nothing aborts the suspend.
        tokio::time::advance(std::time::Duration::from_secs(24 * 60 * 60)).await;
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);
        assert_eq!(
            controller
                .get_task("session".into(), task.id)
                .unwrap()
                .state,
            common::apis::TaskState::Running
        );

        controller
            .complete_task(ids[0].clone(), succeed(), None)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspending);

        unbind(&controller, &ids[0]).await;
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspended);
    }

    #[tokio::test]
    async fn shuffle_leaves_a_session_closed_meanwhile_closed() {
        let (controller, _) = bound_executors(1, 0).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();
        controller.close_session("session".into()).await.unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Closed);

        // Closed between the snapshot and the completion.
        let (controller, _) = bound_executors(0, 0).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();
        let mut context = shuffle_context(controller.clone());
        controller.close_session("session".into()).await.unwrap();
        ShuffleAction {}.execute(&mut context).await.unwrap();
        assert_eq!(state_of(&controller), SessionState::Closed);
    }

    #[tokio::test]
    async fn shuffle_leaves_a_resumed_session_open() {
        let (controller, _) = bound_executors(0, 0).await;
        controller
            .signal_session("session".into(), Signal::Suspend)
            .await
            .unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Suspended);

        controller.resume_session("session".into()).await.unwrap();
        cycle(&controller).await;
        assert_eq!(state_of(&controller), SessionState::Open);
    }

    fn shuffle_context(controller: crate::controller::ControllerPtr) -> Context {
        Context::new(
            controller,
            &crate::scheduler::plugins::PluginsOptions {
                policies: vec!["priority".into(), "das".into()],
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// Assigns a new executor to the session; `bound` also completes the bind.
    async fn new_executor(controller: &crate::controller::ControllerPtr, bound: bool) -> String {
        let executor = controller
            .create_executor("node".into(), "session".into())
            .await
            .unwrap();
        controller.register_executor(&executor).await.unwrap();
        controller
            .bind_session(executor.id.clone(), "session".into())
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

    async fn bound_executors(
        count: usize,
        minimum: u32,
    ) -> (crate::controller::ControllerPtr, Vec<String>) {
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        let controller = crate::controller::new_ptr(storage.clone());
        controller
            .register_application("app".into(), ApplicationAttributes::default())
            .await
            .unwrap();
        storage
            .register_node(&Node {
                name: "node".into(),
                state: NodeState::Ready,
                ..Default::default()
            })
            .await
            .unwrap();
        controller
            .create_session(SessionAttributes {
                id: "session".into(),
                application: "app".into(),
                min_instances: minimum,
                resreq: Some(ResourceRequirement::default()),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..count {
            ids.push(new_executor(&controller, true).await);
        }
        (controller, ids)
    }

    #[tokio::test]
    async fn shuffle_does_not_treat_disabled_application_as_absent() {
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = crate::storage::new_ptr(&config).await.unwrap();
        let controller = crate::controller::new_ptr(storage.clone());
        controller
            .register_application("app".to_string(), ApplicationAttributes::default())
            .await
            .unwrap();
        storage
            .register_node(&Node {
                name: "node".to_string(),
                state: NodeState::Ready,
                ..Default::default()
            })
            .await
            .unwrap();
        controller
            .create_session(SessionAttributes {
                id: "session".to_string(),
                application: "app".to_string(),
                resreq: Some(ResourceRequirement::default()),
                ..Default::default()
            })
            .await
            .unwrap();
        let executor = controller
            .create_executor("node".to_string(), "session".to_string())
            .await
            .unwrap();
        controller.register_executor(&executor).await.unwrap();
        controller
            .bind_session(executor.id.clone(), "session".to_string())
            .await
            .unwrap();
        controller
            .bind_executor_completed(
                executor.id.clone(),
                Some(FlameResult {
                    return_code: BIND_RESULT_OK,
                    message: None,
                }),
                None,
            )
            .await
            .unwrap();
        controller
            .close_session("session".to_string())
            .await
            .unwrap();
        controller
            .unbind_executor(executor.id.clone())
            .await
            .unwrap();

        controller
            .unregister_application("app".to_string())
            .await
            .unwrap();
        controller
            .unbind_executor_completed(executor.id.clone())
            .await
            .unwrap();
        assert_eq!(
            controller.get_executor(executor.id.clone()).unwrap().state,
            ExecutorState::Idle
        );

        let options = crate::scheduler::plugins::PluginsOptions {
            policies: vec![],
            ..Default::default()
        };
        let mut context = Context::new(controller.clone(), &options).unwrap();
        ShuffleAction {}.execute(&mut context).await.unwrap();

        assert_eq!(
            controller.get_executor(executor.id).unwrap().state,
            ExecutorState::Idle
        );
    }
}

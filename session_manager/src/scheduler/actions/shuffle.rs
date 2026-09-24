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

use std::sync::Arc;

use stdng::collections::{BinaryHeap, Cmp};
use stdng::{logs::TraceFn, trace_fn};

use chrono::{DateTime, Duration, Utc};

use crate::model::{ExecutorInfo, SnapShot, BOUND_EXECUTOR, IDLE_EXECUTOR, READY_SESSION};
use crate::scheduler::actions::{Action, ActionPtr};
use crate::scheduler::ctx::Context;
use crate::scheduler::plugins::ssn_order_fn;

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

        let mut underused = BinaryHeap::new(ssn_order_fn(ctx));
        let open_ssns = ss.find_sessions(READY_SESSION)?;
        for ssn in open_ssns.values() {
            if ctx.is_underused(ssn)? {
                underused.push(ssn.clone());
            }
        }

        let mut bound_execs = ss.find_executors(BOUND_EXECUTOR)?;

        // Unbind overused sessions for underused sessions.
        loop {
            if underused.is_empty() {
                break;
            }

            let ssn = underused
                .pop()
                .expect("failed to pop underused session: loop guard ensures non-empty");
            if !ctx.is_underused(&ssn)? {
                continue;
            }

            let mut exec = None;
            for e in bound_execs.values_mut() {
                tracing::debug!(
                    "Try to unbound Executor <{}> for session <{}>",
                    e.id,
                    ssn.id.clone()
                );

                let target_ssn = match e.ssn_id.clone() {
                    Some(ssn_id) => Some(ss.get_session(&ssn_id)?),
                    None => None,
                };

                if let Some(target_ssn) = target_ssn {
                    if !ctx.is_preemptible(&target_ssn)? {
                        continue;
                    }

                    // Unbind the overused session, so the executor will
                    // become idle and be allocated to the underused session.
                    ctx.unbind_session(e, &target_ssn).await?;
                    exec = Some(e.clone());

                    break;
                }
            }

            if let Some(exec) = exec {
                tracing::debug!(
                    "Executor <{}> was pipelined to session <{}>, remove it from bound list.",
                    exec.id,
                    ssn.id.clone()
                );

                bound_execs.remove(&exec.id);

                // Pipeline the executor to the underused session to avoid over allocation.
                ctx.pipeline_executor(&exec, &ssn)?;
                underused.push(ssn.clone());
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
        SessionAttributes, BIND_RESULT_OK,
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

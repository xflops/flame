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

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::{thread, time};
use tokio::time::Duration;

use crate::controller::ControllerPtr;
use crate::model::{ScopedName, SessionInfoPtr, SnapShot};
use crate::scheduler::ctx::Context;
use crate::scheduler::plugins::PluginsOptions;

use crate::FlameThread;
use common::ctx::FlameClusterContext;
use common::FlameError;

mod actions;
mod ctx;
mod plugins;

/// Aggregate workspace-scoped queries for the cluster-wide scheduler.
fn scheduling_sessions(
    snapshot: &SnapShot,
    ready_only: bool,
) -> Result<HashMap<ScopedName, SessionInfoPtr>, FlameError> {
    use common::apis::{SessionFilter, SessionPredicate, SessionState};
    let workspaces: HashSet<String> = snapshot
        .all_sessions()?
        .values()
        .map(|session| session.workspace.clone())
        .collect();
    let mut sessions = HashMap::new();
    for workspace in workspaces {
        let mut filter = SessionFilter::new(workspace).by_state(SessionState::Open);
        if ready_only {
            filter = filter.with_predicate(SessionPredicate::Ready);
        }
        sessions.extend(snapshot.find_sessions(&filter)?);
    }
    Ok(sessions)
}

fn open_sessions(snapshot: &SnapShot) -> Result<HashMap<ScopedName, SessionInfoPtr>, FlameError> {
    scheduling_sessions(snapshot, false)
}

fn ready_sessions(snapshot: &SnapShot) -> Result<HashMap<ScopedName, SessionInfoPtr>, FlameError> {
    scheduling_sessions(snapshot, true)
}

pub fn new(controller: ControllerPtr) -> Arc<dyn FlameThread> {
    Arc::new(ScheduleRunner { controller })
}

struct ScheduleRunner {
    controller: ControllerPtr,
}

#[async_trait]
impl FlameThread for ScheduleRunner {
    async fn run(&self, flame_ctx: FlameClusterContext) -> Result<(), FlameError> {
        let schedule_interval = flame_ctx.cluster.schedule_interval;
        let options = PluginsOptions::from(&flame_ctx);
        tracing::info!(
            "Scheduler started with interval: {}ms, enabled policies: {:?}, max executors per node: {}",
            schedule_interval,
            options.policies,
            options.max_executors,
        );
        let schedule_interval = Duration::from_millis(schedule_interval);

        loop {
            let mut ctx = Context::new(self.controller.clone(), &options)?;

            // Same `ctx` (and thus same in-memory `plugins`) is used for every action.
            for action in ctx.actions.clone() {
                if let Err(e) = action.execute(&mut ctx).await {
                    tracing::error!("Failed to run scheduling: {e}");
                    break;
                };
            }

            self.controller
                .wait_for_scheduler_event(schedule_interval)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scheduler_aggregates_scoped_sessions_across_workspaces() {
        use crate::model::{SessionInfo, SnapShot};
        use common::apis::SessionState;
        use std::sync::Arc;
        let snapshot = SnapShot::new_with_session_retry_limits(2);
        for workspace in ["default", "research"] {
            snapshot
                .add_session(Arc::new(SessionInfo {
                    name: "shared".to_string(),
                    workspace: workspace.to_string(),
                    state: SessionState::Open,
                    ..Default::default()
                }))
                .unwrap();
        }
        snapshot
            .add_session(Arc::new(SessionInfo {
                name: "exhausted".to_string(),
                workspace: "research".to_string(),
                state: SessionState::Open,
                retry_count: 2,
                ..Default::default()
            }))
            .unwrap();
        snapshot
            .add_session(Arc::new(SessionInfo {
                name: "closed".to_string(),
                workspace: "research".to_string(),
                state: SessionState::Closed,
                ..Default::default()
            }))
            .unwrap();
        let open = super::open_sessions(&snapshot).unwrap();
        assert_eq!(open.len(), 3);
        let ready = super::ready_sessions(&snapshot).unwrap();
        assert_eq!(ready.len(), 2);
        for workspace in ["default", "research"] {
            assert!(ready.contains_key(&(workspace.to_string(), "shared".to_string())));
        }
    }

    use rand::Rng;

    use crate::controller;
    use crate::model::ALL_NODE;
    use crate::scheduler::actions::{AllocateAction, DispatchAction};
    use crate::scheduler::ctx::Context;
    use crate::scheduler::plugins::{PluginManager, PluginsOptions};
    use crate::scheduler::ControllerPtr;
    use crate::storage;
    use bytes::Bytes;
    use chrono::Duration;
    use chrono::Utc;
    use common::apis::{
        Application, ApplicationAttributes, Node, NodeInfo, NodeState, ResourceRequirement,
        SessionGID, Shim, TaskOptions,
    };
    use common::ctx::{FlameCluster, FlameClusterContext, FlameRecovery, FlameSessionRecovery};
    use common::FlameError;
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;
    // use tracing_test::traced_test;

    fn new_test_application() -> ApplicationAttributes {
        ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            command: None,
            description: None,
            labels: Vec::new(),
            arguments: Vec::new(),
            working_directory: Some("/tmp".to_string()),
            environments: HashMap::new(),
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        }
    }

    fn new_test_node(name: String) -> Node {
        Node {
            id: Uuid::new_v4().to_string(),
            name,
            allocatable: ResourceRequirement {
                cpu: 64,
                memory: 100 * 1024 * 1024 * 1024,
                gpu: 0,
            },
            capacity: ResourceRequirement {
                cpu: 64,
                memory: 100 * 1024 * 1024 * 1024,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
            state: NodeState::Ready,
        }
    }

    struct TestEnv {
        url: String,
        pub controller: ControllerPtr,
    }

    impl TestEnv {
        pub fn new() -> Result<Self, FlameError> {
            Self::new_with_retry_limit(common::ctx::DEFAULT_SESSION_RETRY_LIMITS)
        }

        pub fn new_with_retry_limit(retry_limits: u32) -> Result<Self, FlameError> {
            let filter = tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("h2=error".parse()?)
                .add_directive("hyper_util=error".parse()?)
                .add_directive("sqlx=error".parse()?)
                .add_directive("tower=error".parse()?);

            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_test_writer()
                .with_target(true)
                .with_ansi(false)
                .try_init();

            let url = common::temp_db_path("flame_test_env");
            let config = FlameClusterContext {
                cluster: FlameCluster {
                    storage: format!("sqlite:///{url}"),
                    recovery: FlameRecovery {
                        session: FlameSessionRecovery { retry_limits },
                    },
                    ..Default::default()
                },
                ..Default::default()
            };

            let storage = tokio_test::block_on(storage::new_ptr(&config))?;
            let controller = controller::new_ptr(storage.clone());

            Ok(Self { url, controller })
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            // Best-effort cleanup - ignore errors (e.g., file in use on Windows)
            let _ = std::fs::remove_file(&self.url);
        }
    }

    /// Test the allocation of void executors to underused sessions.
    #[test]
    fn test_allocate_executors() -> Result<(), FlameError> {
        let env = TestEnv::new()?;
        let controller = env.controller.clone();

        let mut rng = rand::rng();
        let task_num = rng.random_range(1..10);

        tokio_test::block_on(controller.register_application(
            "default".to_string(),
            "flmtest".to_string(),
            new_test_application(),
        ))?;
        // Just register node in storage (no stream connection needed for scheduler test)
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;
        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 =
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                workspace: "default".to_string(),
                name: ssn_1_id.clone(),
                application: "flmtest".to_string(),
                common_data: None,
                tokens: Default::default(),
                min_instances: 0,
                max_instances: None,
                batch_size: 1,
                priority: 0,
                resreq: Some(common::apis::ResourceRequirement {
                    cpu: 1,
                    memory: 1024 * 1024 * 1024,
                    gpu: 0,
                }),
            }))?;

        for _ in 0..task_num {
            tokio_test::block_on(controller.create_task(
                &SessionGID::new("default", &ssn_1.name),
                None,
                None,
            ))?;
        }

        for i in 0..10 {
            let snapshot = controller.snapshot()?;
            let options = PluginsOptions::default();
            let plugins = PluginManager::setup(&snapshot.clone(), &options)?;

            let mut ctx = Context {
                snapshot: snapshot.clone(),
                controller: controller.clone(),
                plugins,
                actions: vec![],
            };

            let dispatch = DispatchAction::new_ptr();
            tokio_test::block_on(dispatch.execute(&mut ctx))?;

            let alloc = AllocateAction::new_ptr();
            tokio_test::block_on(alloc.execute(&mut ctx))?;

            let ssn_list = crate::scheduler::open_sessions(&snapshot)?;
            assert_eq!(ssn_list.len(), 1);
            assert_eq!(ssn_list.values().next().unwrap().id, ssn_1.id.clone());

            let node_list = snapshot.find_nodes(ALL_NODE)?;
            assert_eq!(node_list.len(), 1);
            assert_eq!(node_list.values().next().unwrap().name, "node_1");

            let exec_list = controller.list_executors()?;
            // The test does not run an executor manager, so a created executor
            // remains Void. Allocate pipelines that existing Void executor on
            // later cycles instead of creating duplicates before it becomes
            // Idle and Dispatch can bind it.
            assert_eq!(exec_list.len(), 1, "cycle {i}");
            assert_eq!(exec_list[0].session, None);
        }

        Ok(())
    }

    #[test]
    fn test_allocate_respects_max_executors() -> Result<(), FlameError> {
        let env = TestEnv::new()?;
        let controller = env.controller.clone();

        tokio_test::block_on(controller.register_application(
            "default".to_string(),
            "flmtest".to_string(),
            new_test_application(),
        ))?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        for index in 0..2 {
            let session =
                tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                    workspace: "default".to_string(),
                    name: format!("limited-session-{index}"),
                    application: "flmtest".to_string(),
                    resreq: Some(common::apis::ResourceRequirement {
                        cpu: 1,
                        memory: 1024,
                        gpu: 0,
                    }),
                    ..Default::default()
                }))?;
            tokio_test::block_on(controller.create_task(
                &SessionGID::new("default", &session.name),
                None,
                None,
            ))?;
        }

        let options = PluginsOptions {
            max_executors: 1,
            ..Default::default()
        };
        let mut ctx = Context::new(controller.clone(), &options)?;
        tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;

        let executors = controller.list_executors()?;
        assert_eq!(executors.len(), 1);
        assert_eq!(executors[0].node, "node_1");
        Ok(())
    }

    /// One scheduling cycle keeps the same in-memory [`crate::scheduler::plugins::PluginManager`]
    /// for every action.
    #[test]
    fn test_scheduler_cycle_reuses_plugin_manager_across_actions() -> Result<(), FlameError> {
        let env = TestEnv::new()?;
        let controller = env.controller.clone();

        tokio_test::block_on(controller.register_application(
            "default".to_string(),
            "flmtest".to_string(),
            new_test_application(),
        ))?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let options = PluginsOptions::default();
        let mut ctx = Context::new(controller.clone(), &options)?;
        let plugins_ptr = Arc::as_ptr(&ctx.plugins);
        for action in ctx.actions.clone() {
            tokio_test::block_on(action.execute(&mut ctx))?;
        }
        assert_eq!(plugins_ptr, Arc::as_ptr(&ctx.plugins));
        Ok(())
    }

    fn assert_dispatch_reuses_idle_executors(task_counts: &[usize]) -> Result<(), FlameError> {
        let env = TestEnv::new()?;
        let controller = env.controller.clone();

        tokio_test::block_on(controller.register_application(
            "default".to_string(),
            "flmtest".to_string(),
            new_test_application(),
        ))?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let mut session_ids = Vec::new();
        for task_count in task_counts {
            let ssn_id = format!("reuse-idle-{}", Uuid::new_v4());
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                workspace: "default".to_string(),
                name: ssn_id.clone(),
                application: "flmtest".to_string(),
                common_data: None,
                tokens: Default::default(),
                min_instances: 0,
                max_instances: None,
                batch_size: 1,
                priority: 0,
                resreq: Some(common::apis::ResourceRequirement {
                    cpu: 1,
                    memory: 1024 * 1024 * 1024,
                    gpu: 0,
                }),
            }))?;
            for _ in 0..*task_count {
                tokio_test::block_on(controller.create_task(
                    &SessionGID::new("default", &ssn_id),
                    None,
                    None,
                ))?;
            }
            session_ids.push(ssn_id);
        }

        let executor_count = task_counts.iter().sum::<usize>();
        for _ in 0..executor_count {
            let executor = tokio_test::block_on(controller.create_executor(
                "node_1".to_string(),
                &SessionGID::new("default", &session_ids[0]),
            ))?;
            tokio_test::block_on(controller.register_executor(&executor))?;
        }

        let options = PluginsOptions::default();
        let mut ctx = Context::new(controller.clone(), &options)?;

        let dispatch = DispatchAction::new_ptr();
        tokio_test::block_on(dispatch.execute(&mut ctx))?;

        let allocate = AllocateAction::new_ptr();
        tokio_test::block_on(allocate.execute(&mut ctx))?;

        let executors = controller.list_executors()?;
        assert_eq!(executors.len(), executor_count);
        assert!(executors.iter().all(|executor| {
            executor.state == common::apis::ExecutorState::Binding
                && executor
                    .session
                    .as_ref()
                    .is_some_and(|session_id| session_ids.contains(session_id))
        }));
        for (session_id, task_count) in session_ids.iter().zip(task_counts) {
            assert_eq!(
                executors
                    .iter()
                    .filter(|executor| executor.session.as_ref() == Some(session_id))
                    .count(),
                *task_count
            );
        }

        Ok(())
    }

    #[test]
    fn test_dispatch_reuses_all_idle_executors_for_one_session() -> Result<(), FlameError> {
        assert_dispatch_reuses_idle_executors(&[2])
    }

    #[test]
    fn test_dispatch_reuses_idle_executors_for_separate_sessions() -> Result<(), FlameError> {
        assert_dispatch_reuses_idle_executors(&[1, 1])
    }

    #[test]
    fn test_dispatch_preserves_das_affinity_across_scheduler_cycles() -> Result<(), FlameError> {
        let env = TestEnv::new()?;
        let controller = env.controller.clone();

        tokio_test::block_on(controller.register_application(
            "default".to_string(),
            "flmtest".to_string(),
            new_test_application(),
        ))?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let mut session_ids = Vec::new();
        for index in 0..2 {
            let session =
                tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                    workspace: "default".to_string(),
                    name: format!("das-session-{index}"),
                    application: "flmtest".to_string(),
                    resreq: Some(ResourceRequirement {
                        cpu: 1,
                        memory: 1024,
                        gpu: 0,
                    }),
                    ..Default::default()
                }))?;
            session_ids.push(session.name);
        }

        let mut executor_ids = Vec::new();
        for index in 0..2 {
            let executor = tokio_test::block_on(controller.create_executor(
                "node_1".to_string(),
                &SessionGID::new("default", &session_ids[0]),
            ))?;
            tokio_test::block_on(controller.register_executor(&executor))?;
            {
                let executor = controller.storage().get_executor_ptr(&executor.name)?;
                let mut executor = stdng::lock_ptr!(executor)?;
                executor
                    .attributes
                    .insert(Bytes::from(format!("key-{index}")));
            }
            executor_ids.push(executor.name);
        }

        let options = PluginsOptions {
            policies: vec!["priority".to_string(), "drf".to_string(), "das".to_string()],
            ..Default::default()
        };
        for index in 0..2 {
            tokio_test::block_on(controller.create_task(
                &SessionGID::new("default", &session_ids[index]),
                None,
                Some(TaskOptions {
                    affinity: [Bytes::from(format!("key-{index}"))].into_iter().collect(),
                }),
            ))?;
            let mut ctx = Context::new(controller.clone(), &options)?;
            tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;

            let executor = controller.get_executor(&executor_ids[index])?;
            assert_eq!(executor.session.as_ref(), Some(&session_ids[index]));
        }

        Ok(())
    }

    #[test]
    fn test_scheduler_skips_not_ready_session() -> Result<(), FlameError> {
        let env = TestEnv::new_with_retry_limit(1)?;
        let controller = env.controller.clone();

        tokio_test::block_on(controller.register_application(
            "default".to_string(),
            "flmtest".to_string(),
            new_test_application(),
        ))?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let ssn_id = format!("not-ready-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            workspace: "default".to_string(),
            name: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
            tokens: Default::default(),
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: Some(common::apis::ResourceRequirement {
                cpu: 1,
                memory: 1024 * 1024 * 1024,
                gpu: 0,
            }),
        }))?;
        tokio_test::block_on(controller.create_task(
            &SessionGID::new("default", &ssn_id),
            None,
            None,
        ))?;

        {
            let ssn_ptr = controller.storage().get_session_ptr("default", &ssn_id)?;
            let mut ssn = stdng::lock_ptr!(ssn_ptr)?;
            ssn.retry_count = 1;
        }

        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), &SessionGID::new("default", &ssn_id)),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;

        let options = PluginsOptions::default();
        let mut ctx = Context::new(controller.clone(), &options)?;

        let dispatch = DispatchAction::new_ptr();
        tokio_test::block_on(dispatch.execute(&mut ctx))?;

        let alloc = AllocateAction::new_ptr();
        tokio_test::block_on(alloc.execute(&mut ctx))?;

        let executors = controller.list_executors()?;
        assert_eq!(executors.len(), 1);
        assert_eq!(executors[0].state, common::apis::ExecutorState::Idle);
        assert_eq!(executors[0].session, None);

        Ok(())
    }
}

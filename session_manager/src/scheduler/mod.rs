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
use std::sync::Arc;
use std::{thread, time};
use tokio::time::Duration;

use crate::controller::ControllerPtr;
use crate::scheduler::ctx::Context;
use crate::scheduler::plugins::PluginsOptions;

use crate::FlameThread;
use common::ctx::FlameClusterContext;
use common::FlameError;

mod actions;
mod ctx;
mod plugins;

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
            options.limits.node.max_executors,
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
    use rand::Rng;

    use crate::controller;
    use crate::model::{ALL_NODE, OPEN_SESSION};
    use crate::scheduler::actions::{AllocateAction, DispatchAction};
    use crate::scheduler::ctx::Context;
    use crate::scheduler::plugins::{PluginManager, PluginsOptions};
    use crate::scheduler::ControllerPtr;
    use crate::storage;
    use bytes::Bytes;
    use chrono::Duration;
    use chrono::Utc;
    use common::apis::{
        Application, ApplicationAttributes, Node, NodeInfo, NodeState, ResourceRequirement, Shim,
        TaskOptions,
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

        tokio_test::block_on(
            controller.register_application("flmtest".to_string(), new_test_application()),
        )?;
        // Just register node in storage (no stream connection needed for scheduler test)
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;
        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 =
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                id: ssn_1_id.clone(),
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
            tokio_test::block_on(controller.create_task(ssn_1.id.clone(), None, None))?;
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

            let ssn_list = snapshot.find_sessions(OPEN_SESSION)?;
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
            assert_eq!(exec_list[0].ssn_id, None);
        }

        Ok(())
    }

    #[test]
    fn test_allocate_respects_max_executors() -> Result<(), FlameError> {
        let env = TestEnv::new()?;
        let controller = env.controller.clone();

        tokio_test::block_on(
            controller.register_application("flmtest".to_string(), new_test_application()),
        )?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        for index in 0..2 {
            let session =
                tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                    id: format!("limited-session-{index}"),
                    application: "flmtest".to_string(),
                    resreq: Some(common::apis::ResourceRequirement {
                        cpu: 1,
                        memory: 1024,
                        gpu: 0,
                    }),
                    ..Default::default()
                }))?;
            tokio_test::block_on(controller.create_task(session.id, None, None))?;
        }

        let options = PluginsOptions {
            limits: common::ctx::FlameLimits {
                node: common::ctx::FlameNodeLimits { max_executors: 1 },
                ..Default::default()
            },
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

        tokio_test::block_on(
            controller.register_application("flmtest".to_string(), new_test_application()),
        )?;
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

        tokio_test::block_on(
            controller.register_application("flmtest".to_string(), new_test_application()),
        )?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let mut session_ids = Vec::new();
        for task_count in task_counts {
            let ssn_id = format!("reuse-idle-{}", Uuid::new_v4());
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                id: ssn_id.clone(),
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
                tokio_test::block_on(controller.create_task(ssn_id.clone(), None, None))?;
            }
            session_ids.push(ssn_id);
        }

        let executor_count = task_counts.iter().sum::<usize>();
        for _ in 0..executor_count {
            let executor = tokio_test::block_on(
                controller.create_executor("node_1".to_string(), session_ids[0].clone()),
            )?;
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
                    .ssn_id
                    .as_ref()
                    .is_some_and(|session_id| session_ids.contains(session_id))
        }));
        for (session_id, task_count) in session_ids.iter().zip(task_counts) {
            assert_eq!(
                executors
                    .iter()
                    .filter(|executor| executor.ssn_id.as_ref() == Some(session_id))
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

        tokio_test::block_on(
            controller.register_application("flmtest".to_string(), new_test_application()),
        )?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let mut session_ids = Vec::new();
        for index in 0..2 {
            let session =
                tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                    id: format!("das-session-{index}"),
                    application: "flmtest".to_string(),
                    resreq: Some(ResourceRequirement {
                        cpu: 1,
                        memory: 1024,
                        gpu: 0,
                    }),
                    ..Default::default()
                }))?;
            session_ids.push(session.id);
        }

        let mut executor_ids = Vec::new();
        for index in 0..2 {
            let executor = tokio_test::block_on(
                controller.create_executor("node_1".to_string(), session_ids[0].clone()),
            )?;
            tokio_test::block_on(controller.register_executor(&executor))?;
            {
                let executor = controller.storage().get_executor_ptr(executor.id.clone())?;
                let mut executor = stdng::lock_ptr!(executor)?;
                executor
                    .attributes
                    .insert(Bytes::from(format!("key-{index}")));
            }
            executor_ids.push(executor.id);
        }

        let options = PluginsOptions {
            policies: vec!["priority".to_string(), "drf".to_string(), "das".to_string()],
            ..Default::default()
        };
        for index in 0..2 {
            tokio_test::block_on(controller.create_task(
                session_ids[index].clone(),
                None,
                Some(TaskOptions {
                    affinity: [Bytes::from(format!("key-{index}"))].into_iter().collect(),
                }),
            ))?;
            let mut ctx = Context::new(controller.clone(), &options)?;
            tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;

            let executor = controller.get_executor(executor_ids[index].clone())?;
            assert_eq!(executor.ssn_id.as_ref(), Some(&session_ids[index]));
        }

        Ok(())
    }

    fn instance_limit_env(
        application_max: u32,
        session_max: Option<u32>,
    ) -> Result<(TestEnv, String, PluginsOptions), FlameError> {
        let env = TestEnv::new()?;
        let controller = &env.controller;
        let mut application = new_test_application();
        application.max_instances = application_max;
        tokio_test::block_on(controller.register_application("flmtest".to_string(), application))?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;
        let session_id = format!("instance-limit-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: session_id.clone(),
            application: "flmtest".to_string(),
            max_instances: session_max,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
        // Match the deployed configuration: DRF supplies capacity while DAS
        // selects retained executors. Priority must not mask limit violations.
        let options = PluginsOptions {
            policies: vec!["drf".to_string(), "das".to_string()],
            ..Default::default()
        };
        Ok((env, session_id, options))
    }

    #[test]
    fn test_allocation_without_drf_stops_at_node_capacity() -> Result<(), FlameError> {
        for capacity in [
            ResourceRequirement {
                cpu: 2,
                memory: 10240,
                gpu: 0,
            },
            ResourceRequirement {
                cpu: 10,
                memory: 2048,
                gpu: 0,
            },
        ] {
            let (env, session_id, mut options) = instance_limit_env(1000, None)?;
            options.policies = vec!["priority".into(), "das".into()];
            let mut node = new_test_node("node_1".into());
            node.capacity = capacity.clone();
            node.allocatable = capacity;
            tokio_test::block_on(env.controller.storage().register_node(&node))?;
            for _ in 0..10 {
                tokio_test::block_on(env.controller.create_task(session_id.clone(), None, None))?;
            }
            for cycle in 0..5 {
                let mut context = Context::new(env.controller.clone(), &options)?;
                tokio_test::block_on(DispatchAction::new_ptr().execute(&mut context))?;
                tokio_test::block_on(AllocateAction::new_ptr().execute(&mut context))?;
                let executors = env.controller.list_executors()?;
                assert!(executors.len() <= 2, "cycle {cycle}");
                for executor in executors {
                    if executor.state == common::apis::ExecutorState::Void {
                        tokio_test::block_on(env.controller.register_executor(&executor))?;
                    }
                }
            }
            assert_eq!(env.controller.list_executors()?.len(), 2);
        }
        Ok(())
    }

    #[test]
    fn test_drf_gpu_allocation_stops_at_task_demand_while_binding() -> Result<(), FlameError> {
        let (env, session_id, options) = instance_limit_env(1000, None)?;
        let controller = &env.controller;
        let request = ResourceRequirement {
            gpu: 1,
            ..Default::default()
        };
        {
            let session = controller.storage().get_session_ptr(session_id.clone())?;
            stdng::lock_ptr!(session)?.resreq = Some(request.clone());
        }
        for name in ["node_1", "node_2"] {
            let mut node = new_test_node(name.to_string());
            node.capacity.gpu = 8;
            node.allocatable.gpu = 8;
            tokio_test::block_on(controller.storage().register_node(&node))?;
        }

        // First reproduce one pending task with no configured instance cap,
        // then ensure additional task demand still grows the executor pool.
        for demand in [1, 3] {
            let existing_tasks = controller.list_tasks(session_id.clone())?.len();
            for _ in existing_tasks..demand {
                tokio_test::block_on(controller.create_task(session_id.clone(), None, None))?;
            }
            for cycle in 0..20 {
                let mut ctx = Context::new(controller.clone(), &options)?;
                tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
                tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
                let executors = controller.list_executors()?;
                assert!(
                    executors.len() <= demand,
                    "cycle {cycle}: {demand} pending tasks must not consume every GPU while binding"
                );
                // FEM registration makes provisioned executors available for
                // binding; leave them Binding so no task has started yet.
                for executor in executors {
                    if executor.state == common::apis::ExecutorState::Void {
                        tokio_test::block_on(controller.register_executor(&executor))?;
                    }
                }
            }
            let executors = controller.list_executors()?;
            assert_eq!(executors.len(), demand);
            assert!(executors.iter().all(|executor| {
                executor.state == common::apis::ExecutorState::Binding
                    && executor.ssn_id.as_ref() == Some(&session_id)
                    && executor.resreq == request
            }));
        }
        Ok(())
    }

    #[test]
    fn test_dispatch_session_limit_preserves_das_selected_executor() -> Result<(), FlameError> {
        assert_dispatch_limit_preserves_das(Some(1), None)
    }

    #[test]
    fn test_dispatch_cluster_session_ceiling_preserves_das() -> Result<(), FlameError> {
        assert_dispatch_limit_preserves_das(None, Some(1))?;
        assert_dispatch_limit_preserves_das(Some(10), Some(1))
    }

    fn assert_dispatch_limit_preserves_das(
        session_limit: Option<u32>,
        cluster_limit: Option<u32>,
    ) -> Result<(), FlameError> {
        let (env, session_id, mut options) = instance_limit_env(10, session_limit)?;
        options.limits.session.max_executors = cluster_limit;
        let controller = &env.controller;
        let affinity = Bytes::from_static(b"retained-prefix");
        let mut executor_ids = Vec::new();
        for _ in 0..2 {
            let executor = tokio_test::block_on(
                controller.create_executor("node_1".to_string(), session_id.clone()),
            )?;
            tokio_test::block_on(controller.register_executor(&executor))?;
            executor_ids.push(executor.id);
        }
        {
            let executor = controller
                .storage()
                .get_executor_ptr(executor_ids[1].clone())?;
            stdng::lock_ptr!(executor)?
                .attributes
                .insert(affinity.clone());
        }
        tokio_test::block_on(controller.create_task(
            session_id.clone(),
            None,
            Some(TaskOptions {
                affinity: [affinity].into_iter().collect(),
            }),
        ))?;

        for _ in 0..3 {
            let mut ctx = Context::new(controller.clone(), &options)?;
            tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
            assert_eq!(
                controller
                    .list_executors()?
                    .iter()
                    .filter(|exec| exec.ssn_id.as_ref() == Some(&session_id))
                    .count(),
                1,
                "dispatch must bind only the DAS-selected executor"
            );
            tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;

            let executors = controller.list_executors()?;
            assert_eq!(
                executors.len(),
                2,
                "allocation must reuse the retained executor"
            );
            let selected = controller.get_executor(executor_ids[1].clone())?;
            assert_eq!(selected.ssn_id.as_ref(), Some(&session_id));
            assert_eq!(selected.state, common::apis::ExecutorState::Binding);
            let cold = controller.get_executor(executor_ids[0].clone())?;
            assert_eq!(
                cold.ssn_id, None,
                "the cold executor must not steal the task"
            );
            assert_eq!(cold.state, common::apis::ExecutorState::Idle);
        }
        Ok(())
    }

    #[test]
    fn test_dispatch_session_limit_allows_multiple_instances() -> Result<(), FlameError> {
        let (env, session_id, options) = instance_limit_env(10, Some(2))?;
        let controller = &env.controller;
        for _ in 0..3 {
            let executor = tokio_test::block_on(
                controller.create_executor("node_1".to_string(), session_id.clone()),
            )?;
            tokio_test::block_on(controller.register_executor(&executor))?;
            tokio_test::block_on(controller.create_task(session_id.clone(), None, None))?;
        }
        for _ in 0..2 {
            let mut ctx = Context::new(controller.clone(), &options)?;
            tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
            tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
            let executors = controller.list_executors()?;
            assert_eq!(executors.len(), 3);
            assert_eq!(
                executors
                    .iter()
                    .filter(|exec| exec.ssn_id.as_ref() == Some(&session_id))
                    .count(),
                2
            );
            assert_eq!(
                executors
                    .iter()
                    .filter(|exec| exec.state == common::apis::ExecutorState::Idle)
                    .count(),
                1
            );
        }
        Ok(())
    }

    #[test]
    fn test_allocate_session_limit_counts_inflight_executor() -> Result<(), FlameError> {
        let (env, session_id, options) = instance_limit_env(10, Some(1))?;
        let controller = &env.controller;
        tokio_test::block_on(controller.create_task(session_id.clone(), None, None))?;
        for cycle in 0..3 {
            let mut ctx = Context::new(controller.clone(), &options)?;
            tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
            tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
            let executors = controller.list_executors()?;
            assert_eq!(
                executors.len(),
                1,
                "an unregistered executor already reserves an instance"
            );
            if cycle == 0 {
                assert_eq!(executors[0].state, common::apis::ExecutorState::Void);
                tokio_test::block_on(controller.register_executor(&executors[0]))?;
            } else {
                assert_eq!(executors[0].state, common::apis::ExecutorState::Binding);
                assert_eq!(executors[0].ssn_id.as_ref(), Some(&session_id));
            }
        }
        Ok(())
    }

    #[test]
    fn test_allocate_application_limit_without_session_limit() -> Result<(), FlameError> {
        assert_application_limit_spans_sessions(1, None)
    }

    #[test]
    fn test_allocate_cluster_application_ceiling_spans_sessions() -> Result<(), FlameError> {
        assert_application_limit_spans_sessions(10, Some(1))
    }

    fn assert_application_limit_spans_sessions(
        application_limit: u32,
        cluster_limit: Option<u32>,
    ) -> Result<(), FlameError> {
        let (env, session_id, mut options) = instance_limit_env(application_limit, None)?;
        options.limits.application.max_executors = cluster_limit;
        let controller = &env.controller;
        tokio_test::block_on(controller.create_task(session_id.clone(), None, None))?;
        let second_session =
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                id: format!("instance-limit-second-{}", Uuid::new_v4()),
                application: "flmtest".to_string(),
                max_instances: None,
                resreq: Some(ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            }))?;
        tokio_test::block_on(controller.create_task(second_session.id, None, None))?;
        for cycle in 0..3 {
            let mut ctx = Context::new(controller.clone(), &options)?;
            tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
            tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
            let executors = controller.list_executors()?;
            assert_eq!(
                executors.len(),
                1,
                "the application limit spans all sessions"
            );
            if cycle == 0 {
                tokio_test::block_on(controller.register_executor(&executors[0]))?;
            }
        }
        Ok(())
    }

    #[test]
    fn test_dispatch_zero_session_limit_keeps_retained_executor_idle() -> Result<(), FlameError> {
        let (env, session_id, options) = instance_limit_env(10, Some(0))?;
        let controller = &env.controller;
        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), session_id.clone()),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;
        tokio_test::block_on(controller.create_task(session_id, None, None))?;
        let mut ctx = Context::new(controller.clone(), &options)?;
        tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
        tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
        let executors = controller.list_executors()?;
        assert_eq!(executors.len(), 1);
        assert_eq!(executors[0].state, common::apis::ExecutorState::Idle);
        assert_eq!(executors[0].ssn_id, None);
        Ok(())
    }

    #[test]
    fn test_allocate_zero_application_limit_creates_no_executor() -> Result<(), FlameError> {
        let (env, session_id, options) = instance_limit_env(0, None)?;
        tokio_test::block_on(env.controller.create_task(session_id, None, None))?;
        let mut ctx = Context::new(env.controller.clone(), &options)?;
        tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
        assert!(env.controller.list_executors()?.is_empty());
        Ok(())
    }

    #[test]
    fn test_scheduler_skips_not_ready_session() -> Result<(), FlameError> {
        let env = TestEnv::new_with_retry_limit(1)?;
        let controller = env.controller.clone();

        tokio_test::block_on(
            controller.register_application("flmtest".to_string(), new_test_application()),
        )?;
        tokio_test::block_on(
            controller
                .storage()
                .register_node(&new_test_node("node_1".to_string())),
        )?;

        let ssn_id = format!("not-ready-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
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
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None, None))?;

        {
            let ssn_ptr = controller.storage().get_session_ptr(ssn_id.clone())?;
            let mut ssn = stdng::lock_ptr!(ssn_ptr)?;
            ssn.retry_count = 1;
        }

        let executor =
            tokio_test::block_on(controller.create_executor("node_1".to_string(), ssn_id.clone()))?;
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
        assert_eq!(executors[0].ssn_id, None);

        Ok(())
    }
}

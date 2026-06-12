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

use crate::controller::ControllerPtr;
use crate::scheduler::ctx::Context;

use crate::FlameThread;
use common::ctx::FlameClusterContext;
use common::FlameError;

mod actions;
mod ctx;
mod plugins;
pub mod statement;

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
        let policies = &flame_ctx.cluster.policies;
        tracing::info!(
            "Scheduler started with interval: {}ms, enabled policies: {:?}",
            schedule_interval,
            policies
        );

        loop {
            let mut ctx = Context::new(self.controller.clone(), policies)?;

            // Same `ctx` (and thus same in-memory `plugins`) for every action: Dispatch mutations
            // are visible to Allocate (e.g. Gang `is_fulfilled` / `is_ready` after binds).
            for action in ctx.actions.clone() {
                if let Err(e) = action.execute(&mut ctx).await {
                    tracing::error!("Failed to run scheduling: {e}");
                    break;
                };
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(schedule_interval)).await;
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
    use crate::scheduler::plugins::PluginManager;
    use crate::scheduler::ControllerPtr;
    use crate::storage;
    use chrono::Duration;
    use chrono::Utc;
    use common::apis::{
        Application, ApplicationAttributes, Node, NodeInfo, NodeState, ResourceRequirement, Shim,
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

    /// With the default policy (priority + gang, batch_size=1), AllocateAction must create
    /// exactly `task_num` executors — one per pending task — in the first scheduling cycle,
    /// and must NOT create additional executors in subsequent cycles.
    ///
    /// The gang plugin ensures `needed = (task_num / 1) * 1 = task_num`.  Once all executors
    /// are in snapshot (`allocated == task_num`), `is_ready = Some(true)` causes the pre-check
    /// to skip the session, so the executor count stays stable across all subsequent cycles.
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
            tokio_test::block_on(controller.create_task(ssn_1.id.clone(), None))?;
        }

        for i in 0..10 {
            let snapshot = controller.snapshot()?;
            let default_policies: Vec<String> = common::ctx::DEFAULT_POLICIES
                .iter()
                .map(|s| s.to_string())
                .collect();
            let plugins = PluginManager::setup(&snapshot.clone(), &default_policies)?;

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

            let exec_list = controller.list_executor()?;
            assert_eq!(
                exec_list.len(),
                task_num as usize,
                "cycle {i}: expected {task_num} executors (one per task), got {}",
                exec_list.len()
            );
        }

        Ok(())
    }

    /// Regression test: when only the DRF plugin is enabled (without Gang), tasks must not stay
    /// pending forever.
    ///
    /// Previously, `is_fulfilled` and `is_ready` both defaulted to `true` when no plugin
    /// implemented them — `AllocateAction`'s pre-check `if fulfilled || ready { continue }`
    /// caused it to skip every session before any executor was created.
    ///
    /// The fix changes `is_ready`/`is_fulfilled` to return `Option<bool>`.  When Gang is not
    /// loaded, both return `None` (no opinion).  The pre-check only skips on `Some(true)`;
    /// `None` is treated as "no batch constraint — 1 executor per cycle is enough".
    #[test]
    fn test_drf_only_policy_allocates_executor() -> Result<(), FlameError> {
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

        let ssn_id = format!("drf-only-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
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
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;

        // Use DRF-only policy (no explicit "gang" — Gang is now always-on internally).
        let drf_only: Vec<String> = vec!["drf".to_string()];

        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot, &drf_only)?;
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

        // An executor must have been created; without the fix it would be 0.
        let exec_list = controller.list_executor()?;
        assert_eq!(
            exec_list.len(),
            1,
            "DRF-only policy must create an executor for a pending task"
        );

        Ok(())
    }

    /// One scheduling cycle must keep the same in-memory [`crate::scheduler::plugins::PluginManager`]
    /// so Gang (and similar) state from Dispatch is visible to Allocate.
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

        let default_policies: Vec<String> = common::ctx::DEFAULT_POLICIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut ctx = Context::new(controller.clone(), &default_policies)?;
        let plugins_ptr = Arc::as_ptr(&ctx.plugins);
        for action in ctx.actions.clone() {
            tokio_test::block_on(action.execute(&mut ctx))?;
        }
        assert_eq!(plugins_ptr, Arc::as_ptr(&ctx.plugins));
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
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;

        {
            let ssn_ptr = controller.storage().get_session_ptr(ssn_id.clone())?;
            let mut ssn = stdng::lock_ptr!(ssn_ptr)?;
            ssn.retry_count = 1;
        }

        let executor =
            tokio_test::block_on(controller.create_executor("node_1".to_string(), ssn_id.clone()))?;
        tokio_test::block_on(controller.register_executor(&executor))?;

        let default_policies: Vec<String> = common::ctx::DEFAULT_POLICIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut ctx = Context::new(controller.clone(), &default_policies)?;

        let dispatch = DispatchAction::new_ptr();
        tokio_test::block_on(dispatch.execute(&mut ctx))?;

        let alloc = AllocateAction::new_ptr();
        tokio_test::block_on(alloc.execute(&mut ctx))?;

        let executors = controller.list_executor()?;
        assert_eq!(executors.len(), 1);
        assert_eq!(executors[0].state, common::apis::ExecutorState::Idle);
        assert_eq!(executors[0].ssn_id, None);

        Ok(())
    }

    /// Regression: with only the priority plugin (no gang, no DRF), a session with a pending
    /// task must still get an executor allocated.
    ///
    /// Previously, without gang, `is_ready` defaulted to `true` (unwrap_or) so AllocateAction's
    /// pre-check always skipped every session.  The fix changes `is_ready` / `is_fulfilled` to
    /// return `Option<bool>`; `None` (no gang opinion) is no longer treated as "already ready".
    #[test]
    fn test_priority_only_policy_allocates_executor() -> Result<(), FlameError> {
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

        let ssn_id = format!("priority-only-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
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
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;

        let priority_only: Vec<String> = vec!["priority".to_string()];
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot, &priority_only)?;
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

        let exec_list = controller.list_executor()?;
        assert_eq!(
            exec_list.len(),
            1,
            "priority-only policy must create exactly 1 executor for a pending task"
        );
        Ok(())
    }

    /// Regression: with gang plugin, batch_size=1, and multiple pending tasks, AllocateAction
    /// must create one executor per pending task — not stop at the first one.
    ///
    /// Root cause: the old `is_ready` formula returned `Some(true)` as soon as `allocated=1`
    /// (because `1 % 1 == 0`), so the pre-check in AllocateAction skipped the session for all
    /// subsequent cycles, leaving tasks 2…N without executors forever.
    ///
    /// With the fixed formula (`needed = (incomplete_tasks / batch_size) * batch_size`), a
    /// session with 3 pending tasks has `needed=3`.  The pre-check fires only when `total==3`,
    /// so AllocateAction creates all 3 executors in a single scheduling cycle.
    #[test]
    fn test_gang_batch_size_1_allocates_for_each_task() -> Result<(), FlameError> {
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

        let ssn_id = format!("gang-batch1-multi-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
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
        // 3 pending tasks → gang plugin needs 3 executors (needed = (3/1)*1 = 3)
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;

        let gang_policies: Vec<String> = vec!["priority".to_string(), "gang".to_string()];
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot, &gang_policies)?;
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

        let exec_list = controller.list_executor()?;
        assert_eq!(
            exec_list.len(),
            3,
            "gang batch_size=1 with 3 pending tasks must create 3 executors in one cycle (got {})",
            exec_list.len()
        );
        Ok(())
    }

    /// With the gang plugin and batch_size=2, AllocateAction must create exactly 2 executors
    /// in a single cycle (the full gang batch), not 1 and not more than 2.
    #[test]
    fn test_gang_batch_size_2_creates_two_executors_per_cycle() -> Result<(), FlameError> {
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

        let ssn_id = format!("gang-batch2-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 2,
            priority: 0,
            resreq: Some(common::apis::ResourceRequirement {
                cpu: 1,
                memory: 1024 * 1024 * 1024,
                gpu: 0,
            }),
        }))?;
        // Create 2 pending tasks so the session is considered underused
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;

        // Both priority (for is_underused) and gang (for batch scheduling) are required.
        let gang_policies: Vec<String> = vec!["priority".to_string(), "gang".to_string()];
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot, &gang_policies)?;
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

        let exec_list = controller.list_executor()?;
        assert_eq!(
            exec_list.len(),
            2,
            "gang batch_size=2 must create exactly 2 executors in one scheduling cycle"
        );
        Ok(())
    }

    /// Without gang, AllocateAction must allocate at most 1 executor per cycle per session,
    /// even when the node has plenty of capacity and there are many pending tasks.
    ///
    /// This guards against the "fill the node" regression that would occur if is_ready=None
    /// were treated as "keep allocating".
    #[test]
    fn test_no_gang_allocates_at_most_one_executor_per_cycle() -> Result<(), FlameError> {
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

        let ssn_id = format!("no-gang-cap-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
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
        // 5 pending tasks — node has 64 CPUs so without a cap we'd create 5+ executors
        for _ in 0..5 {
            tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;
        }

        // Priority-only: no gang, no DRF
        let priority_only: Vec<String> = vec!["priority".to_string()];
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot, &priority_only)?;
        let mut ctx = Context {
            snapshot: snapshot.clone(),
            controller: controller.clone(),
            plugins,
            actions: vec![],
        };

        let alloc = AllocateAction::new_ptr();
        tokio_test::block_on(alloc.execute(&mut ctx))?;

        let exec_list = controller.list_executor()?;
        assert_eq!(
            exec_list.len(),
            1,
            "without gang, AllocateAction must create at most 1 executor per cycle (got {})",
            exec_list.len()
        );
        Ok(())
    }

    /// DispatchAction must bind an idle executor to a session even when no gang plugin is
    /// loaded (is_fulfilled returns None).
    ///
    /// Previously, when is_fulfilled defaulted to true (no gang), DispatchAction's pre-check
    /// `if is_fulfilled { skip }` would skip every session.  Now the pre-check only skips on
    /// `Some(true)`; `None` proceeds with binding.
    #[test]
    fn test_dispatch_without_gang_binds_idle_executor() -> Result<(), FlameError> {
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

        let ssn_id = format!("dispatch-no-gang-{}", Uuid::new_v4());
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
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
        tokio_test::block_on(controller.create_task(ssn_id.clone(), None))?;

        // Pre-create an idle executor bound to the session (simulate what AllocateAction would do)
        let executor =
            tokio_test::block_on(controller.create_executor("node_1".to_string(), ssn_id.clone()))?;
        tokio_test::block_on(controller.register_executor(&executor))?;

        // Priority-only (no gang)
        let priority_only: Vec<String> = vec!["priority".to_string()];
        let snapshot = controller.snapshot()?;
        let plugins = PluginManager::setup(&snapshot, &priority_only)?;
        let mut ctx = Context {
            snapshot: snapshot.clone(),
            controller: controller.clone(),
            plugins,
            actions: vec![],
        };

        let dispatch = DispatchAction::new_ptr();
        tokio_test::block_on(dispatch.execute(&mut ctx))?;

        // The executor must have transitioned from Idle → Binding
        let executors = controller.list_executor()?;
        assert_eq!(executors.len(), 1);
        assert_eq!(
            executors[0].state,
            common::apis::ExecutorState::Binding,
            "DispatchAction must bind the idle executor even without gang plugin"
        );
        Ok(())
    }
}

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
use rand::RngExt;

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
            ..Default::default()
        },
        state: NodeState::Ready,
        shims: vec![common::apis::Shim::Host],
    }
}

struct TestEnv {
    _directory: tempfile::TempDir,
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
            .add_directive("tower=error".parse()?);

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .with_target(true)
            .with_ansi(false)
            .try_init();

        let directory =
            tempfile::tempdir().map_err(|error| FlameError::Storage(error.to_string()))?;
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: format!("fs://{}", directory.path().display()),
                recovery: FlameRecovery {
                    session: FlameSessionRecovery { retry_limits },
                },
                ..Default::default()
            },
            ..Default::default()
        };

        let storage = tokio_test::block_on(storage::new_ptr(&config))?;
        let controller = controller::new_ptr(storage.clone());

        Ok(Self {
            _directory: directory,
            controller,
        })
    }
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
            batch_size: 1,
            id: ssn_id.clone(),
            application: "flmtest".to_string(),
            common_data: None,
            tokens: Default::default(),
            min_instances: 0,
            max_instances: None,
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

/// Two Idle executors and a session whose pending task asks for `wanted`. The executor with the
/// smaller ID publishes three attributes and the other none, and `both_hold_wanted` adds `wanted`
/// to both. Returns whether Dispatch bound the executor with fewer attributes.
fn dispatch_binds_the_emptier_executor(both_hold_wanted: bool) -> Result<bool, FlameError> {
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
    let session =
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: "das-empty-cache".to_string(),
            application: "flmtest".to_string(),
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;

    let mut executor_ids = Vec::new();
    for _ in 0..2 {
        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), session.id.clone()),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;
        executor_ids.push(executor.id);
    }
    executor_ids.sort();
    let (smaller_id, larger_id) = (executor_ids[0].clone(), executor_ids[1].clone());
    for (id, extra) in [(&smaller_id, 3), (&larger_id, 0)] {
        let executor = controller.storage().get_executor_ptr(id.clone())?;
        let mut executor = stdng::lock_ptr!(executor)?;
        for index in 0..extra {
            executor
                .attributes
                .insert(Bytes::from(format!("held-{index}")));
        }
        if both_hold_wanted {
            executor.attributes.insert(Bytes::from_static(b"wanted"));
        }
    }

    tokio_test::block_on(controller.create_task(
        session.id.clone(),
        None,
        Some(TaskOptions {
            affinity: [Bytes::from_static(b"wanted")].into_iter().collect(),
        }),
    ))?;
    let options = PluginsOptions {
        policies: vec!["priority".to_string(), "drf".to_string(), "das".to_string()],
        ..Default::default()
    };
    let mut ctx = Context::new(controller.clone(), &options)?;
    tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;

    let bound = controller.get_executor(larger_id)?.ssn_id.as_ref() == Some(&session.id);
    let other = controller.get_executor(smaller_id)?.ssn_id.as_ref() == Some(&session.id);
    assert_ne!(bound, other, "exactly one executor is bound");
    Ok(bound)
}

/// Two Idle executors with no attributes and a session whose pending task asks for a key nobody
/// holds. The executor with the smaller ID was updated just now and the other ten minutes ago.
/// Returns whether Dispatch bound the executor that has been Idle longer.
fn dispatch_binds_the_executor_idle_longest() -> Result<bool, FlameError> {
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
    let session =
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: "das-idle-longest".to_string(),
            application: "flmtest".to_string(),
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
    let mut executor_ids = Vec::new();
    for _ in 0..2 {
        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), session.id.clone()),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;
        executor_ids.push(executor.id);
    }
    executor_ids.sort();
    let (smaller_id, larger_id) = (executor_ids[0].clone(), executor_ids[1].clone());
    for (id, idle_minutes) in [(&smaller_id, 0), (&larger_id, 10)] {
        let executor = controller.storage().get_executor_ptr(id.clone())?;
        let mut executor = stdng::lock_ptr!(executor)?;
        executor.latest_updated_timestamp =
            chrono::Utc::now() - chrono::Duration::minutes(idle_minutes);
    }

    tokio_test::block_on(controller.create_task(
        session.id.clone(),
        None,
        Some(TaskOptions {
            affinity: [Bytes::from_static(b"wanted")].into_iter().collect(),
        }),
    ))?;
    let options = PluginsOptions {
        policies: vec!["priority".to_string(), "drf".to_string(), "das".to_string()],
        ..Default::default()
    };
    let mut ctx = Context::new(controller.clone(), &options)?;
    tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
    Ok(controller.get_executor(larger_id)?.ssn_id.as_ref() == Some(&session.id))
}

/// Runs one Dispatch over `idle` retained Idle executors and a session of
/// `batch_size` with one pending task; returns the executors bound to it.
fn gang_dispatch(batch_size: u32, idle: usize) -> Result<usize, FlameError> {
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
    let session =
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: format!("gang-{}", Uuid::new_v4()),
            application: "flmtest".to_string(),
            batch_size,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
    tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
    for _ in 0..idle {
        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), session.id.clone()),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;
    }

    let mut ctx = Context::new(controller.clone(), &PluginsOptions::default())?;
    tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;

    Ok(controller
        .list_executors()?
        .iter()
        .filter(|executor| executor.ssn_id.as_ref() == Some(&session.id))
        .count())
}

/// Runs `cycles` full scheduling cycles (Dispatch then Allocate); between
/// cycles every Void executor registers, as its node would. Returns
/// (executors that exist, executors bound to the gang session).
fn gang_cycles(batch_size: u32, idle: usize, cycles: usize) -> Result<(usize, usize), FlameError> {
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
    let session =
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: format!("gang-{}", Uuid::new_v4()),
            application: "flmtest".to_string(),
            batch_size,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
    tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
    for _ in 0..idle {
        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), session.id.clone()),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;
    }

    for _ in 0..cycles {
        let mut ctx = Context::new(controller.clone(), &PluginsOptions::default())?;
        tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
        tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
        for executor in controller.list_executors()? {
            if executor.state == common::apis::ExecutorState::Void {
                tokio_test::block_on(controller.register_executor(&executor))?;
            }
        }
    }

    let executors = controller.list_executors()?;
    let bound = executors
        .iter()
        .filter(|executor| executor.ssn_id.as_ref() == Some(&session.id))
        .count();
    Ok((executors.len(), bound))
}

fn competing_gangs(policies: &[&str], size: u32, cycles: usize) -> Result<Vec<usize>, FlameError> {
    let env = TestEnv::new()?;
    let controller = env.controller.clone();
    tokio_test::block_on(
        controller.register_application("flmtest".to_string(), new_test_application()),
    )?;
    let mut node = new_test_node("node_1".to_string());
    node.capacity = ResourceRequirement {
        cpu: 6,
        memory: 6 * 1024,
        gpu: 0,
    };
    node.allocatable = node.capacity.clone();
    tokio_test::block_on(controller.storage().register_node(&node))?;
    let mut sessions = vec![];
    for _ in 0..2 {
        let session =
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                id: format!("gang-{}", Uuid::new_v4()),
                application: "flmtest".to_string(),
                batch_size: size,
                min_instances: size,
                resreq: Some(ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            }))?;
        tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
        sessions.push(session);
    }
    let options = PluginsOptions {
        policies: policies.iter().map(|p| p.to_string()).collect(),
        ..Default::default()
    };
    for _ in 0..cycles {
        let mut ctx = Context::new(controller.clone(), &options)?;
        tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
        tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
        for executor in controller.list_executors()? {
            if executor.state == common::apis::ExecutorState::Void {
                tokio_test::block_on(controller.register_executor(&executor))?;
            }
        }
    }
    sessions
        .iter()
        .map(|session| {
            controller.list_executors().map(|all| {
                all.iter()
                    .filter(|e| e.ssn_id.as_ref() == Some(&session.id))
                    .count()
            })
        })
        .collect()
}

/// A cluster of one node with `slots` executors of cpu 1, and a session of `batch_size` and
/// `min_instances` with `tasks` pending tasks that runs `cycles` scheduling cycles. Returns the
/// number of executors the session holds.
fn executors_held(
    policies: &[&str],
    slots: u32,
    batch_size: u32,
    min_instances: u32,
    tasks: u32,
    cycles: usize,
) -> Result<usize, FlameError> {
    let env = TestEnv::new()?;
    let controller = env.controller.clone();
    tokio_test::block_on(
        controller.register_application("flmtest".to_string(), new_test_application()),
    )?;
    let mut node = new_test_node("node_1".to_string());
    node.capacity = ResourceRequirement {
        cpu: slots as u64,
        memory: slots as u64 * 1024,
        gpu: 0,
    };
    node.allocatable = node.capacity.clone();
    tokio_test::block_on(controller.storage().register_node(&node))?;
    let session =
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: format!("kept-warm-{}", Uuid::new_v4()),
            application: "flmtest".to_string(),
            batch_size,
            min_instances,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
    for _ in 0..tasks {
        tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
    }
    let options = PluginsOptions {
        policies: policies.iter().map(|p| p.to_string()).collect(),
        ..Default::default()
    };
    for _ in 0..cycles {
        cycle(&controller, &options)?;
    }
    Ok(controller
        .list_executors()?
        .iter()
        .filter(|e| e.ssn_id.as_deref() == Some(session.id.as_str()))
        .count())
}

fn node_named(name: &str, state: NodeState) -> Node {
    let mut node = new_test_node(name.to_string());
    node.state = state;
    node
}

fn cycle(controller: &ControllerPtr, options: &PluginsOptions) -> Result<(), FlameError> {
    let mut ctx = Context::new(controller.clone(), options)?;
    tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;
    tokio_test::block_on(AllocateAction::new_ptr().execute(&mut ctx))?;
    for executor in controller.list_executors()? {
        if executor.state == common::apis::ExecutorState::Void {
            tokio_test::block_on(controller.register_executor(&executor))?;
        }
    }
    Ok(())
}

fn session_with_task(controller: &ControllerPtr) -> Result<common::apis::Session, FlameError> {
    let session =
        tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
            id: format!("ready-nodes-{}", Uuid::new_v4()),
            application: "flmtest".to_string(),
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
    tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
    Ok(session)
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

mod scheduler;

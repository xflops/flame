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
    let ssn_1 = tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
        batch_size: 1,
        id: ssn_1_id.clone(),
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

/// With equal scores and equal attribute counts, the session goes to the executor that has been
/// Idle the longest, not to the smaller executor ID.
#[test]
fn test_dispatch_with_equal_das_scores_and_attributes_prefers_the_executor_idle_longest(
) -> Result<(), FlameError> {
    assert!(dispatch_binds_the_executor_idle_longest()?);
    Ok(())
}

/// With no executor holding the key, or with both holding it, the session goes to the executor
/// that publishes fewer attributes, not to the smaller executor ID.
#[test]
fn test_dispatch_with_equal_das_scores_prefers_the_emptier_executor() -> Result<(), FlameError> {
    assert!(dispatch_binds_the_emptier_executor(false)?, "no match");
    assert!(dispatch_binds_the_emptier_executor(true)?, "equal match");
    Ok(())
}

#[test]
fn test_dispatch_gang_binds_whole_batch_at_once() -> Result<(), FlameError> {
    assert_eq!(gang_dispatch(2, 2)?, 2);
    assert_eq!(gang_dispatch(3, 3)?, 3);
    Ok(())
}

#[test]
fn test_dispatch_gang_binds_nothing_with_partial_supply() -> Result<(), FlameError> {
    assert_eq!(gang_dispatch(2, 1)?, 0);
    assert_eq!(gang_dispatch(3, 2)?, 0);
    Ok(())
}

#[test]
fn test_dispatch_gang_binds_only_batch_size_for_one_pending_task() -> Result<(), FlameError> {
    assert_eq!(gang_dispatch(2, 3)?, 2);
    Ok(())
}

#[test]
fn test_dispatch_single_instance_batch_is_not_a_gang() -> Result<(), FlameError> {
    assert_eq!(gang_dispatch(1, 1)?, 1);
    Ok(())
}

#[test]
fn test_dispatch_two_gangs_with_supply_for_one() -> Result<(), FlameError> {
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
    let mut ids = vec![];
    for index in 0..2 {
        let session =
            tokio_test::block_on(controller.create_session(common::apis::SessionAttributes {
                id: format!("gang-{index}"),
                application: "flmtest".to_string(),
                batch_size: 2,
                resreq: Some(ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            }))?;
        tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
        ids.push(session.id);
    }
    for _ in 0..2 {
        let executor =
            tokio_test::block_on(controller.create_executor("node_1".to_string(), ids[0].clone()))?;
        tokio_test::block_on(controller.register_executor(&executor))?;
    }

    let mut ctx = Context::new(controller.clone(), &PluginsOptions::default())?;
    tokio_test::block_on(DispatchAction::new_ptr().execute(&mut ctx))?;

    let bound = |id: &String| {
        controller
            .list_executors()
            .unwrap()
            .iter()
            .filter(|executor| executor.ssn_id.as_ref() == Some(id))
            .count()
    };
    let mut counts = [bound(&ids[0]), bound(&ids[1])];
    counts.sort();
    assert_eq!(counts, [0, 2]);
    Ok(())
}

#[test]
fn test_statement_discard_restores_plugins_and_snapshot() -> Result<(), FlameError> {
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
            id: "stmt".to_string(),
            application: "flmtest".to_string(),
            batch_size: 2,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        }))?;
    tokio_test::block_on(controller.create_task(session.id.clone(), None, None))?;
    let mut executor_ids = vec![];
    for _ in 0..2 {
        let executor = tokio_test::block_on(
            controller.create_executor("node_1".to_string(), session.id.clone()),
        )?;
        tokio_test::block_on(controller.register_executor(&executor))?;
        executor_ids.push(executor.id);
    }

    let ctx = Context::new(controller.clone(), &PluginsOptions::default())?;
    let info = ctx
        .snapshot
        .find_sessions(crate::model::READY_SESSION)?
        .values()
        .next()
        .cloned()
        .unwrap();
    let idle = ctx
        .snapshot
        .find_executors(crate::model::IDLE_EXECUTOR)?
        .values()
        .cloned()
        .collect::<Vec<_>>();
    assert!(!ctx.is_satisfied(&info)?);

    let mut statement = crate::scheduler::statement::Statement::new(&ctx);
    statement.bind(&idle[0], &info)?;
    assert!(!ctx.is_satisfied(&info)?);
    statement.bind(&idle[1], &info)?;
    assert!(ctx.is_satisfied(&info)?);
    assert!(ctx
        .snapshot
        .find_executors(crate::model::IDLE_EXECUTOR)?
        .is_empty());

    assert_eq!(statement.discard()?.len(), 2);
    assert!(!ctx.is_satisfied(&info)?);
    assert_eq!(
        ctx.snapshot
            .find_executors(crate::model::IDLE_EXECUTOR)?
            .len(),
        2
    );
    // The controller was never touched.
    for id in executor_ids {
        assert!(controller.get_executor(id)?.ssn_id.is_none());
    }
    Ok(())
}

#[test]
fn test_gang_assembles_from_nothing_without_over_creation() -> Result<(), FlameError> {
    assert_eq!(gang_cycles(3, 0, 10)?, (3, 3));
    Ok(())
}

#[test]
fn test_gang_assembles_from_partial_idle_without_over_creation() -> Result<(), FlameError> {
    assert_eq!(gang_cycles(3, 2, 10)?, (3, 3));
    assert_eq!(gang_cycles(4, 1, 10)?, (4, 4));
    Ok(())
}

#[test]
fn test_competing_gangs_never_hold_a_partial_set() -> Result<(), FlameError> {
    for policies in [["priority", "das"], ["drf", "das"]] {
        let held = competing_gangs(&policies, 4, 12)?;
        assert!(
            held.iter().all(|count| *count == 0 || *count == 4),
            "{policies:?}: {held:?}"
        );
    }
    Ok(())
}

/// A gang is created with `min_instances` equal to its size so its executors stay, and the
/// session exists before its first task. Idle executors must not be bound to it one by one in
/// between: the gang is formed all at once, or not at all.
#[test]
fn test_gang_kept_warm_binds_all_at_once_even_without_a_task() -> Result<(), FlameError> {
    for policies in [["priority", "das"], ["drf", "das"]] {
        // Larger than the cluster: nothing is bound, with a task or without.
        assert_eq!(
            executors_held(&policies, 6, 7, 7, 0, 12)?,
            0,
            "{policies:?}"
        );
        assert_eq!(
            executors_held(&policies, 6, 7, 7, 7, 12)?,
            0,
            "{policies:?}"
        );
        // It fits: all of it is bound, with a task or without.
        assert_eq!(
            executors_held(&policies, 7, 7, 7, 0, 12)?,
            7,
            "{policies:?}"
        );
        assert_eq!(
            executors_held(&policies, 7, 7, 7, 7, 12)?,
            7,
            "{policies:?}"
        );
        // Not kept warm: it waits for its tasks.
        assert_eq!(
            executors_held(&policies, 7, 7, 0, 0, 12)?,
            0,
            "{policies:?}"
        );
    }
    Ok(())
}

#[test]
fn test_drf_rebalances_a_session_holding_the_whole_cluster() -> Result<(), FlameError> {
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
        sessions.push(tokio_test::block_on(controller.create_session(
            common::apis::SessionAttributes {
                id: format!("fair-{}", Uuid::new_v4()),
                application: "flmtest".to_string(),
                resreq: Some(ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                }),
                ..Default::default()
            },
        ))?);
    }
    let options = PluginsOptions {
        policies: vec!["drf".to_string(), "das".to_string()],
        ..Default::default()
    };
    let cycle = |controller: &ControllerPtr| -> Result<(), FlameError> {
        let mut ctx = Context::new(controller.clone(), &options)?;
        for action in ctx.actions.clone() {
            tokio_test::block_on(action.execute(&mut ctx))?;
        }
        for executor in controller.list_executors()? {
            match executor.state {
                common::apis::ExecutorState::Void => {
                    tokio_test::block_on(controller.register_executor(&executor))?
                }
                common::apis::ExecutorState::Binding => tokio_test::block_on(
                    controller.bind_session_completed(executor.id.clone(), None),
                )?,
                _ => {}
            }
        }
        Ok(())
    };
    for _ in 0..12 {
        tokio_test::block_on(controller.create_task(sessions[0].id.clone(), None, None))?;
    }
    for _ in 0..20 {
        cycle(&controller)?;
    }
    let held = |ssn: &str| -> Result<usize, FlameError> {
        Ok(controller
            .list_executors()?
            .iter()
            .filter(|e| e.ssn_id.as_deref() == Some(ssn))
            .count())
    };
    assert_eq!(held(&sessions[0].id)?, 6);
    for _ in 0..12 {
        tokio_test::block_on(controller.create_task(sessions[1].id.clone(), None, None))?;
    }
    for _ in 0..3 {
        cycle(&controller)?;
    }
    let interrupted = controller
        .list_executors()?
        .iter()
        .filter(|e| e.signal == Some(common::apis::Signal::Interrupt))
        .count();
    assert_eq!(interrupted, 3, "re-balancing must stop at an equal split");
    Ok(())
}

/// An executor created for a node whose executor manager is not connected is never started
/// and stays `Void`, so the session waits forever.
#[test]
fn test_no_executor_is_allocated_on_a_node_that_is_not_ready() -> Result<(), FlameError> {
    let env = TestEnv::new()?;
    let controller = env.controller.clone();
    tokio_test::block_on(
        controller.register_application("flmtest".to_string(), new_test_application()),
    )?;
    tokio_test::block_on(
        controller
            .storage()
            .register_node(&node_named("node_dead", NodeState::NotReady)),
    )?;
    session_with_task(&controller)?;
    let options = PluginsOptions::default();

    for _ in 0..5 {
        cycle(&controller, &options)?;
    }
    assert!(
        controller.list_executors()?.is_empty(),
        "an executor was created on a node that is not ready"
    );

    // The node comes back: the executor is allocated there.
    tokio_test::block_on(
        controller
            .storage()
            .register_node(&node_named("node_dead", NodeState::Ready)),
    )?;
    for _ in 0..5 {
        cycle(&controller, &options)?;
    }
    let executors = controller.list_executors()?;
    assert_eq!(executors.len(), 1);
    assert_eq!(executors[0].node, "node_dead");
    Ok(())
}

/// An executor created for a node whose executor manager does not run the shim of the
/// application cannot be started there, so no executor is allocated on that node.
#[test]
fn test_no_executor_is_allocated_on_a_node_that_does_not_run_the_shim() -> Result<(), FlameError> {
    let env = TestEnv::new()?;
    let controller = env.controller.clone();
    // The application of the test uses the default (host) shim.
    tokio_test::block_on(
        controller.register_application("flmtest".to_string(), new_test_application()),
    )?;
    let mut cri_node = node_named("node_cri", NodeState::Ready);
    cri_node.shims = vec![common::apis::Shim::Cri];
    tokio_test::block_on(controller.storage().register_node(&cri_node))?;
    session_with_task(&controller)?;
    let options = PluginsOptions::default();

    for _ in 0..5 {
        cycle(&controller, &options)?;
    }
    assert!(
        controller.list_executors()?.is_empty(),
        "an executor was created on a node that does not run the application's shim"
    );

    // A node that runs both shims takes it, though the CRI node is still there.
    let mut both = node_named("node_both", NodeState::Ready);
    both.shims = vec![common::apis::Shim::Cri, common::apis::Shim::Host];
    tokio_test::block_on(controller.storage().register_node(&both))?;
    for _ in 0..5 {
        cycle(&controller, &options)?;
    }
    let executors = controller.list_executors()?;
    assert_eq!(executors.len(), 1);
    assert_eq!(executors[0].node, "node_both");
    assert_eq!(executors[0].shim, common::apis::Shim::Host);
    Ok(())
}

#[test]
fn test_void_executor_on_a_node_that_is_not_ready_is_not_reused() -> Result<(), FlameError> {
    let env = TestEnv::new()?;
    let controller = env.controller.clone();
    tokio_test::block_on(
        controller.register_application("flmtest".to_string(), new_test_application()),
    )?;
    for node in [
        node_named("node_dead", NodeState::NotReady),
        node_named("node_ok", NodeState::Ready),
    ] {
        tokio_test::block_on(controller.storage().register_node(&node))?;
    }
    let session = session_with_task(&controller)?;
    // An executor left behind on the dead node, as the incident did.
    let stranded = tokio_test::block_on(
        controller.create_executor("node_dead".to_string(), session.id.clone()),
    )?;
    let options = PluginsOptions::default();

    for _ in 0..8 {
        cycle(&controller, &options)?;
    }
    let on_ok = controller
        .list_executors()?
        .into_iter()
        .filter(|e| e.node == "node_ok" && e.ssn_id.as_ref() == Some(&session.id))
        .count();
    assert_eq!(
        on_ok, 1,
        "the session did not get an executor on the ready node"
    );
    let stranded = controller
        .list_executors()?
        .into_iter()
        .find(|e| e.id == stranded.id)
        .unwrap();
    assert_eq!(stranded.node, "node_dead");
    Ok(())
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
        let existing_tasks = tokio_test::block_on(controller.list_tasks(session_id.clone()))?.len();
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

#[test]
fn test_dispatch_zero_session_limit_keeps_retained_executor_idle() -> Result<(), FlameError> {
    let (env, session_id, options) = instance_limit_env(10, Some(0))?;
    let controller = &env.controller;
    let executor =
        tokio_test::block_on(controller.create_executor("node_1".to_string(), session_id.clone()))?;
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

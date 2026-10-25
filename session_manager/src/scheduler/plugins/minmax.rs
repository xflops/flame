/*
Copyright 2026 The Flame Authors.
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

use std::collections::HashMap;

use common::apis::{ExecutorState, ResourceRequirement, SessionState};
use common::ctx::FlameLimits;
use common::FlameError;

use crate::model::{
    ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShot, ALL_APPLICATION, ALL_EXECUTOR,
};
use crate::scheduler::plugins::{batch_size, compute_demand, Plugin, PluginPtr, PluginsOptions};

#[derive(Default)]
pub struct MinMaxPlugin {
    // Configured executor limits for nodes, applications and sessions.
    limits: FlameLimits,
    // Node name -> reserved executor count, including Idle/departing executors.
    // Enforces the node executor limit.
    node_executors: HashMap<String, usize>,
    // Node name -> reserved CPU/memory/GPU totals. Prevents resource overcommit.
    node_resources: HashMap<String, ResourceRequirement>,
    // Application name -> max_instances, combined with the cluster application limit.
    application_limits: HashMap<String, u32>,
    // Application name -> reserved executor count. Enforces application limits.
    application_executors: HashMap<String, usize>,
    // Session ID -> allocated/pipelined executor count, including interrupted ones.
    // Used for readiness and hard session limits.
    session_executors: HashMap<String, usize>,
    // Session ID -> interrupted executor count. Subtracted from session_executors
    // to determine effective usage when checking whether a session is overused.
    session_interrupted: HashMap<String, usize>,
}

impl MinMaxPlugin {
    pub fn new_ptr(options: &PluginsOptions) -> PluginPtr {
        Box::new(Self {
            limits: options.limits.clone(),
            ..Default::default()
        })
    }
}

impl Plugin for MinMaxPlugin {
    fn name(&self) -> &'static str {
        "minmax"
    }

    fn setup(&mut self, snapshot: &SnapShot) -> Result<(), FlameError> {
        self.node_executors.clear();
        self.node_resources.clear();
        self.application_executors.clear();
        self.session_executors.clear();
        self.session_interrupted.clear();
        self.application_limits = snapshot
            .find_applications(ALL_APPLICATION)?
            .values()
            .map(|application| (application.name.clone(), application.max_instances))
            .collect();
        for executor in snapshot.find_executors(ALL_EXECUTOR)?.values() {
            // Retained and departing executors occupy capacity until Released.
            if executor.state == ExecutorState::Released {
                continue;
            }
            self.account_capacity(executor);
            if let Some(session) = &executor.ssn_id {
                *self.session_executors.entry(session.clone()).or_default() += 1;
                if executor.is_interrupted() {
                    *self.session_interrupted.entry(session.clone()).or_default() += 1;
                }
            }
        }
        Ok(())
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let instances = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        if executor_limit(ssn.max_instances, self.limits.session.max_executors)
            .is_some_and(|maximum| instances >= maximum as usize)
        {
            return Some(false);
        }
        // Below demand, including warmup without pending tasks, the session needs
        // more executors; `is_ready` and the configured policies bound how fast.
        (instances < compute_demand(ssn) as usize).then_some(true)
    }

    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let instances = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        if executor_limit(ssn.max_instances, self.limits.session.max_executors)
            .is_some_and(|maximum| instances >= maximum as usize)
        {
            return Some(true);
        }
        // Below the retained minimum, or while a gang is still short of its size.
        if instances < ssn.min_instances.max(batch_size(ssn)) as usize {
            return Some(false);
        }
        // Count existing and pipelined reservations before another allocation.
        // Below demand, let the configured policy apply fairness constraints.
        (instances >= compute_demand(ssn) as usize).then_some(true)
    }

    fn is_satisfied(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        // The gang cannot exceed what the session limit allows.
        let gang = batch_size(ssn);
        let minimum = executor_limit(Some(gang), self.limits.session.max_executors)
            .map_or(gang, |limit| limit.min(gang));
        let instances = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        Some(instances >= minimum as usize)
    }

    fn is_overused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let executors = self.session_executors.get(&ssn.id).copied().unwrap_or(0);
        let interrupted = self.session_interrupted.get(&ssn.id).copied().unwrap_or(0);
        let effective = executors.saturating_sub(interrupted);
        let demand = executor_limit(ssn.max_instances, self.limits.session.max_executors)
            .map_or(compute_demand(ssn), |maximum| {
                compute_demand(ssn).min(maximum)
            });
        (effective > demand as usize).then_some(true)
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        // Resource limits are admission constraints under every policy. DRF
        // controls fairness, but enabling it must not be required for safety.
        let used = self
            .node_resources
            .get(&node.name)
            .cloned()
            .unwrap_or_default();
        let request = ssn.resreq.clone().unwrap_or_default();
        if request.cpu > node.allocatable.cpu.saturating_sub(used.cpu)
            || request.memory > node.allocatable.memory.saturating_sub(used.memory)
            || request.gpu > node.allocatable.gpu.saturating_sub(used.gpu)
        {
            return Some(false);
        }
        let active = self.node_executors.get(&node.name).copied().unwrap_or(0);
        let allocatable = active < self.limits.node.max_executors as usize;
        if !allocatable {
            tracing::debug!(
                "Node <{}> reached max_executors limit: {} >= {}",
                node.name,
                active,
                self.limits.node.max_executors
            );
        }
        if !allocatable {
            return Some(false);
        }
        let Some(max_instances) = executor_limit(
            self.application_limits.get(&ssn.application).copied(),
            self.limits.application.max_executors,
        ) else {
            return Some(true);
        };
        let instances = self
            .application_executors
            .get(&ssn.application)
            .copied()
            .unwrap_or(0);
        Some(instances < max_instances as usize)
    }

    fn is_available(&self, executor: &ExecutorInfoPtr, _ssn: &SessionInfoPtr) -> Option<bool> {
        executor.is_interrupted().then_some(false)
    }

    fn is_reclaimable(&self, executor: &ExecutorInfoPtr) -> Option<bool> {
        executor.is_interrupted().then_some(false)
    }

    fn on_executor_create(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        self.account_capacity(&executor);
        *self.session_executors.entry(ssn.id.clone()).or_default() += 1;
    }

    fn on_executor_pipeline(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Physical capacity and any existing session owner were counted by setup.
        if executor.ssn_id.as_ref() != Some(&ssn.id) {
            *self.session_executors.entry(ssn.id.clone()).or_default() += 1;
        }
    }

    fn on_session_bind(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Dispatch binds retained Idle capacity; its node/application usage
        // is already counted. A reservation owned by this session is not new.
        if executor.ssn_id.as_ref() != Some(&ssn.id) {
            *self.session_executors.entry(ssn.id.clone()).or_default() += 1;
        }
    }

    fn on_executor_interrupt(&mut self, _executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Keep the reservation, but exclude the interrupted executor from overuse.
        *self.session_interrupted.entry(ssn.id.clone()).or_default() += 1;
    }

    fn on_session_unbind(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        let instances = self.session_executors.entry(ssn.id.clone()).or_default();
        *instances = instances.saturating_sub(1);
        if executor.is_interrupted() {
            let interrupted = self.session_interrupted.entry(ssn.id.clone()).or_default();
            *interrupted = interrupted.saturating_sub(1);
        }
    }
}

impl MinMaxPlugin {
    fn account_capacity(&mut self, executor: &ExecutorInfoPtr) {
        *self
            .node_executors
            .entry(executor.node.clone())
            .or_default() += 1;
        self.node_resources
            .entry(executor.node.clone())
            .or_default()
            .add(&executor.resreq);
        *self
            .application_executors
            .entry(executor.application.clone())
            .or_default() += 1;
    }
}

fn executor_limit(entity: Option<u32>, cluster: Option<u32>) -> Option<u32> {
    match (entity, cluster) {
        (Some(entity), Some(cluster)) => Some(entity.min(cluster)),
        (entity, cluster) => entity.or(cluster),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::{AppInfo, ExecutorInfo, NodeInfo, SessionInfo};
    use common::apis::TaskState;

    fn plugin(snapshot: &SnapShot) -> MinMaxPlugin {
        let mut plugin = MinMaxPlugin::default();
        plugin.setup(snapshot).unwrap();
        plugin
    }

    fn limited_session(max_instances: Option<u32>) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: "session".to_string(),
            application: "app".to_string(),
            max_instances,
            tasks_status: [(TaskState::Pending, 10)].into_iter().collect(),
            ..Default::default()
        })
    }

    #[test]
    fn gang_is_the_batch_size_while_tasks_are_pending_and_not_min_instances() {
        let snapshot = SnapShot::new();
        let mut plugin = plugin(&snapshot);
        let gang = |min_instances, batch_size, pending| {
            Arc::new(SessionInfo {
                min_instances,
                batch_size,
                tasks_status: [(TaskState::Pending, pending)].into_iter().collect(),
                ..limited_session(None).as_ref().clone()
            })
        };

        // A minimum is a floor of long-lived instances, not a gang: one executor satisfies it.
        let minimum = gang(3, 1, 1);
        plugin.on_session_bind(executor("a", "node", ExecutorState::Bound), minimum.clone());
        assert_eq!(plugin.is_satisfied(&minimum), Some(true));

        // A gang needs every member while tasks wait ...
        let batch = gang(0, 3, 1);
        assert_eq!(plugin.is_satisfied(&batch), Some(false));
        plugin.on_session_bind(executor("b", "node", ExecutorState::Bound), batch.clone());
        assert_eq!(plugin.is_satisfied(&batch), Some(false));
        plugin.on_session_bind(executor("c", "node", ExecutorState::Bound), batch.clone());
        assert_eq!(plugin.is_satisfied(&batch), Some(true));

        // ... and demands them, but only while tasks wait.
        assert_eq!(compute_demand(&gang(0, 4, 1)), 4);
        assert_eq!(compute_demand(&gang(0, 4, 0)), 0);
        assert_eq!(compute_demand(&gang(2, 4, 0)), 2);
        assert_eq!(compute_demand(&gang(0, 1, 3)), 3);
    }

    #[test]
    fn only_open_sessions_demand_executors() {
        let snapshot = SnapShot::new();
        let mut plugin = plugin(&snapshot);
        let open = Arc::new(SessionInfo {
            min_instances: 1,
            ..limited_session(None).as_ref().clone()
        });
        assert_eq!(compute_demand(&open), 10);
        assert_eq!(plugin.is_underused(&open), Some(true));

        for state in [
            SessionState::Closed,
            SessionState::Suspending,
            SessionState::Suspended,
        ] {
            let ssn = Arc::new(SessionInfo {
                state,
                ..open.as_ref().clone()
            });
            assert_eq!(compute_demand(&ssn), 0, "{state}");
            assert_eq!(plugin.is_underused(&ssn), None, "{state}");
            plugin.on_session_bind(executor("one", "node", ExecutorState::Bound), ssn.clone());
            assert_eq!(plugin.is_overused(&ssn), Some(true), "{state}");
            plugin.on_session_unbind(executor("one", "node", ExecutorState::Bound), ssn);
        }
    }

    #[test]
    fn overused_preserves_demand_and_applies_effective_maximum() {
        let snapshot = SnapShot::new();
        let mut plugin = plugin(&snapshot);
        let session = Arc::new(SessionInfo {
            min_instances: 1,
            tasks_status: Default::default(),
            ..limited_session(None).as_ref().clone()
        });
        for id in ["one", "two", "three"] {
            plugin.on_session_bind(executor(id, "node", ExecutorState::Bound), session.clone());
        }
        assert_eq!(plugin.is_overused(&session), Some(true));
        for task_state in [TaskState::Pending, TaskState::Running] {
            let busy = Arc::new(SessionInfo {
                tasks_status: [(task_state, 3)].into_iter().collect(),
                ..session.as_ref().clone()
            });
            assert_eq!(plugin.is_overused(&busy), None);
            let bounded = Arc::new(SessionInfo {
                max_instances: Some(2),
                ..busy.as_ref().clone()
            });
            assert_eq!(plugin.is_overused(&bounded), Some(true));
        }
        plugin.on_session_unbind(
            executor("one", "node", ExecutorState::Bound),
            session.clone(),
        );
        plugin.on_session_unbind(
            executor("two", "node", ExecutorState::Bound),
            session.clone(),
        );
        assert_eq!(plugin.is_overused(&session), None);
        plugin.limits.session.max_executors = Some(0);
        assert_eq!(plugin.is_overused(&session), Some(true));
    }

    #[test]
    fn overused_excludes_departures_across_scheduling_cycles() {
        let snapshot = SnapShot::new();
        let session = Arc::new(SessionInfo {
            min_instances: 1,
            tasks_status: Default::default(),
            ..limited_session(None).as_ref().clone()
        });
        for (id, state) in [
            ("binding", ExecutorState::Binding),
            ("bound", ExecutorState::Bound),
            ("departing", ExecutorState::Unbinding),
        ] {
            snapshot
                .add_executor(Arc::new(ExecutorInfo {
                    signal: (id == "departing").then_some(common::apis::Signal::Interrupt),
                    ssn_id: Some(session.id.clone()),
                    application: session.application.clone(),
                    ..executor(id, "node", state).as_ref().clone()
                }))
                .unwrap();
        }
        let mut plugin = plugin(&snapshot);
        assert_eq!(plugin.is_overused(&session), Some(true));
        let bound = snapshot.find_executors(ALL_EXECUTOR).unwrap()["bound"].clone();
        plugin.on_executor_interrupt(bound.clone(), session.clone());
        assert_eq!(plugin.is_overused(&session), None);
        snapshot
            .update_executor(Arc::new(ExecutorInfo {
                signal: Some(common::apis::Signal::Interrupt),
                ..bound.as_ref().clone()
            }))
            .unwrap();
        plugin.setup(&snapshot).unwrap();
        assert_eq!(plugin.is_overused(&session), None);
        assert_eq!(plugin.node_executors["node"], 3);
        assert_eq!(plugin.application_executors["app"], 3);
        assert_eq!(plugin.session_executors[&session.id], 3);
        assert_eq!(plugin.session_interrupted[&session.id], 2);
    }

    #[test]
    fn interrupted_reservations_preserve_capacity_and_session_ceiling() {
        use crate::scheduler::plugins::{PluginManager, PluginsOptions};
        for request in [
            ResourceRequirement {
                cpu: 1,
                ..Default::default()
            },
            ResourceRequirement {
                memory: 1024,
                ..Default::default()
            },
            ResourceRequirement {
                gpu: 1,
                ..Default::default()
            },
        ] {
            let snapshot = SnapShot::new();
            let session = Arc::new(SessionInfo {
                resreq: Some(request.clone()),
                ..limited_session(Some(1)).as_ref().clone()
            });
            snapshot.add_session(session.clone()).unwrap();
            let node = Arc::new(NodeInfo {
                name: "node".into(),
                allocatable: request.clone(),
                state: common::apis::NodeState::Ready,
            });
            snapshot.add_node(node.clone()).unwrap();
            let retained = Arc::new(ExecutorInfo {
                ssn_id: Some(session.id.clone()),
                application: session.application.clone(),
                resreq: request,
                ..executor("retained", "node", ExecutorState::Bound)
                    .as_ref()
                    .clone()
            });
            snapshot.add_executor(retained.clone()).unwrap();
            let mut policy = plugin(&snapshot);
            policy.on_executor_interrupt(retained.clone(), session.clone());
            assert_eq!(policy.session_executors[&session.id], 1);
            assert_eq!(policy.is_overused(&session), None);
            assert_eq!(policy.is_ready(&session), Some(true));
            assert_eq!(policy.is_underused(&session), Some(false));
            assert_eq!(policy.is_allocatable(&node, &session), Some(false));
            snapshot
                .update_executor(Arc::new(ExecutorInfo {
                    signal: Some(common::apis::Signal::Interrupt),
                    ..retained.as_ref().clone()
                }))
                .unwrap();
            policy.setup(&snapshot).unwrap();
            assert_eq!(policy.session_executors[&session.id], 1);
            assert_eq!(policy.is_allocatable(&node, &session), Some(false));
            assert_eq!(policy.is_ready(&session), Some(true));
            let manager = PluginManager::setup(
                &snapshot,
                &PluginsOptions {
                    policies: vec!["drf".into()],
                    ..Default::default()
                },
            )
            .unwrap();
            let interrupted = snapshot.find_executors(ALL_EXECUTOR).unwrap()[&retained.id].clone();
            assert!(!manager.is_available(&interrupted, &session).unwrap());
            assert!(!manager.is_reclaimable(&interrupted).unwrap());
            assert!(!manager.is_allocatable(&node, &session).unwrap());
            assert!(!manager.is_overused(&session).unwrap());
            assert_eq!(policy.node_executors["node"], 1);
            assert_eq!(policy.application_executors["app"], 1);
        }
    }

    #[test]
    fn resource_capacity_counts_retained_and_inflight_executors() {
        for request in [
            ResourceRequirement {
                cpu: 1,
                ..Default::default()
            },
            ResourceRequirement {
                memory: 1024,
                ..Default::default()
            },
            ResourceRequirement {
                gpu: 1,
                ..Default::default()
            },
        ] {
            for state in [
                ExecutorState::Void,
                ExecutorState::Binding,
                ExecutorState::Idle,
                ExecutorState::Bound,
                ExecutorState::Unbinding,
                ExecutorState::Releasing,
            ] {
                let snapshot = SnapShot::new();
                let retained = Arc::new(ExecutorInfo {
                    resreq: request.clone(),
                    ..executor("retained", "node", state).as_ref().clone()
                });
                snapshot.add_executor(retained.clone()).unwrap();
                let node = Arc::new(NodeInfo {
                    name: "node".into(),
                    allocatable: ResourceRequirement {
                        cpu: request.cpu * 2,
                        memory: request.memory * 2,
                        gpu: request.gpu * 2,
                    },
                    ..Default::default()
                });
                let session = Arc::new(SessionInfo {
                    resreq: Some(request.clone()),
                    ..limited_session(None).as_ref().clone()
                });
                let mut plugin = plugin(&snapshot);
                plugin.limits.node.max_executors = 10;
                assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
                // Reserving retained capacity does not charge resources twice.
                plugin.on_executor_pipeline(retained, session.clone());
                assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
                let new = Arc::new(ExecutorInfo {
                    resreq: request.clone(),
                    ..executor("new", "node", ExecutorState::Void)
                        .as_ref()
                        .clone()
                });
                plugin.on_executor_create(new, session.clone());
                assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
                // Only Released executors stop consuming capacity at next setup.
                let released = Arc::new(ExecutorInfo {
                    state: ExecutorState::Released,
                    resreq: request.clone(),
                    ..executor("retained", "node", state).as_ref().clone()
                });
                snapshot
                    .update_executor_state(released, ExecutorState::Released)
                    .unwrap();
                plugin.setup(&snapshot).unwrap();
                assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
            }
        }
    }

    #[test]
    fn session_limit_defers_below_cap_and_blocks_zero() {
        let snapshot = SnapShot::new();
        let plugin = plugin(&snapshot);
        assert_eq!(plugin.is_ready(&limited_session(None)), None);
        assert_eq!(plugin.is_ready(&limited_session(Some(1))), None);
        assert_eq!(plugin.is_ready(&limited_session(Some(0))), Some(true));
    }

    #[test]
    fn task_demand_counts_running_binding_and_pipeline_reservations() {
        let mut plugin = plugin(&SnapShot::new());
        let session = Arc::new(SessionInfo {
            tasks_status: [(TaskState::Pending, 1), (TaskState::Running, 2)]
                .into_iter()
                .collect(),
            ..limited_session(None).as_ref().clone()
        });
        for index in 0..3 {
            assert_eq!(plugin.is_ready(&session), None);
            let reserved = executor(&format!("executor-{index}"), "node", ExecutorState::Binding);
            plugin.on_executor_create(reserved.clone(), session.clone());
            let binding = Arc::new(ExecutorInfo {
                ssn_id: Some(session.id.clone()),
                ..reserved.as_ref().clone()
            });
            plugin.on_session_bind(binding, session.clone());
            assert_eq!(plugin.session_executors[&session.id], index + 1);
        }
        assert_eq!(plugin.is_ready(&session), Some(true));
    }

    #[test]
    fn minimum_counts_pipeline_bindings_and_defers_when_satisfied() {
        let mut plugin = plugin(&SnapShot::new());
        let session = Arc::new(SessionInfo {
            min_instances: 2,
            tasks_status: Default::default(),
            ..limited_session(None).as_ref().clone()
        });
        assert_eq!(plugin.is_underused(&session), Some(true));
        assert_eq!(plugin.is_ready(&session), Some(false));
        let first = executor("first", "node", ExecutorState::Void);
        plugin.on_executor_create(first, session.clone());
        assert_eq!(plugin.is_ready(&session), Some(false));
        plugin.on_session_bind(
            executor("second", "node", ExecutorState::Binding),
            session.clone(),
        );
        assert_eq!(plugin.is_underused(&session), None);
        assert_eq!(plugin.is_ready(&session), Some(true));
        plugin.on_session_unbind(
            executor("second", "node", ExecutorState::Bound),
            session.clone(),
        );
        assert_eq!(plugin.is_underused(&session), Some(true));
        assert_eq!(plugin.is_ready(&session), Some(false));
    }

    #[test]
    fn underused_while_below_task_demand() {
        let mut plugin = plugin(&SnapShot::new());
        let session = Arc::new(SessionInfo {
            tasks_status: [(TaskState::Pending, 2)].into_iter().collect(),
            ..limited_session(None).as_ref().clone()
        });
        assert_eq!(plugin.is_underused(&session), Some(true));
        for index in 0..2 {
            plugin.on_session_bind(
                executor(&format!("executor-{index}"), "node", ExecutorState::Binding),
                session.clone(),
            );
        }
        assert_eq!(plugin.is_underused(&session), None);
    }

    #[test]
    fn session_ceiling_takes_precedence_over_minimum() {
        let session = Arc::new(SessionInfo {
            min_instances: 3,
            ..limited_session(Some(1)).as_ref().clone()
        });
        let mut plugin = plugin(&SnapShot::new());
        plugin.on_executor_pipeline(
            executor("first", "node", ExecutorState::Void),
            session.clone(),
        );
        assert_eq!(plugin.is_ready(&session), Some(true));
        assert_eq!(plugin.is_underused(&session), Some(false));
        plugin.limits.session.max_executors = Some(0);
        assert_eq!(plugin.is_ready(&session), Some(true));
        assert_eq!(plugin.is_underused(&session), Some(false));
    }

    #[tokio::test]
    async fn warmup_without_pending_tasks_works_with_each_demand_policy() {
        use crate::scheduler::actions::{AllocateAction, DispatchAction};
        use crate::scheduler::Context;
        use common::apis::{
            ApplicationAttributes, Node, NodeState, ResourceRequirement, SessionAttributes,
        };
        use common::ctx::{FlameCluster, FlameClusterContext};

        for policies in [
            vec!["drf", "das"],
            vec!["priority", "drf", "das"],
            vec!["priority", "das"],
        ] {
            for cluster_ceiling in [None, Some(2)] {
                let database = common::temp_db_path("minmax_warmup");
                let config = FlameClusterContext {
                    cluster: FlameCluster {
                        storage: format!("sqlite:///{database}"),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let storage = crate::storage::new_ptr(&config).await.unwrap();
                let controller = crate::controller::new_ptr(storage);
                controller
                    .register_application("app".into(), ApplicationAttributes::default())
                    .await
                    .unwrap();
                let capacity = ResourceRequirement {
                    cpu: 16,
                    memory: 16384,
                    gpu: 0,
                };
                controller
                    .storage()
                    .register_node(&Node {
                        name: "node".into(),
                        capacity: capacity.clone(),
                        allocatable: capacity,
                        state: NodeState::Ready,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                let session = controller
                    .create_session(SessionAttributes {
                        id: "warmup".into(),
                        application: "app".into(),
                        min_instances: 3,
                        resreq: Some(ResourceRequirement {
                            cpu: 1,
                            memory: 1024,
                            gpu: 0,
                        }),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                let mut options = PluginsOptions {
                    policies: policies.iter().map(|policy| policy.to_string()).collect(),
                    ..Default::default()
                };
                options.limits.session.max_executors = cluster_ceiling;
                let expected = cluster_ceiling.unwrap_or(3) as usize;
                for cycle in 0..8 {
                    let mut context = Context::new(controller.clone(), &options).unwrap();
                    DispatchAction::new_ptr()
                        .execute(&mut context)
                        .await
                        .unwrap();
                    AllocateAction::new_ptr()
                        .execute(&mut context)
                        .await
                        .unwrap();
                    let executors = controller.list_executors().unwrap();
                    assert!(
                        executors.len() <= expected,
                        "cycle {cycle}, policies {policies:?}"
                    );
                    for executor in executors {
                        if executor.state == ExecutorState::Void {
                            controller.register_executor(&executor).await.unwrap();
                        }
                    }
                }
                let executors = controller.list_executors().unwrap();
                assert_eq!(executors.len(), expected, "policies {policies:?}");
                assert!(executors
                    .iter()
                    .all(|executor| executor.state == ExecutorState::Binding
                        && executor.ssn_id.as_ref() == Some(&session.id)));
                assert!(controller.list_tasks(session.id).unwrap().is_empty());
                drop(controller);
                let _ = std::fs::remove_file(database);
            }
        }
    }

    #[test]
    fn cluster_session_ceiling_preserves_tighter_entity_limits() {
        for (entity_limit, effective_limit) in [(None, 2), (Some(10), 2), (Some(1), 1)] {
            let mut plugin = plugin(&SnapShot::new());
            plugin.limits.session.max_executors = Some(2);
            let session = limited_session(entity_limit);
            for index in 0..effective_limit {
                assert_eq!(plugin.is_ready(&session), None);
                plugin.on_session_bind(
                    executor(&format!("executor-{index}"), "node", ExecutorState::Idle),
                    session.clone(),
                );
            }
            assert_eq!(plugin.is_ready(&session), Some(true));
        }
        let mut plugin = plugin(&SnapShot::new());
        plugin.limits.session.max_executors = Some(0);
        assert_eq!(plugin.is_ready(&limited_session(None)), Some(true));
    }

    #[test]
    fn cluster_application_ceiling_preserves_tighter_entity_limits() {
        for (entity_limit, effective_limit) in [(None, 2), (Some(10), 2), (Some(1), 1)] {
            let snapshot = SnapShot::new();
            if let Some(max_instances) = entity_limit {
                snapshot
                    .add_application(Arc::new(AppInfo {
                        name: "app".to_string(),
                        max_instances,
                        ..Default::default()
                    }))
                    .unwrap();
            }
            let mut plugin = plugin(&snapshot);
            plugin.limits.application.max_executors = Some(2);
            let session = limited_session(None);
            let node = Arc::new(NodeInfo::default());
            for index in 0..effective_limit {
                assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
                plugin.on_executor_create(
                    Arc::new(ExecutorInfo {
                        id: format!("executor-{index}"),
                        application: "app".to_string(),
                        state: ExecutorState::Void,
                        ..Default::default()
                    }),
                    session.clone(),
                );
            }
            assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
        }
        let mut plugin = plugin(&SnapShot::new());
        plugin.limits.application.max_executors = Some(0);
        assert_eq!(
            plugin.is_allocatable(&Arc::new(NodeInfo::default()), &limited_session(None)),
            Some(false)
        );
    }

    #[test]
    fn session_limit_counts_live_owners_from_setup() {
        for state in [
            ExecutorState::Binding,
            ExecutorState::Bound,
            ExecutorState::Released,
        ] {
            let snapshot = SnapShot::new();
            let session = limited_session(Some(1));
            snapshot
                .add_executor(Arc::new(ExecutorInfo {
                    id: "owned".to_string(),
                    state,
                    ssn_id: Some(session.id.clone()),
                    ..Default::default()
                }))
                .unwrap();
            snapshot
                .add_executor(Arc::new(ExecutorInfo {
                    id: "other-session".to_string(),
                    state: ExecutorState::Bound,
                    ssn_id: Some("other".to_string()),
                    ..Default::default()
                }))
                .unwrap();
            let plugin = plugin(&snapshot);
            assert_eq!(
                plugin.is_ready(&session),
                (state != ExecutorState::Released).then_some(true)
            );
        }
    }

    #[test]
    fn binding_callbacks_update_session_limit() {
        let snapshot = SnapShot::new();
        let idle = executor("bound", "node", ExecutorState::Idle);
        snapshot.add_executor(idle.clone()).unwrap();
        let mut plugin = plugin(&snapshot);
        let session = limited_session(Some(1));
        assert_eq!(plugin.is_ready(&session), None);
        plugin.on_session_bind(idle.clone(), session.clone());
        assert_eq!(plugin.is_ready(&session), Some(true));
        assert_eq!(plugin.session_executors[&session.id], 1);
        assert_eq!(
            plugin
                .session_interrupted
                .get(&session.id)
                .copied()
                .unwrap_or(0),
            0
        );
        let bound = Arc::new(ExecutorInfo {
            state: ExecutorState::Bound,
            ssn_id: Some(session.id.clone()),
            ..idle.as_ref().clone()
        });
        plugin.on_session_unbind(bound, session.clone());
        assert_eq!(plugin.is_ready(&session), None);
        assert_eq!(plugin.session_executors[&session.id], 0);
        assert_eq!(
            plugin
                .session_interrupted
                .get(&session.id)
                .copied()
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn pipeline_callbacks_count_reserved_executors_as_active() {
        let mut plugin = plugin(&SnapShot::new());
        let session = limited_session(Some(2));
        let first = executor("first", "node", ExecutorState::Void);
        plugin.on_executor_create(first, session.clone());
        assert_eq!(plugin.is_ready(&session), None);
        plugin.on_executor_pipeline(
            executor("second", "node", ExecutorState::Unbinding),
            session.clone(),
        );
        assert_eq!(plugin.is_ready(&session), Some(true));
    }

    #[test]
    fn application_limit_counts_startup_retention_and_release_across_sessions() {
        let session = limited_session(None);
        let node = Arc::new(NodeInfo {
            name: "new-node".to_string(),
            ..Default::default()
        });
        for state in [
            ExecutorState::Void,
            ExecutorState::Binding,
            ExecutorState::Bound,
            ExecutorState::Idle,
            ExecutorState::Releasing,
            ExecutorState::Released,
        ] {
            let snapshot = SnapShot::new();
            snapshot
                .add_application(Arc::new(AppInfo {
                    name: "app".to_string(),
                    max_instances: 1,
                    ..Default::default()
                }))
                .unwrap();
            let executor = Arc::new(ExecutorInfo {
                id: "retained".to_string(),
                node: "other-node".to_string(),
                application: "app".to_string(),
                ssn_id: Some("other-session".to_string()),
                state,
                ..Default::default()
            });
            snapshot.add_executor(executor.clone()).unwrap();
            let plugin = plugin(&snapshot);
            assert_eq!(
                plugin.is_allocatable(&node, &session),
                Some(state == ExecutorState::Released)
            );
            // The cap prohibits creation; retained executor reuse remains
            // governed by compatibility and the session's own limit.
            assert_eq!(plugin.is_available(&executor, &session), None);
        }
    }

    #[test]
    fn zero_application_limit_blocks_creation_without_blocking_reuse() {
        let snapshot = SnapShot::new();
        snapshot
            .add_application(Arc::new(AppInfo {
                name: "app".to_string(),
                max_instances: 0,
                ..Default::default()
            }))
            .unwrap();
        let plugin = plugin(&snapshot);
        let session = limited_session(None);
        let node = Arc::new(NodeInfo::default());
        assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
        assert_eq!(plugin.is_ready(&session), None);
        assert_eq!(
            plugin.is_available(&executor("idle", "node", ExecutorState::Idle), &session),
            None
        );
    }

    #[test]
    fn pipeline_callbacks_count_application_instances_once_and_binding_reuses_them() {
        let snapshot = SnapShot::new();
        snapshot
            .add_application(Arc::new(AppInfo {
                name: "app".to_string(),
                max_instances: 2,
                ..Default::default()
            }))
            .unwrap();
        let retained = Arc::new(ExecutorInfo {
            id: "retained".to_string(),
            application: "app".to_string(),
            state: ExecutorState::Idle,
            ..Default::default()
        });
        snapshot.add_executor(retained.clone()).unwrap();
        let mut plugin = plugin(&snapshot);
        let session = limited_session(Some(2));
        let node = Arc::new(NodeInfo::default());
        assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
        plugin.on_session_bind(retained.clone(), session.clone());
        assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
        plugin.on_session_unbind(
            Arc::new(ExecutorInfo {
                state: ExecutorState::Bound,
                ssn_id: Some(session.id.clone()),
                ..retained.as_ref().clone()
            }),
            session.clone(),
        );
        let new = Arc::new(ExecutorInfo {
            id: "new".to_string(),
            application: "app".to_string(),
            state: ExecutorState::Void,
            ..Default::default()
        });
        plugin.on_executor_create(new, session.clone());
        assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
        assert_eq!(plugin.is_ready(&session), None);
        // Pipelining an existing instance changes its reservation, not the
        // number of application processes occupying capacity.
        plugin.on_executor_pipeline(retained, session.clone());
        assert_eq!(plugin.application_executors["app"], 2);
        assert_eq!(plugin.is_ready(&session), Some(true));
    }

    fn executor(id: &str, node: &str, state: ExecutorState) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            node: node.to_string(),
            state,
            ..Default::default()
        })
    }

    #[test]
    fn enforces_each_nodes_limit_and_ignores_released_records() {
        let snapshot = SnapShot::new();
        snapshot
            .add_executor(executor("active-a", "node-a", ExecutorState::Idle))
            .unwrap();
        snapshot
            .add_executor(executor("released-a", "node-a", ExecutorState::Released))
            .unwrap();

        let mut plugin = MinMaxPlugin {
            limits: FlameLimits {
                node: common::ctx::FlameNodeLimits { max_executors: 1 },
                ..Default::default()
            },
            ..Default::default()
        };
        plugin.setup(&snapshot).unwrap();

        let session = Arc::new(SessionInfo::default());
        let node_a = Arc::new(NodeInfo {
            name: "node-a".to_string(),
            ..Default::default()
        });
        let node_b = Arc::new(NodeInfo {
            name: "node-b".to_string(),
            ..Default::default()
        });

        assert_eq!(plugin.is_allocatable(&node_a, &session), Some(false));
        assert_eq!(plugin.is_allocatable(&node_b, &session), Some(true));
    }

    #[test]
    fn creation_and_pipeline_charge_distinct_capacity_and_session_reservations() {
        let snapshot = SnapShot::new();
        let retained = executor("retained", "other-node", ExecutorState::Void);
        snapshot.add_executor(retained.clone()).unwrap();
        let mut plugin = MinMaxPlugin {
            limits: FlameLimits {
                node: common::ctx::FlameNodeLimits { max_executors: 1 },
                ..Default::default()
            },
            ..Default::default()
        };
        plugin.setup(&snapshot).unwrap();

        let session = Arc::new(SessionInfo::default());
        let node = Arc::new(NodeInfo {
            name: "node".to_string(),
            ..Default::default()
        });
        let created = executor("new", "node", ExecutorState::Void);

        assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
        plugin.on_executor_create(created, session.clone());
        assert_eq!(plugin.node_executors["node"], 1);
        assert_eq!(plugin.session_executors[&session.id], 1);
        plugin.on_executor_pipeline(retained, session.clone());
        assert_eq!(plugin.node_executors["other-node"], 1);
        assert_eq!(plugin.node_executors["node"], 1);
        assert_eq!(plugin.session_executors[&session.id], 2);
        assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
    }
}

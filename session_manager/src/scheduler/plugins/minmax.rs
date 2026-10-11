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

use std::collections::{HashMap, HashSet};

use common::apis::ExecutorState;
use common::ctx::FlameLimits;
use common::FlameError;

use crate::model::{
    ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShot, ALL_APPLICATION, ALL_EXECUTOR,
};
use crate::scheduler::plugins::{Plugin, PluginPtr, PluginsOptions};

#[derive(Default)]
pub struct MinMaxPlugin {
    limits: FlameLimits,
    active_executors: HashMap<String, HashSet<String>>,
    application_limits: HashMap<String, u32>,
    application_executors: HashMap<String, HashSet<String>>,
    session_executors: HashMap<String, HashSet<String>>,
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
        self.active_executors.clear();
        self.application_executors.clear();
        self.session_executors.clear();
        self.application_limits = snapshot
            .find_applications(ALL_APPLICATION)?
            .values()
            .map(|application| (application.name.clone(), application.max_instances))
            .collect();
        for executor in snapshot.find_executors(ALL_EXECUTOR)?.values() {
            // Retained Idle and Releasing executors still occupy application
            // and node capacity. The next setup removes them once Released.
            if executor.state != ExecutorState::Released {
                self.active_executors
                    .entry(executor.node.clone())
                    .or_default()
                    .insert(executor.id.clone());
                self.application_executors
                    .entry(executor.application.clone())
                    .or_default()
                    .insert(executor.id.clone());
                if let Some(session) = &executor.ssn_id {
                    self.session_executors
                        .entry(session.clone())
                        .or_default()
                        .insert(executor.id.clone());
                }
            }
        }
        Ok(())
    }

    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let max_instances = executor_limit(ssn.max_instances, self.limits.session.max_executors)?;
        let instances = self.session_executors.get(&ssn.id).map_or(0, HashSet::len);
        // Below the hard ceiling, the demand policy still decides readiness.
        (instances >= max_instances as usize).then_some(true)
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        let active = self
            .active_executors
            .get(&node.name)
            .map_or(0, HashSet::len);
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
            .map_or(0, HashSet::len);
        Some(instances < max_instances as usize)
    }

    fn on_executor_pipeline(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        self.on_session_bind(executor, ssn);
    }

    fn on_session_bind(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        self.active_executors
            .entry(executor.node.clone())
            .or_default()
            .insert(executor.id.clone());
        self.application_executors
            .entry(executor.application.clone())
            .or_default()
            .insert(executor.id.clone());
        self.session_executors
            .entry(ssn.id.clone())
            .or_default()
            .insert(executor.id.clone());
    }

    fn on_session_unbind(&mut self, executor: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        // Unbinding ends session ownership but keeps the executor and its
        // application service alive. Keep application and node counts until
        // the executor is Released and excluded by the next setup.
        if let Some(executors) = self.session_executors.get_mut(&ssn.id) {
            executors.remove(&executor.id);
        }
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
            ..Default::default()
        })
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
                plugin.on_executor_pipeline(
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
        let mut plugin = plugin(&SnapShot::new());
        let session = limited_session(Some(1));
        let bound = executor("bound", "node", ExecutorState::Idle);
        assert_eq!(plugin.is_ready(&session), None);
        plugin.on_session_bind(bound.clone(), session.clone());
        plugin.on_session_bind(bound.clone(), session.clone());
        assert_eq!(plugin.is_ready(&session), Some(true));
        assert_eq!(plugin.session_executors[&session.id].len(), 1);
        plugin.on_session_unbind(
            executor("other", "node", ExecutorState::Idle),
            session.clone(),
        );
        assert_eq!(plugin.is_ready(&session), Some(true));
        plugin.on_session_unbind(bound.clone(), session.clone());
        assert_eq!(plugin.is_ready(&session), None);
        plugin.on_session_unbind(bound, session.clone());
        assert_eq!(plugin.is_ready(&session), None);
    }

    #[test]
    fn pipeline_callbacks_count_reserved_executors_as_active() {
        let mut plugin = plugin(&SnapShot::new());
        let session = limited_session(Some(2));
        let first = executor("first", "node", ExecutorState::Void);
        plugin.on_executor_pipeline(first.clone(), session.clone());
        plugin.on_session_bind(first, session.clone());
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
        plugin.on_session_unbind(retained.clone(), session.clone());
        let new = Arc::new(ExecutorInfo {
            id: "new".to_string(),
            application: "app".to_string(),
            state: ExecutorState::Void,
            ..Default::default()
        });
        plugin.on_executor_pipeline(new, session.clone());
        assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
        assert_eq!(plugin.is_ready(&session), None);
        // Pipelining an existing instance changes its reservation, not the
        // number of application processes occupying capacity.
        plugin.on_executor_pipeline(retained, session.clone());
        assert_eq!(plugin.application_executors["app"].len(), 2);
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
    fn pipelined_executors_are_counted_once_within_the_cycle() {
        let snapshot = SnapShot::new();
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
        let executor = executor("new", "node", ExecutorState::Void);

        assert_eq!(plugin.is_allocatable(&node, &session), Some(true));
        plugin.on_executor_pipeline(executor.clone(), session.clone());
        plugin.on_executor_pipeline(executor, session.clone());
        assert_eq!(plugin.is_allocatable(&node, &session), Some(false));
    }
}

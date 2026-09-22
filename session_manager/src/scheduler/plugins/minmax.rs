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
use common::FlameError;

use crate::model::{ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShot, ALL_EXECUTOR};
use crate::scheduler::plugins::{Plugin, PluginPtr, PluginsOptions};

pub struct MinMaxPlugin {
    max_executors: u32,
    active_executors: HashMap<String, HashSet<String>>,
}

impl MinMaxPlugin {
    pub fn new_ptr(options: &PluginsOptions) -> PluginPtr {
        Box::new(Self {
            max_executors: options.max_executors,
            active_executors: HashMap::new(),
        })
    }
}

impl Plugin for MinMaxPlugin {
    fn name(&self) -> &'static str {
        "minmax"
    }

    fn setup(&mut self, snapshot: &SnapShot) -> Result<(), FlameError> {
        self.active_executors.clear();
        for executor in snapshot.find_executors(ALL_EXECUTOR)?.values() {
            if executor.state != ExecutorState::Released {
                self.active_executors
                    .entry(executor.node.clone())
                    .or_default()
                    .insert(executor.id.clone());
            }
        }
        Ok(())
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, _ssn: &SessionInfoPtr) -> Option<bool> {
        let active = self
            .active_executors
            .get(&node.name)
            .map_or(0, HashSet::len);
        let allocatable = active < self.max_executors as usize;
        if !allocatable {
            tracing::debug!(
                "Node <{}> reached max_executors limit: {} >= {}",
                node.name,
                active,
                self.max_executors
            );
        }
        Some(allocatable)
    }

    fn on_executor_pipeline(&mut self, executor: ExecutorInfoPtr, _ssn: SessionInfoPtr) {
        self.active_executors
            .entry(executor.node.clone())
            .or_default()
            .insert(executor.id.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::{ExecutorInfo, NodeInfo, SessionInfo};

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
            max_executors: 1,
            active_executors: HashMap::new(),
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
            max_executors: 1,
            active_executors: HashMap::new(),
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

/*
Copyright 2025 The Flame Authors.
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

use common::apis::SessionID;
use common::FlameError;

use crate::model::{ExecutorInfoPtr, NodeInfoPtr, SessionInfoPtr, SnapShot};
use crate::scheduler::plugins::{Plugin, PluginPtr};

struct GangState {
    batch_size: u32,
    allocated: u32,
    pipelined: u32,
    bound: u32,
}

pub struct GangPlugin {
    ssn_state: HashMap<SessionID, GangState>,
}

impl GangPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(GangPlugin {
            ssn_state: HashMap::new(),
        })
    }
}

impl Plugin for GangPlugin {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_state.clear();

        {
            let sessions = ss
                .sessions
                .lock()
                .map_err(|e| FlameError::Internal(format!("failed to lock sessions: {}", e)))?;

            for ssn in sessions.values() {
                self.ssn_state.insert(
                    ssn.id.clone(),
                    GangState {
                        batch_size: ssn.batch_size.max(1),
                        allocated: 0,
                        pipelined: 0,
                        bound: 0,
                    },
                );
            }
        }

        let executors = ss.find_executors(None)?;
        for exec in executors.values() {
            if let Some(ssn_id) = &exec.ssn_id {
                if let Some(state) = self.ssn_state.get_mut(ssn_id) {
                    state.allocated += 1;
                }
            }
        }

        Ok(())
    }

    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let state = self.ssn_state.get(&ssn.id)?;
        let total = state.allocated + state.pipelined;
        Some(state.pipelined > 0 && total % state.batch_size == 0)
    }

    fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let state = self.ssn_state.get(&ssn.id)?;
        let total = state.allocated + state.bound;
        Some(state.bound > 0 && total % state.batch_size == 0)
    }

    fn on_executor_allocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined += 1;
        }
    }

    fn on_executor_unallocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined = state.pipelined.saturating_sub(1);
        }
    }

    fn on_executor_pipeline(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined += 1;
        }
    }

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.bound += 1;
        }
    }

    fn on_executor_discard(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined = state.pipelined.saturating_sub(1);
        }
    }

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.bound = state.bound.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExecutorInfo, NodeInfo, SessionInfo};
    use chrono::Utc;
    use common::apis::{ExecutorState, ResourceRequirement, SessionState, Shim, TaskState};
    use std::sync::Arc;

    fn create_test_session(id: &str, batch_size: u32) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            slots: 1,
            tasks_status: HashMap::from([(TaskState::Pending, 1)]),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            batch_size,
        })
    }

    fn create_test_executor(id: &str, ssn_id: Option<&str>) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
            },
            slots: 1,
            shim: Shim::Host,
            task_id: None,
            ssn_id: ssn_id.map(|s| s.to_string()),
            creation_time: Utc::now(),
            state: ExecutorState::Idle,
        })
    }

    fn create_test_node(name: &str) -> NodeInfoPtr {
        Arc::new(NodeInfo {
            name: name.to_string(),
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
            },
            state: common::apis::NodeState::Ready,
        })
    }

    #[test]
    fn test_is_fulfilled_batch_size_1() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_fulfilled(&ssn).unwrap());

        let node = create_test_node("node-1");
        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn).unwrap());
    }

    #[test]
    fn test_is_fulfilled_batch_size_2() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_fulfilled(&ssn).unwrap());

        let node = create_test_node("node-1");
        plugin.on_session_bind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn).unwrap());

        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn).unwrap());
    }

    #[test]
    fn test_is_fulfilled_with_allocated() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let exec = create_test_executor("exec-1", Some("ssn-1"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_fulfilled(&ssn).unwrap());

        let node = create_test_node("node-1");
        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn).unwrap());
    }

    #[test]
    fn test_is_ready_batch_size_1() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_ready(&ssn).unwrap());

        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node, ssn.clone());

        assert!(plugin.is_ready(&ssn).unwrap());
    }

    #[test]
    fn test_is_ready_batch_size_2() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_ready(&ssn).unwrap());

        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node.clone(), ssn.clone());

        assert!(!plugin.is_ready(&ssn).unwrap());

        plugin.on_executor_allocate(node, ssn.clone());

        assert!(plugin.is_ready(&ssn).unwrap());
    }

    #[test]
    fn test_is_ready_with_allocated() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let exec = create_test_executor("exec-1", Some("ssn-1"));
        ss.add_executor(exec).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        assert!(!plugin.is_ready(&ssn).unwrap());

        let node = create_test_node("node-1");
        plugin.on_executor_allocate(node, ssn.clone());

        assert!(plugin.is_ready(&ssn).unwrap());
    }

    #[test]
    fn test_on_pipeline_and_discard() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        let exec = create_test_executor("exec-1", None);
        plugin.on_executor_pipeline(exec.clone(), ssn.clone());

        assert!(!plugin.is_ready(&ssn).unwrap());

        plugin.on_executor_pipeline(exec.clone(), ssn.clone());

        assert!(plugin.is_ready(&ssn).unwrap());

        plugin.on_executor_discard(exec.clone(), ssn.clone());

        assert!(!plugin.is_ready(&ssn).unwrap());

        plugin.on_executor_discard(exec, ssn.clone());

        assert!(!plugin.is_ready(&ssn).unwrap());
    }

    #[test]
    fn test_on_bind_and_unbind() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let ssn = create_test_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let mut plugin = GangPlugin {
            ssn_state: HashMap::new(),
        };
        plugin.setup(&ss).unwrap();

        plugin.on_session_bind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn).unwrap());

        plugin.on_session_bind(ssn.clone());

        assert!(plugin.is_fulfilled(&ssn).unwrap());

        plugin.on_session_unbind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn).unwrap());

        plugin.on_session_unbind(ssn.clone());

        assert!(!plugin.is_fulfilled(&ssn).unwrap());
    }
}

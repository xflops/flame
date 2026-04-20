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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use common::apis::{ExecutorState, Node, NodeInfo, NodeState, ResourceRequirement, Shim};
    use common::ctx::{FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits};
    use common::FlameError;
    use stdng::lock_ptr;

    use crate::model::Executor;
    use crate::storage::engine::{Engine, SqliteEngine};

    fn create_test_context(db_url: &str) -> FlameClusterContext {
        FlameClusterContext {
            cluster: FlameCluster {
                name: "test".to_string(),
                endpoint: "http://localhost:8080".to_string(),
                storage: db_url.to_string(),
                slot: ResourceRequirement::default(),
                policy: "fifo".to_string(),
                schedule_interval: 1000,
                executors: FlameExecutors {
                    shim: Shim::default(),
                },
                tls: None,
                limits: FlameLimits {
                    max_sessions: None,
                    max_executors: 10,
                },
            },
            cache: None,
        }
    }

    #[test]
    fn test_load_data_resets_binding_executor_to_idle() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_load_data_binding_recovery");

        let engine = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        let node = Node {
            name: "recovery-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            info: NodeInfo::default(),
        };
        tokio_test::block_on(engine.create_node(&node))?;

        let binding_executor = Executor {
            id: "binding-exec".to_string(),
            node: "recovery-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 2,
                memory: 4096,
            },
            slots: 2,
            shim: Shim::Host,
            task_id: None,
            ssn_id: Some("incomplete-session".to_string()),
            creation_time: Utc::now(),
            state: ExecutorState::Binding,
        };
        tokio_test::block_on(engine.create_executor(&binding_executor))?;

        let idle_executor = Executor {
            id: "idle-exec".to_string(),
            node: "recovery-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 2,
                memory: 4096,
            },
            slots: 2,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Idle,
        };
        tokio_test::block_on(engine.create_executor(&idle_executor))?;

        let ctx = create_test_context(&url);
        let storage = tokio_test::block_on(crate::storage::new_ptr(&ctx))?;
        tokio_test::block_on(storage.load_data())?;

        let executors = storage.list_executor(None)?;
        assert_eq!(executors.len(), 2);

        let binding_exec = executors.iter().find(|e| e.id == "binding-exec").unwrap();
        assert_eq!(binding_exec.state, ExecutorState::Idle);
        assert_eq!(binding_exec.ssn_id, None);

        let idle_exec = executors.iter().find(|e| e.id == "idle-exec").unwrap();
        assert_eq!(idle_exec.state, ExecutorState::Idle);

        let db_executor = tokio_test::block_on(engine.get_executor(&"binding-exec".to_string()))?;
        assert!(db_executor.is_some());
        let db_executor = db_executor.unwrap();
        assert_eq!(db_executor.state, ExecutorState::Idle);
        assert_eq!(db_executor.ssn_id, None);

        Ok(())
    }

    #[test]
    fn test_load_data_preserves_other_executor_states() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_load_data_preserves_states");

        let engine = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        let node = Node {
            name: "state-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            info: NodeInfo::default(),
        };
        tokio_test::block_on(engine.create_node(&node))?;

        let states_to_test = vec![
            ("void-exec", ExecutorState::Void),
            ("idle-exec", ExecutorState::Idle),
            ("bound-exec", ExecutorState::Bound),
            ("unbinding-exec", ExecutorState::Unbinding),
            ("releasing-exec", ExecutorState::Releasing),
        ];

        for (id, state) in &states_to_test {
            let executor = Executor {
                id: id.to_string(),
                node: "state-node".to_string(),
                resreq: ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                },
                slots: 1,
                shim: Shim::Host,
                task_id: None,
                ssn_id: None,
                creation_time: Utc::now(),
                state: *state,
            };
            tokio_test::block_on(engine.create_executor(&executor))?;
        }

        let ctx = create_test_context(&url);
        let storage = tokio_test::block_on(crate::storage::new_ptr(&ctx))?;
        tokio_test::block_on(storage.load_data())?;

        let executors = storage.list_executor(None)?;
        assert_eq!(executors.len(), states_to_test.len());

        for (id, expected_state) in &states_to_test {
            let exec = executors.iter().find(|e| e.id == *id).unwrap();
            assert_eq!(
                exec.state, *expected_state,
                "Executor {} should remain in {:?} state",
                id, expected_state
            );
        }

        Ok(())
    }
}

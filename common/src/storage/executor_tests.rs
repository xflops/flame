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
    use crate::apis::ExecutorFilter;
    use crate::apis::{
        ApplicationAttributes, ExecutorState, Node, NodeState, ResourceRequirement,
        SessionAttributes, Shim,
    };
    use crate::ctx::{FlameCluster, FlameClusterContext};
    use crate::storage;
    use stdng::lock_ptr;

    fn test_context() -> FlameClusterContext {
        FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    async fn test_storage() -> storage::StoragePtr {
        let storage = storage::new_ptr(&test_context()).await.unwrap();
        storage
            .register_application(
                "default".to_string(),
                "test-app".to_string(),
                Default::default(),
            )
            .await
            .unwrap();
        storage
    }

    fn create_session_attr(id: &str) -> SessionAttributes {
        SessionAttributes {
            tokens: Default::default(),
            workspace: "default".to_string(),
            name: id.to_string(),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 1,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: Some(ResourceRequirement::default()),
        }
    }

    fn create_test_node(name: &str) -> Node {
        Node {
            name: name.to_string(),
            state: NodeState::Ready,
            ..Default::default()
        }
    }

    mod create_executor {
        use super::*;

        #[tokio::test]
        async fn creates_executor_for_session() {
            let storage = test_storage().await;

            let attr = create_session_attr("exec-create-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("exec-node");
            storage.register_node(&node).await.unwrap();

            let executor = storage
                .create_executor("exec-node".to_string(), "default", "exec-create-ssn")
                .await
                .unwrap();

            assert_eq!(executor.node, "exec-node");
            assert_eq!(executor.application, "test-app");
            assert_eq!(executor.state, ExecutorState::Void);
        }

        #[tokio::test]
        async fn creates_executor_with_unique_id() {
            let storage = test_storage().await;

            let attr = create_session_attr("multi-exec-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("multi-exec-node");
            storage.register_node(&node).await.unwrap();

            let exec1 = storage
                .create_executor("multi-exec-node".to_string(), "default", "multi-exec-ssn")
                .await
                .unwrap();
            let exec2 = storage
                .create_executor("multi-exec-node".to_string(), "default", "multi-exec-ssn")
                .await
                .unwrap();

            assert_ne!(exec1.id, exec2.id);
        }

        #[tokio::test]
        async fn uses_application_shim() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();
            storage
                .register_application(
                    "default".to_string(),
                    "test-app".to_string(),
                    ApplicationAttributes {
                        shim: Shim::Cri,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            storage
                .create_session(create_session_attr("cri-exec-ssn"))
                .await
                .unwrap();

            let executor = storage
                .create_executor("cri-node".to_string(), "default", "cri-exec-ssn")
                .await
                .unwrap();

            assert_eq!(executor.shim, Shim::Cri);
        }

        #[tokio::test]
        async fn returns_error_for_nonexistent_session() {
            let storage = test_storage().await;

            let result = storage
                .create_executor("some-node".to_string(), "default", "nonexistent-ssn")
                .await;
            assert!(result.is_err());
        }
    }

    mod get_executor_ptr {
        use super::*;

        #[tokio::test]
        async fn returns_executor_by_id() {
            let storage = test_storage().await;

            let attr = create_session_attr("get-exec-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("get-exec-node");
            storage.register_node(&node).await.unwrap();

            let created_exec = storage
                .create_executor("get-exec-node".to_string(), "default", "get-exec-ssn")
                .await
                .unwrap();

            let exec_ptr = storage.get_executor_ptr(&created_exec.name).unwrap();
            let exec = lock_ptr!(exec_ptr).unwrap();

            assert_eq!(exec.id, created_exec.id);
        }

        #[tokio::test]
        async fn returns_error_for_nonexistent_executor() {
            let storage = test_storage().await;

            let result = storage.get_executor_ptr("nonexistent-exec");
            assert!(result.is_err());
        }
    }

    mod update_executor {
        use super::*;

        #[tokio::test]
        async fn updates_executor_state() {
            let storage = test_storage().await;

            let attr = create_session_attr("update-exec-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("update-exec-node");
            storage.register_node(&node).await.unwrap();

            let mut executor = storage
                .create_executor("update-exec-node".to_string(), "default", "update-exec-ssn")
                .await
                .unwrap();

            executor.state = ExecutorState::Idle;
            storage.update_executor(&executor).await.unwrap();

            let exec_ptr = storage.get_executor_ptr(&executor.name).unwrap();
            let exec = lock_ptr!(exec_ptr).unwrap();
            assert_eq!(exec.state, ExecutorState::Idle);
        }

        #[tokio::test]
        async fn updates_executor_with_session_binding() {
            let storage = test_storage().await;

            let attr = create_session_attr("bind-exec-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("bind-exec-node");
            storage.register_node(&node).await.unwrap();

            let mut executor = storage
                .create_executor("bind-exec-node".to_string(), "default", "bind-exec-ssn")
                .await
                .unwrap();

            executor.state = ExecutorState::Binding;
            executor.session = Some("bind-exec-ssn".to_string());
            storage.update_executor(&executor).await.unwrap();

            let exec_ptr = storage.get_executor_ptr(&executor.name).unwrap();
            let exec = lock_ptr!(exec_ptr).unwrap();
            assert_eq!(exec.state, ExecutorState::Binding);
            assert_eq!(exec.session, Some("bind-exec-ssn".to_string()));
        }
    }

    mod delete_executor {
        use super::*;

        #[tokio::test]
        async fn deletes_executor_from_storage() {
            let storage = test_storage().await;

            let attr = create_session_attr("del-exec-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("del-exec-node");
            storage.register_node(&node).await.unwrap();

            let executor = storage
                .create_executor("del-exec-node".to_string(), "default", "del-exec-ssn")
                .await
                .unwrap();

            storage
                .delete_executor("default", &executor.name)
                .await
                .unwrap();

            let result = storage.get_executor_ptr(&executor.name);
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn delete_removes_from_list() {
            let storage = test_storage().await;

            let attr = create_session_attr("del-list-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("del-list-node");
            storage.register_node(&node).await.unwrap();

            let executor = storage
                .create_executor("del-list-node".to_string(), "default", "del-list-ssn")
                .await
                .unwrap();

            assert_eq!(storage.list_executors(None).unwrap().len(), 1);

            storage
                .delete_executor("default", &executor.name)
                .await
                .unwrap();

            assert_eq!(storage.list_executors(None).unwrap().len(), 0);
        }
    }

    mod delete_executors {
        use super::*;

        #[tokio::test]
        async fn deletes_multiple_executors() {
            let storage = test_storage().await;

            let attr = create_session_attr("bulk-del-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("bulk-del-node");
            storage.register_node(&node).await.unwrap();

            let exec1 = storage
                .create_executor("bulk-del-node".to_string(), "default", "bulk-del-ssn")
                .await
                .unwrap();
            let exec2 = storage
                .create_executor("bulk-del-node".to_string(), "default", "bulk-del-ssn")
                .await
                .unwrap();

            let to_delete = vec![exec1, exec2];
            let deleted_ids = storage.delete_executors(&to_delete).await.unwrap();

            assert_eq!(deleted_ids.len(), 2);
            assert_eq!(storage.list_executors(None).unwrap().len(), 0);
        }
    }

    mod list_executors {
        use super::*;

        #[tokio::test]
        async fn returns_empty_list_when_no_executors() {
            let storage = test_storage().await;

            let executors = storage.list_executors(None).unwrap();
            assert!(executors.is_empty());
        }

        #[tokio::test]
        async fn returns_all_executors_with_no_filter() {
            let storage = test_storage().await;

            let attr = create_session_attr("list-exec-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("list-exec-node");
            storage.register_node(&node).await.unwrap();

            for _ in 0..3 {
                storage
                    .create_executor("list-exec-node".to_string(), "default", "list-exec-ssn")
                    .await
                    .unwrap();
            }

            let executors = storage.list_executors(None).unwrap();
            assert_eq!(executors.len(), 3);
        }

        #[tokio::test]
        async fn filters_by_state() {
            let storage = test_storage().await;

            let attr = create_session_attr("filter-state-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("filter-state-node");
            storage.register_node(&node).await.unwrap();

            let mut exec1 = storage
                .create_executor(
                    "filter-state-node".to_string(),
                    "default",
                    "filter-state-ssn",
                )
                .await
                .unwrap();
            exec1.state = ExecutorState::Idle;
            storage.update_executor(&exec1).await.unwrap();

            storage
                .create_executor(
                    "filter-state-node".to_string(),
                    "default",
                    "filter-state-ssn",
                )
                .await
                .unwrap();

            let filter = ExecutorFilter {
                state: Some(ExecutorState::Idle),
                node: None,
                names: None,
            };
            let filtered = storage.list_executors(Some(&filter)).unwrap();
            assert_eq!(filtered.len(), 1);
            assert_eq!(filtered[0].state, ExecutorState::Idle);
        }

        #[tokio::test]
        async fn filters_by_node() {
            let storage = test_storage().await;

            let attr = create_session_attr("filter-node-ssn");
            storage.create_session(attr).await.unwrap();

            let node1 = create_test_node("node-1");
            storage.register_node(&node1).await.unwrap();
            let node2 = create_test_node("node-2");
            storage.register_node(&node2).await.unwrap();

            storage
                .create_executor("node-1".to_string(), "default", "filter-node-ssn")
                .await
                .unwrap();
            storage
                .create_executor("node-1".to_string(), "default", "filter-node-ssn")
                .await
                .unwrap();
            storage
                .create_executor("node-2".to_string(), "default", "filter-node-ssn")
                .await
                .unwrap();

            let filter = ExecutorFilter {
                state: None,
                node: Some("node-1".to_string()),
                names: None,
            };
            let filtered = storage.list_executors(Some(&filter)).unwrap();
            assert_eq!(filtered.len(), 2);
            assert!(filtered.iter().all(|e| e.node == "node-1"));
        }

        #[tokio::test]
        async fn filters_by_ids() {
            let storage = test_storage().await;

            let attr = create_session_attr("filter-ids-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("filter-ids-node");
            storage.register_node(&node).await.unwrap();

            let exec1 = storage
                .create_executor("filter-ids-node".to_string(), "default", "filter-ids-ssn")
                .await
                .unwrap();
            let exec2 = storage
                .create_executor("filter-ids-node".to_string(), "default", "filter-ids-ssn")
                .await
                .unwrap();
            storage
                .create_executor("filter-ids-node".to_string(), "default", "filter-ids-ssn")
                .await
                .unwrap();

            let filter = ExecutorFilter {
                state: None,
                node: None,
                names: Some(vec![exec1.name.clone(), exec2.name.clone()]),
            };
            let filtered = storage.list_executors(Some(&filter)).unwrap();
            assert_eq!(filtered.len(), 2);

            let names: Vec<_> = filtered.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&exec1.name.as_str()));
            assert!(names.contains(&exec2.name.as_str()));
        }

        #[tokio::test]
        async fn combines_multiple_filters() {
            let storage = test_storage().await;

            let attr = create_session_attr("multi-filter-ssn");
            storage.create_session(attr).await.unwrap();

            let node1 = create_test_node("mf-node-1");
            storage.register_node(&node1).await.unwrap();
            let node2 = create_test_node("mf-node-2");
            storage.register_node(&node2).await.unwrap();

            let mut exec1 = storage
                .create_executor("mf-node-1".to_string(), "default", "multi-filter-ssn")
                .await
                .unwrap();
            exec1.state = ExecutorState::Idle;
            storage.update_executor(&exec1).await.unwrap();

            let mut exec2 = storage
                .create_executor("mf-node-1".to_string(), "default", "multi-filter-ssn")
                .await
                .unwrap();
            exec2.state = ExecutorState::Binding;
            storage.update_executor(&exec2).await.unwrap();

            let mut exec3 = storage
                .create_executor("mf-node-2".to_string(), "default", "multi-filter-ssn")
                .await
                .unwrap();
            exec3.state = ExecutorState::Idle;
            storage.update_executor(&exec3).await.unwrap();

            let filter = ExecutorFilter {
                state: Some(ExecutorState::Idle),
                node: Some("mf-node-1".to_string()),
                names: None,
            };
            let filtered = storage.list_executors(Some(&filter)).unwrap();
            assert_eq!(filtered.len(), 1);
            assert_eq!(filtered[0].node, "mf-node-1");
            assert_eq!(filtered[0].state, ExecutorState::Idle);
        }
    }
}

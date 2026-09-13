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
    use crate::storage;
    use common::apis::{
        ApplicationAttributes, Node, NodeState, ResourceRequirement, SessionAttributes,
        SessionState, TaskOptions, TaskState,
    };
    use common::ctx::{FlameCluster, FlameClusterContext};
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

    fn create_session_attr(id: &str) -> SessionAttributes {
        SessionAttributes {
            id: id.to_string(),
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

    fn create_app_attr() -> ApplicationAttributes {
        ApplicationAttributes::default()
    }

    mod snapshot {
        use super::*;

        #[tokio::test]
        async fn returns_empty_snapshot_initially() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let snapshot = storage.snapshot().unwrap();

            let sessions = lock_ptr!(snapshot.sessions).unwrap();
            let executors = lock_ptr!(snapshot.executors).unwrap();
            let nodes = lock_ptr!(snapshot.nodes).unwrap();

            assert!(sessions.is_empty());
            assert!(executors.is_empty());
            assert!(nodes.is_empty());
        }

        #[tokio::test]
        async fn includes_all_sessions() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            for i in 0..3 {
                let attr = create_session_attr(&format!("snap-ssn-{}", i));
                storage.create_session(attr).await.unwrap();
            }

            let snapshot = storage.snapshot().unwrap();
            let sessions = lock_ptr!(snapshot.sessions).unwrap();

            assert_eq!(sessions.len(), 3);
        }

        #[tokio::test]
        async fn includes_all_nodes() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            for i in 0..2 {
                let node = create_test_node(&format!("snap-node-{}", i));
                storage.register_node(&node).await.unwrap();
            }

            let snapshot = storage.snapshot().unwrap();
            let nodes = lock_ptr!(snapshot.nodes).unwrap();

            assert_eq!(nodes.len(), 2);
        }

        #[tokio::test]
        async fn includes_all_executors() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_session_attr("exec-snap-ssn");
            storage.create_session(attr).await.unwrap();

            let node = create_test_node("exec-snap-node");
            storage.register_node(&node).await.unwrap();

            for _ in 0..2 {
                storage
                    .create_executor("exec-snap-node".to_string(), "exec-snap-ssn".to_string())
                    .await
                    .unwrap();
            }

            let snapshot = storage.snapshot().unwrap();
            let executors = lock_ptr!(snapshot.executors).unwrap();

            assert_eq!(executors.len(), 2);
        }

        #[tokio::test]
        async fn includes_all_applications() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            storage
                .register_application("snap-app-1".to_string(), attr.clone())
                .await
                .unwrap();
            storage
                .register_application("snap-app-2".to_string(), attr)
                .await
                .unwrap();

            let snapshot = storage.snapshot().unwrap();
            let applications = lock_ptr!(snapshot.applications).unwrap();

            assert_eq!(applications.len(), 2);
        }

        #[tokio::test]
        async fn snapshot_reflects_state_changes() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_session_attr("state-snap-ssn");
            storage.create_session(attr).await.unwrap();
            storage
                .close_session("state-snap-ssn".to_string())
                .await
                .unwrap();

            let snapshot = storage.snapshot().unwrap();
            let sessions = lock_ptr!(snapshot.sessions).unwrap();

            let ssn_info = sessions.values().next().unwrap();
            assert_eq!(ssn_info.state, SessionState::Closed);
        }

        #[tokio::test]
        async fn task_index_contains_only_non_terminal_tasks_with_affinity() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();
            storage
                .create_session(create_session_attr("task-index-ssn"))
                .await
                .unwrap();

            let pending_key = bytes::Bytes::from_static(b"pending");
            let pending = storage
                .create_task(
                    "task-index-ssn".to_string(),
                    None,
                    Some(TaskOptions {
                        affinity: [pending_key.clone()].into_iter().collect(),
                    }),
                )
                .await
                .unwrap();
            let running = storage
                .create_task(
                    "task-index-ssn".to_string(),
                    None,
                    Some(TaskOptions {
                        affinity: [bytes::Bytes::from_static(b"running")]
                            .into_iter()
                            .collect(),
                    }),
                )
                .await
                .unwrap();
            let finished = storage
                .create_task("task-index-ssn".to_string(), None, None)
                .await
                .unwrap();
            let session = storage
                .get_session_ptr("task-index-ssn".to_string())
                .unwrap();
            let running_ptr = storage.get_task_ptr(running.gid()).unwrap();
            storage
                .update_task_state(session.clone(), running_ptr, TaskState::Running, None)
                .await
                .unwrap();
            let finished_ptr = storage.get_task_ptr(finished.gid()).unwrap();
            storage
                .update_task_state(session, finished_ptr, TaskState::Succeed, None)
                .await
                .unwrap();

            let snapshot = storage.snapshot().unwrap();
            let sessions = lock_ptr!(snapshot.sessions).unwrap();
            let session = sessions.get("task-index-ssn").unwrap();
            let indexed = session.task_index.get(&TaskState::Pending).unwrap();

            assert_eq!(indexed.len(), 1);
            let task = indexed.get(&pending.id).unwrap();
            assert_eq!(task.state, TaskState::Pending);
            assert_eq!(task.affinity, [pending_key].into_iter().collect());
            assert_eq!(
                session.task_index.get(&TaskState::Running).unwrap().len(),
                1
            );
            assert!(!session.task_index.contains_key(&TaskState::Succeed));
        }
    }
}

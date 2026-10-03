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
    use crate::apis::{
        Event, EventOwner, ResourceRequirement, SessionAttributes, SessionState, TaskState,
    };
    use crate::ctx::{FlameCluster, FlameClusterContext};
    use crate::storage;
    use chrono::Utc;

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

    mod create_session {
        use super::*;

        #[tokio::test]
        async fn creates_session_with_correct_id() {
            let storage = test_storage().await;

            let mut attr = create_session_attr("test-ssn-1");
            attr.tokens.insert("service".into(), "credential".into());
            let ssn = storage.create_session(attr).await.unwrap();

            assert_eq!(ssn.name, "test-ssn-1");
            assert_eq!(ssn.application, "test-app");
            assert_eq!(
                ssn.tokens.get("service").map(String::as_str),
                Some("credential")
            );
            assert_eq!(ssn.status.state, SessionState::Open);
        }

        #[tokio::test]
        async fn creates_session_with_specified_resreq() {
            let storage = test_storage().await;

            let mut attr = create_session_attr("test-ssn-resreq");
            attr.resreq = Some(ResourceRequirement {
                cpu: 4,
                memory: 8 * 1024 * 1024 * 1024,
                gpu: 2,
            });
            let ssn = storage.create_session(attr).await.unwrap();

            let resreq = ssn.resreq.expect("session resreq should be stored");
            assert_eq!(resreq.cpu, 4);
            assert_eq!(resreq.gpu, 2);
        }

        #[tokio::test]
        async fn creates_multiple_sessions() {
            let storage = test_storage().await;

            for i in 0..5 {
                let attr = create_session_attr(&format!("ssn-{}", i));
                storage.create_session(attr).await.unwrap();
            }

            let sessions = storage
                .list_sessions(&crate::apis::SessionFilter::new("default"))
                .unwrap();
            assert_eq!(sessions.len(), 5);
        }
    }

    mod get_session {
        use super::*;

        #[tokio::test]
        async fn returns_session_by_id() {
            let storage = test_storage().await;

            let attr = create_session_attr("get-test-ssn");
            storage.create_session(attr).await.unwrap();

            let ssn = storage.get_session("default", "get-test-ssn").unwrap();
            assert_eq!(ssn.name, "get-test-ssn");
        }

        #[tokio::test]
        async fn returns_error_for_nonexistent_session() {
            let storage = test_storage().await;

            let result = storage.get_session("default", "nonexistent");
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn get_session_ptr_returns_pointer() {
            let storage = test_storage().await;

            let attr = create_session_attr("ptr-test-ssn");
            storage.create_session(attr).await.unwrap();

            let ssn_ptr = storage.get_session_ptr("default", "ptr-test-ssn").unwrap();
            let ssn = stdng::lock_ptr!(ssn_ptr).unwrap();
            assert_eq!(ssn.name, "ptr-test-ssn");
        }

        #[tokio::test]
        async fn includes_session_events() {
            let storage = test_storage().await;

            let attr = create_session_attr("event-test-ssn");
            storage.create_session(attr).await.unwrap();
            storage
                .record_event(
                    EventOwner::session("default".to_string(), "event-test-ssn".to_string()),
                    Event {
                        code: 1001,
                        message: Some("bind failed".to_string()),
                        creation_time: Utc::now(),
                    },
                )
                .await
                .unwrap();

            let ssn = storage.get_session("default", "event-test-ssn").unwrap();

            assert_eq!(ssn.events.len(), 1);
            assert_eq!(ssn.events[0].code, 1001);
            assert_eq!(ssn.events[0].message.as_deref(), Some("bind failed"));
        }
    }

    mod open_session {
        use super::*;

        #[tokio::test]
        async fn creates_new_session_if_not_exists() {
            let storage = test_storage().await;

            let attr = create_session_attr("open-new-ssn");
            let ssn = storage
                .open_session("default", "open-new-ssn", Some(attr))
                .await
                .unwrap();

            assert_eq!(ssn.name, "open-new-ssn");
            assert_eq!(ssn.status.state, SessionState::Open);
        }

        #[tokio::test]
        async fn returns_existing_open_session() {
            let storage = test_storage().await;

            let attr = create_session_attr("existing-ssn");
            storage.create_session(attr.clone()).await.unwrap();

            let ssn = storage
                .open_session("default", "existing-ssn", Some(attr))
                .await
                .unwrap();

            assert_eq!(ssn.name, "existing-ssn");
            assert_eq!(ssn.status.state, SessionState::Open);
        }
    }

    mod close_session {
        use super::*;

        #[tokio::test]
        async fn closes_session_and_sets_state() {
            let storage = test_storage().await;

            let attr = create_session_attr("close-test-ssn");
            storage.create_session(attr).await.unwrap();

            let ssn = storage
                .close_session("default", "close-test-ssn")
                .await
                .unwrap();

            assert_eq!(ssn.status.state, SessionState::Closed);
            assert!(ssn.completion_time.is_some());
        }

        #[tokio::test]
        async fn close_returns_error_for_nonexistent_session() {
            let storage = test_storage().await;

            let result = storage.close_session("default", "nonexistent").await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn closed_session_reflects_in_get() {
            let storage = test_storage().await;

            let attr = create_session_attr("verify-close-ssn");
            storage.create_session(attr).await.unwrap();
            storage
                .close_session("default", "verify-close-ssn")
                .await
                .unwrap();

            let ssn = storage.get_session("default", "verify-close-ssn").unwrap();
            assert_eq!(ssn.status.state, SessionState::Closed);
        }

        #[tokio::test]
        async fn close_cancels_pending_tasks_in_cache() {
            let storage = test_storage().await;

            let attr = create_session_attr("close-cancel-pending-ssn");
            storage.create_session(attr).await.unwrap();
            let task = storage
                .create_task("default", "close-cancel-pending-ssn", None, None)
                .await
                .unwrap();

            storage
                .close_session("default", "close-cancel-pending-ssn")
                .await
                .unwrap();

            let task = storage
                .get_task(
                    "default",
                    "close-cancel-pending-ssn",
                    &task.name.to_string(),
                )
                .unwrap();
            assert_eq!(task.state, TaskState::Cancelled);
            assert!(task.completion_time.is_some());
        }

        #[tokio::test]
        async fn close_rejects_running_tasks_without_mutating_session() {
            let storage = test_storage().await;

            let attr = create_session_attr("close-running-ssn");
            storage.create_session(attr).await.unwrap();
            let task = storage
                .create_task("default", "close-running-ssn", None, None)
                .await
                .unwrap();
            let ssn_ptr = storage
                .get_session_ptr("default", "close-running-ssn")
                .unwrap();
            let task_ptr = storage
                .get_task_ptr("default", "close-running-ssn", &task.name.to_string())
                .unwrap();
            storage
                .update_task_state(ssn_ptr, task_ptr, TaskState::Running, None)
                .await
                .unwrap();

            let result = storage.close_session("default", "close-running-ssn").await;
            assert!(result.is_err());

            let ssn = storage.get_session("default", "close-running-ssn").unwrap();
            assert_eq!(ssn.status.state, SessionState::Open);
        }
    }

    mod delete_session {
        use super::*;

        #[tokio::test]
        async fn deletes_session_from_storage() {
            let storage = test_storage().await;

            let attr = create_session_attr("delete-test-ssn");
            storage.create_session(attr).await.unwrap();

            let deleted = storage
                .delete_session("default", "delete-test-ssn")
                .await
                .unwrap();
            assert_eq!(deleted.name, "delete-test-ssn");

            let result = storage.get_session("default", "delete-test-ssn");
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn delete_returns_error_for_nonexistent() {
            let storage = test_storage().await;

            let result = storage.delete_session("default", "nonexistent").await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn delete_removes_from_list() {
            let storage = test_storage().await;

            let attr = create_session_attr("list-delete-ssn");
            storage.create_session(attr).await.unwrap();

            assert_eq!(
                storage
                    .list_sessions(&crate::apis::SessionFilter::new("default"))
                    .unwrap()
                    .len(),
                1
            );

            storage
                .delete_session("default", "list-delete-ssn")
                .await
                .unwrap();

            assert_eq!(
                storage
                    .list_sessions(&crate::apis::SessionFilter::new("default"))
                    .unwrap()
                    .len(),
                0
            );
        }
    }

    mod list_sessions {
        use super::*;

        #[tokio::test]
        async fn returns_empty_list_initially() {
            let storage = test_storage().await;

            let sessions = storage
                .list_sessions(&crate::apis::SessionFilter::new("default"))
                .unwrap();
            assert!(sessions.is_empty());
        }

        #[tokio::test]
        async fn returns_all_created_sessions() {
            let storage = test_storage().await;

            for i in 0..3 {
                let attr = create_session_attr(&format!("list-ssn-{}", i));
                storage.create_session(attr).await.unwrap();
            }

            let sessions = storage
                .list_sessions(&crate::apis::SessionFilter::new("default"))
                .unwrap();
            assert_eq!(sessions.len(), 3);

            let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
            assert!(names.contains(&"list-ssn-0"));
            assert!(names.contains(&"list-ssn-1"));
            assert!(names.contains(&"list-ssn-2"));
        }

        #[tokio::test]
        async fn includes_both_open_and_closed_sessions() {
            let storage = test_storage().await;

            let attr1 = create_session_attr("open-ssn");
            storage.create_session(attr1).await.unwrap();

            let attr2 = create_session_attr("closed-ssn");
            storage.create_session(attr2).await.unwrap();
            storage
                .close_session("default", "closed-ssn")
                .await
                .unwrap();

            let sessions = storage
                .list_sessions(&crate::apis::SessionFilter::new("default"))
                .unwrap();
            assert_eq!(sessions.len(), 2);

            let open_count = sessions
                .iter()
                .filter(|s| s.status.state == SessionState::Open)
                .count();
            let closed_count = sessions
                .iter()
                .filter(|s| s.status.state == SessionState::Closed)
                .count();

            assert_eq!(open_count, 1);
            assert_eq!(closed_count, 1);
        }

        #[tokio::test]
        async fn includes_session_events() {
            let storage = test_storage().await;

            let attr = create_session_attr("list-event-ssn");
            storage.create_session(attr).await.unwrap();
            storage
                .record_event(
                    EventOwner::session("default".to_string(), "list-event-ssn".to_string()),
                    Event {
                        code: 1002,
                        message: Some("retry limit reached".to_string()),
                        creation_time: Utc::now(),
                    },
                )
                .await
                .unwrap();

            let sessions = storage
                .list_sessions(&crate::apis::SessionFilter::new("default"))
                .unwrap();
            let session = sessions
                .iter()
                .find(|session| session.name == "list-event-ssn")
                .expect("session should be listed");

            assert_eq!(session.events.len(), 1);
            assert_eq!(session.events[0].code, 1002);
            assert_eq!(
                session.events[0].message.as_deref(),
                Some("retry limit reached")
            );
        }
    }
}

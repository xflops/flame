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
    use crate::apis::ApplicationAttributes;
    use crate::ctx::{FlameCluster, FlameClusterContext};
    use crate::storage;

    fn test_context() -> FlameClusterContext {
        FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn create_app_attr() -> ApplicationAttributes {
        ApplicationAttributes::default()
    }

    mod register_application {
        use super::*;

        #[tokio::test]
        async fn registers_new_application() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            storage
                .register_application("default".to_string(), "test-app".to_string(), attr)
                .await
                .unwrap();

            let apps = storage
                .list_applications(&crate::apis::ApplicationFilter::new("default"))
                .await
                .unwrap();
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].name, "test-app");
        }

        #[tokio::test]
        async fn registers_multiple_applications() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            storage
                .register_application("default".to_string(), "app-1".to_string(), attr.clone())
                .await
                .unwrap();
            storage
                .register_application("default".to_string(), "app-2".to_string(), attr)
                .await
                .unwrap();

            let apps = storage
                .list_applications(&crate::apis::ApplicationFilter::new("default"))
                .await
                .unwrap();
            assert_eq!(apps.len(), 2);
        }

        #[tokio::test]
        async fn stores_application_attributes() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = ApplicationAttributes {
                image: Some("custom-image:v1".to_string()),
                command: Some("python main.py".to_string()),
                ..Default::default()
            };
            storage
                .register_application("default".to_string(), "attr-app".to_string(), attr)
                .await
                .unwrap();

            let app = storage
                .get_application("default", "attr-app")
                .await
                .unwrap();
            assert_eq!(app.image, Some("custom-image:v1".to_string()));
            assert_eq!(app.command, Some("python main.py".to_string()));
        }
    }

    mod get_application {
        use super::*;

        #[tokio::test]
        async fn returns_application_by_id() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            storage
                .register_application("default".to_string(), "get-app".to_string(), attr)
                .await
                .unwrap();

            let app = storage.get_application("default", "get-app").await.unwrap();
            assert_eq!(app.name, "get-app");
        }

        #[tokio::test]
        async fn returns_error_for_nonexistent_application() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let result = storage.get_application("default", "nonexistent").await;
            assert!(result.is_err());
        }
    }

    mod update_application {
        use super::*;
        use crate::apis::ApplicationState;

        #[tokio::test]
        async fn updates_existing_application() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            storage
                .register_application("default".to_string(), "update-app".to_string(), attr)
                .await
                .unwrap();

            let new_attr = ApplicationAttributes {
                image: Some("new-image:v2".to_string()),
                ..Default::default()
            };
            storage
                .update_application("default", "update-app", new_attr)
                .await
                .unwrap();

            let app = storage
                .get_application("default", "update-app")
                .await
                .unwrap();
            assert_eq!(app.image, Some("new-image:v2".to_string()));
        }

        #[tokio::test]
        async fn rejects_updates_to_disabled_application() {
            let storage = storage::new_ptr(&test_context()).await.unwrap();
            storage
                .register_application(
                    "default".to_string(),
                    "disabled-app".to_string(),
                    create_app_attr(),
                )
                .await
                .unwrap();
            let disabled = storage
                .update_application_state("default", "disabled-app", ApplicationState::Disabled)
                .await
                .unwrap();

            let result = storage
                .update_application(
                    "default",
                    "disabled-app",
                    ApplicationAttributes {
                        image: Some("must-not-be-written".to_string()),
                        ..Default::default()
                    },
                )
                .await;
            assert!(matches!(result, Err(crate::FlameError::InvalidState(_))));

            let unchanged = storage
                .get_application("default", "disabled-app")
                .await
                .unwrap();
            assert_eq!(unchanged.version, disabled.version);
            assert_eq!(unchanged.image, disabled.image);
        }
    }

    mod application_lifecycle {
        use super::*;
        use crate::apis::{ApplicationState, ResourceRequirement, SessionAttributes, SessionState};

        fn create_session_attr(id: &str, app: &str) -> SessionAttributes {
            SessionAttributes {
                tokens: Default::default(),
                workspace: "default".to_string(),
                name: id.to_string(),
                application: app.to_string(),
                common_data: None,
                min_instances: 1,
                max_instances: None,
                batch_size: 1,
                priority: 0,
                resreq: Some(ResourceRequirement::default()),
            }
        }

        #[tokio::test]
        async fn updates_state_idempotently_and_removes_disabled_application() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            storage
                .register_application("default".to_string(), "unregister-app".to_string(), attr)
                .await
                .unwrap();

            let disabled = storage
                .update_application_state("default", "unregister-app", ApplicationState::Disabled)
                .await
                .unwrap();
            assert_eq!(disabled.state, ApplicationState::Disabled);
            assert_eq!(disabled.version, 2);

            let unchanged = storage
                .update_application_state("default", "unregister-app", ApplicationState::Disabled)
                .await
                .unwrap();
            assert_eq!(unchanged.version, disabled.version);

            storage
                .delete_application("default", "unregister-app")
                .await
                .unwrap();

            let apps = storage
                .list_applications(&crate::apis::ApplicationFilter::new("default"))
                .await
                .unwrap();
            assert!(apps.is_empty());
        }

        #[tokio::test]
        async fn rejects_deletion_until_open_sessions_close() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let app_attr = create_app_attr();
            storage
                .register_application("default".to_string(), "cleanup-app".to_string(), app_attr)
                .await
                .unwrap();

            let ssn_attr = create_session_attr("cleanup-ssn", "cleanup-app");
            storage.create_session(ssn_attr).await.unwrap();

            assert_eq!(
                storage
                    .list_sessions(&crate::apis::SessionFilter::new("default"))
                    .unwrap()
                    .len(),
                1
            );

            storage
                .update_application_state("default", "cleanup-app", ApplicationState::Disabled)
                .await
                .unwrap();

            let open_sessions = crate::apis::SessionFilter::new("default")
                .by_application("cleanup-app")
                .by_state(SessionState::Open);
            assert_eq!(storage.count_session(&open_sessions).unwrap(), 1);
            let result = storage.delete_application("default", "cleanup-app").await;
            assert!(matches!(result, Err(crate::FlameError::InvalidState(_))));

            storage
                .close_session("default", "cleanup-ssn")
                .await
                .unwrap();
            let result = storage.delete_application("default", "cleanup-app").await;
            assert!(matches!(result, Err(crate::FlameError::InvalidState(_))));
            assert_eq!(
                storage
                    .list_sessions(&crate::apis::SessionFilter::new("default"))
                    .unwrap()
                    .len(),
                1
            );

            storage
                .delete_session("default", "cleanup-ssn")
                .await
                .unwrap();
            storage
                .delete_application("default", "cleanup-app")
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

        #[tokio::test]
        async fn rejects_deleting_an_enabled_application() {
            let storage = storage::new_ptr(&test_context()).await.unwrap();
            storage
                .register_application(
                    "default".to_string(),
                    "enabled-app".to_string(),
                    create_app_attr(),
                )
                .await
                .unwrap();

            let result = storage.delete_application("default", "enabled-app").await;
            assert!(matches!(result, Err(crate::FlameError::InvalidState(_))));
        }
    }

    mod list_applications {
        use super::*;

        #[tokio::test]
        async fn returns_empty_list_when_no_applications() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let apps = storage
                .list_applications(&crate::apis::ApplicationFilter::new("default"))
                .await
                .unwrap();
            assert!(apps.is_empty());
        }

        #[tokio::test]
        async fn returns_all_registered_applications() {
            let ctx = test_context();
            let storage = storage::new_ptr(&ctx).await.unwrap();

            let attr = create_app_attr();
            for i in 0..3 {
                storage
                    .register_application(
                        "default".to_string(),
                        format!("list-app-{}", i),
                        attr.clone(),
                    )
                    .await
                    .unwrap();
            }

            let apps = storage
                .list_applications(&crate::apis::ApplicationFilter::new("default"))
                .await
                .unwrap();
            assert_eq!(apps.len(), 3);

            let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
            assert!(names.contains(&"list-app-0"));
            assert!(names.contains(&"list-app-1"));
            assert!(names.contains(&"list-app-2"));
        }

        #[tokio::test]
        async fn filters_by_application_state() {
            let storage = storage::new_ptr(&test_context()).await.unwrap();
            storage
                .register_application(
                    "default".to_string(),
                    "enabled-app".to_string(),
                    create_app_attr(),
                )
                .await
                .unwrap();
            storage
                .register_application(
                    "default".to_string(),
                    "disabled-app".to_string(),
                    create_app_attr(),
                )
                .await
                .unwrap();
            storage
                .update_application_state(
                    "default",
                    "disabled-app",
                    crate::apis::ApplicationState::Disabled,
                )
                .await
                .unwrap();

            let filter = crate::apis::ApplicationFilter::new("default")
                .by_state(crate::apis::ApplicationState::Disabled);
            let apps = storage.list_applications(&filter).await.unwrap();
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].name, "disabled-app");
        }
    }
}

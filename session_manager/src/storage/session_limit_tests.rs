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
    use common::apis::{SessionAttributes, SessionState};
    use common::ctx::{FlameCluster, FlameClusterContext, FlameLimits};
    use stdng::lock_ptr;

    fn test_context_with_limit(max_sessions: Option<usize>) -> FlameClusterContext {
        FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                limits: FlameLimits {
                    max_sessions,
                    max_executors: 128,
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_no_eviction_when_no_limit() {
        let ctx = test_context_with_limit(None);
        let storage = storage::new_ptr(&ctx).await.unwrap();

        for i in 0..5 {
            let attr = SessionAttributes {
                id: format!("ssn-{}", i),
                application: "test-app".to_string(),
                slots: 1,
                common_data: None,
                min_instances: 1,
                max_instances: None,
                batch_size: 1,
                priority: 0,
            };
            storage.create_session(attr).await.unwrap();
        }

        for i in 0..3 {
            storage.close_session(format!("ssn-{}", i)).await.unwrap();
        }

        let sessions = storage.list_session().unwrap();
        assert_eq!(sessions.len(), 5);
    }

    #[tokio::test]
    async fn test_eviction_when_limit_reached() {
        let ctx = test_context_with_limit(Some(3));
        let storage = storage::new_ptr(&ctx).await.unwrap();

        for i in 0..3 {
            let attr = SessionAttributes {
                id: format!("ssn-{}", i),
                application: "test-app".to_string(),
                slots: 1,
                common_data: None,
                min_instances: 1,
                max_instances: None,
                batch_size: 1,
                priority: 0,
            };
            storage.create_session(attr).await.unwrap();
        }

        let sessions_before = storage.list_session().unwrap();
        assert_eq!(sessions_before.len(), 3);

        storage.close_session("ssn-0".to_string()).await.unwrap();

        let sessions_after = storage.list_session().unwrap();
        assert_eq!(sessions_after.len(), 2);

        let session_ids: Vec<_> = sessions_after.iter().map(|s| s.id.as_str()).collect();
        assert!(!session_ids.contains(&"ssn-0"));
    }

    #[tokio::test]
    async fn test_no_eviction_under_limit() {
        let ctx = test_context_with_limit(Some(5));
        let storage = storage::new_ptr(&ctx).await.unwrap();

        for i in 0..3 {
            let attr = SessionAttributes {
                id: format!("ssn-{}", i),
                application: "test-app".to_string(),
                slots: 1,
                common_data: None,
                min_instances: 1,
                max_instances: None,
                batch_size: 1,
                priority: 0,
            };
            storage.create_session(attr).await.unwrap();
        }

        storage.close_session("ssn-0".to_string()).await.unwrap();

        let sessions = storage.list_session().unwrap();
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn test_eviction_only_removes_closed_sessions() {
        let ctx = test_context_with_limit(Some(3));
        let storage = storage::new_ptr(&ctx).await.unwrap();

        for i in 0..3 {
            let attr = SessionAttributes {
                id: format!("ssn-{}", i),
                application: "test-app".to_string(),
                slots: 1,
                common_data: None,
                min_instances: 1,
                max_instances: None,
                batch_size: 1,
                priority: 0,
            };
            storage.create_session(attr).await.unwrap();
        }

        storage.close_session("ssn-1".to_string()).await.unwrap();

        let sessions = storage.list_session().unwrap();
        assert_eq!(sessions.len(), 2);

        let open_count = sessions
            .iter()
            .filter(|s| s.status.state == SessionState::Open)
            .count();
        assert_eq!(open_count, 2);
    }
}

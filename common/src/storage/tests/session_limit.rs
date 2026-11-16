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

use crate::apis::{SessionAttributes, SessionState};
use crate::ctx::{FlameCluster, FlameClusterContext, FlameLimits};
use crate::storage;

fn test_context_with_limit(max_sessions: Option<usize>) -> FlameClusterContext {
    FlameClusterContext {
        cluster: FlameCluster {
            storage: "none".to_string(),
            limits: FlameLimits {
                max_sessions,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn retains_closed_metadata_without_a_limit() {
    let ctx = test_context_with_limit(None);
    let storage = storage::new_ptr(&ctx).await.unwrap();

    for i in 0..5 {
        let attr = SessionAttributes {
            batch_size: 1,
            tokens: Default::default(),
            id: format!("ssn-{}", i),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 1,
            max_instances: None,
            priority: 0,
            resreq: None,
        };
        storage.create_session(attr).await.unwrap();
    }

    for i in 0..3 {
        storage.close_session(format!("ssn-{}", i)).await.unwrap();
    }

    let sessions = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 5);
}

#[tokio::test]
async fn closed_handoff_preserves_history_at_open_limit() {
    let ctx = test_context_with_limit(Some(3));
    let storage = storage::new_ptr(&ctx).await.unwrap();

    for i in 0..3 {
        let attr = SessionAttributes {
            batch_size: 1,
            tokens: Default::default(),
            id: format!("ssn-{}", i),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 1,
            max_instances: None,
            priority: 0,
            resreq: None,
        };
        storage.create_session(attr).await.unwrap();
    }

    let sessions_before = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions_before.len(), 3);

    storage.close_session("ssn-0".to_string()).await.unwrap();

    assert!(storage.get_session_ptr("ssn-0".into()).is_ok());
    for id in ["ssn-1", "ssn-2"] {
        assert!(storage.get_session_ptr(id.into()).is_ok());
    }

    // Closed history remains in the coordinated in-memory metadata directory.
    let sessions_after = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions_after.len(), 3);
    assert_eq!(
        sessions_after
            .iter()
            .find(|session| session.id == "ssn-0")
            .unwrap()
            .status
            .state,
        SessionState::Closed
    );
}

#[tokio::test]
async fn retains_closed_metadata_below_open_limit() {
    let ctx = test_context_with_limit(Some(5));
    let storage = storage::new_ptr(&ctx).await.unwrap();

    for i in 0..3 {
        let attr = SessionAttributes {
            batch_size: 1,
            tokens: Default::default(),
            id: format!("ssn-{}", i),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 1,
            max_instances: None,
            priority: 0,
            resreq: None,
        };
        storage.create_session(attr).await.unwrap();
    }

    storage.close_session("ssn-0".to_string()).await.unwrap();

    let sessions = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 3);
}

#[tokio::test]
async fn closed_handoff_preserves_live_execution_sessions() {
    let ctx = test_context_with_limit(Some(3));
    let storage = storage::new_ptr(&ctx).await.unwrap();

    for i in 0..3 {
        let attr = SessionAttributes {
            batch_size: 1,
            tokens: Default::default(),
            id: format!("ssn-{}", i),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 1,
            max_instances: None,
            priority: 0,
            resreq: None,
        };
        storage.create_session(attr).await.unwrap();
    }

    storage.close_session("ssn-1".to_string()).await.unwrap();

    assert!(storage.get_session_ptr("ssn-1".into()).is_ok());
    for id in ["ssn-0", "ssn-2"] {
        assert!(storage.get_session_ptr(id.into()).is_ok());
    }

    let sessions = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(
        sessions
            .iter()
            .filter(|session| session.status.state == SessionState::Closed)
            .count(),
        1
    );

    let open_count = sessions
        .iter()
        .filter(|s| s.status.state == SessionState::Open)
        .count();
    assert_eq!(open_count, 2);
}

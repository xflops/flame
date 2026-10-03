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

use common::apis::{
    ApplicationAttributes, ApplicationFilter, Node, ResourceRequirement, SessionAttributes,
    SessionFilter, SessionState, TaskState,
};
use common::ctx::{FlameCluster, FlameClusterContext};
use common::storage;
use uuid::Uuid;

fn context() -> FlameClusterContext {
    FlameClusterContext {
        cluster: FlameCluster {
            storage: "none".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn names_are_scoped_by_workspace_and_metadata_ids_are_uuids() {
    let store = storage::new_ptr(&context()).await.unwrap();
    assert!(store.workspace_exists("default").unwrap());
    store.create_workspace("team-a".into()).await.unwrap();
    store.create_workspace("team-b".into()).await.unwrap();
    store
        .register_node(&Node {
            name: "worker".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    for workspace in ["team-a", "team-b"] {
        let app = store
            .register_application(
                workspace.into(),
                "service".into(),
                ApplicationAttributes::default(),
            )
            .await
            .unwrap();
        assert_eq!(app.workspace, workspace);
        Uuid::parse_str(&app.id).unwrap();
        let session = store
            .create_session(SessionAttributes {
                workspace: workspace.into(),
                name: "run".into(),
                application: "service".into(),
                resreq: Some(ResourceRequirement::default()),
                ..Default::default()
            })
            .await
            .unwrap();
        Uuid::parse_str(&session.id).unwrap();
        let task = store
            .create_task(workspace, "run", None, None)
            .await
            .unwrap();
        assert_eq!(task.name, 1);
        assert_eq!(task.workspace, workspace);
        assert_eq!(task.session, "run");
        Uuid::parse_str(&task.id).unwrap();
        assert_eq!(store.get_task(workspace, "run", "1").unwrap().id, task.id);
        let executor = store
            .create_executor("worker".into(), workspace, "run")
            .await
            .unwrap();
        assert_eq!(executor.workspace, workspace);
        assert_eq!(executor.application, "service");
        Uuid::parse_str(&executor.id).unwrap();
    }

    for workspace in ["team-a", "team-b"] {
        let filter = SessionFilter::new(workspace)
            .by_state(SessionState::Open)
            .by_names(vec!["run".to_string()])
            .by_application("service");
        let applications = store
            .list_applications(&ApplicationFilter::new(workspace))
            .await
            .unwrap();
        assert_eq!(applications.len(), 1);
        assert_eq!(applications[0].workspace, workspace);
        assert_eq!(applications[0].name, "service");
        let sessions = store.list_sessions(&filter).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].workspace, workspace);
        assert_eq!(sessions[0].name, "run");
    }
    assert!(store
        .list_sessions(&SessionFilter::new("default"))
        .unwrap()
        .is_empty());

    assert_ne!(
        store.get_application("team-a", "service").await.unwrap().id,
        store.get_application("team-b", "service").await.unwrap().id
    );
    assert_ne!(
        store.get_session("team-a", "run").unwrap().id,
        store.get_session("team-b", "run").unwrap().id
    );
    let wrong_session = store.get_session_ptr("team-b", "run").unwrap();
    let task = store.get_task_ptr("team-a", "run", "1").unwrap();
    assert!(store
        .update_task_state(wrong_session, task, TaskState::Running, None)
        .await
        .is_err());
    assert_eq!(
        store.get_task("team-a", "run", "1").unwrap().state,
        TaskState::Pending
    );
    assert!(store
        .register_application(
            "team-a".into(),
            "service".into(),
            ApplicationAttributes::default()
        )
        .await
        .is_err());
    assert!(store
        .create_session(SessionAttributes {
            workspace: "team-a".into(),
            name: "run".into(),
            application: "service".into(),
            ..Default::default()
        })
        .await
        .is_err());
}

#[tokio::test]
async fn session_application_must_exist_in_same_workspace() {
    let store = storage::new_ptr(&context()).await.unwrap();
    store.create_workspace("team-a".into()).await.unwrap();
    store.create_workspace("team-b".into()).await.unwrap();
    store
        .register_application(
            "team-a".into(),
            "service".into(),
            ApplicationAttributes::default(),
        )
        .await
        .unwrap();
    let result = store
        .create_session(SessionAttributes {
            workspace: "team-b".into(),
            name: "run".into(),
            application: "service".into(),
            ..Default::default()
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
#[allow(deprecated)]
async fn node_registration_assigns_and_preserves_uuid_metadata() {
    let store = storage::new_ptr(&context()).await.unwrap();
    let node = Node {
        id: "untrusted-id".into(),
        name: "worker".into(),
        ..Default::default()
    };
    store.register_node(&node).await.unwrap();
    let first = store.get_node("worker").unwrap().unwrap();
    Uuid::parse_str(&first.id).unwrap();
    assert_ne!(first.id, node.id);

    store.register_node(&node).await.unwrap();
    assert_eq!(store.get_node("worker").unwrap().unwrap().id, first.id);

    let heartbeat = Node {
        name: "heartbeat-worker".into(),
        ..node.clone()
    };
    store.sync_node(&heartbeat, &Vec::new()).await.unwrap();
    Uuid::parse_str(&store.get_node("heartbeat-worker").unwrap().unwrap().id).unwrap();
}

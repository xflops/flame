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

use common::apis::{ApplicationAttributes, SessionAttributes};
use common::ctx::FlameClusterContext;
use common::storage;
use uuid::Uuid;

#[tokio::test]
async fn sqlite_uses_names_scoped_by_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("FLAME_TEST_DIR", dir.path());
    let mut config = FlameClusterContext::default();
    config.cluster.storage = format!("sqlite://{}", dir.path().join("flame.db").display());
    let storage = storage::new_ptr(&config).await.unwrap();
    storage.create_workspace("other".to_string()).await.unwrap();

    let mut task_ids = Vec::new();
    for workspace in ["default", "other"] {
        let app = storage
            .register_application(
                workspace.into(),
                "app".into(),
                ApplicationAttributes::default(),
            )
            .await
            .unwrap();
        assert_eq!(app.name, "app");
        assert_eq!(Uuid::parse_str(&app.id).unwrap().get_version_num(), 4);
        let session = storage
            .create_session(SessionAttributes {
                workspace: workspace.into(),
                name: "shared".into(),
                application: "app".into(),
                ..SessionAttributes::default()
            })
            .await
            .unwrap();
        assert_eq!(session.name, "shared");
        assert_eq!(Uuid::parse_str(&session.id).unwrap().get_version_num(), 4);
        let task = storage
            .create_task(workspace, "shared", None, None)
            .await
            .unwrap();
        assert_eq!(task.name, 1);
        assert_eq!(task.session, "shared");
        assert_eq!(Uuid::parse_str(&task.id).unwrap().get_version_num(), 4);
        task_ids.push(task.id);
    }
    assert_ne!(task_ids[0], task_ids[1]);
    assert_eq!(storage.list_workspaces().unwrap().len(), 2);
    assert_eq!(
        storage.get_task("default", "shared", "1").unwrap().id,
        task_ids[0]
    );
    assert_eq!(
        storage.get_task("other", "shared", "1").unwrap().id,
        task_ids[1]
    );
    drop(storage);

    let recovered = storage::new_ptr(&config).await.unwrap();
    recovered.load_data().await.unwrap();
    assert_eq!(recovered.list_workspaces().unwrap().len(), 2);
    assert_eq!(
        recovered.get_task("default", "shared", "1").unwrap().id,
        task_ids[0]
    );
    assert_eq!(
        recovered.get_task("other", "shared", "1").unwrap().id,
        task_ids[1]
    );
}

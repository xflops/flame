/*
Copyright 2025 The xflops Authors.
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
    use std::sync::Arc;

    use stdng::lock_ptr;

    use crate::apis::{
        ApplicationAttributes, SessionAttributes, SessionState, TaskResult, TaskState,
    };
    use crate::ctx::{FlameCluster, FlameClusterContext};
    use crate::events::MemoryEventManager;
    use crate::storage::{self, engine, Storage};
    use crate::FlameError;

    async fn test_storage(url: &str) -> Storage {
        let context = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut storage = (*storage::new_ptr(&context).await.unwrap()).clone();
        storage.engine = engine::connect(url).await.unwrap();
        storage.event_manager = Arc::new(MemoryEventManager::new());
        storage
    }

    fn urls(dir: &std::path::Path) -> [String; 3] {
        [
            "none".to_string(),
            format!("sqlite://{}", dir.join("suspend.db").display()),
            format!("fs://{}", dir.join("filesystem").display()),
        ]
    }

    fn attr(id: &str) -> SessionAttributes {
        SessionAttributes {
            id: id.to_string(),
            application: "app".to_string(),
            min_instances: 1,
            batch_size: 1,
            ..Default::default()
        }
    }

    /// Pop the next pending task the way `Controller::wait_for_task` does.
    fn pop(storage: &Storage, id: &str) -> crate::apis::TaskPtr {
        let ssn = storage.get_session_ptr(id.to_string()).unwrap();
        let mut ssn = lock_ptr!(ssn).unwrap();
        ssn.pop_pending_task().unwrap()
    }

    fn assert_invalid_state<T: std::fmt::Debug>(result: Result<T, FlameError>) {
        assert!(
            matches!(result, Err(FlameError::InvalidState(_))),
            "expected InvalidState, got {result:?}"
        );
    }

    async fn set(
        storage: &Storage,
        id: &str,
        state: SessionState,
    ) -> Result<crate::apis::Session, FlameError> {
        storage.update_session_state(id.into(), state).await
    }

    async fn suspend(storage: &Storage, id: &str) -> Result<crate::apis::Session, FlameError> {
        set(storage, id, SessionState::Suspending).await
    }

    async fn complete(storage: &Storage, id: &str) -> Result<crate::apis::Session, FlameError> {
        set(storage, id, SessionState::Suspended).await
    }

    async fn resume(storage: &Storage, id: &str) -> Result<crate::apis::Session, FlameError> {
        set(storage, id, SessionState::Open).await
    }

    #[tokio::test]
    async fn update_session_state_sets_the_state_until_the_session_closes() {
        let dir = tempfile::tempdir().unwrap();
        for url in urls(dir.path()) {
            let storage = test_storage(&url).await;
            storage.create_session(attr("s")).await.unwrap();

            for state in [
                SessionState::Suspending,
                SessionState::Suspended,
                SessionState::Open,
                SessionState::Suspending,
                SessionState::Open,
            ] {
                let ssn = set(&storage, "s", state).await.unwrap();
                assert_eq!(ssn.status.state, state, "{url}");
                assert_eq!(storage.get_session("s".into()).unwrap().status.state, state);
                // The none engine keeps nothing to read back.
                if url != "none" {
                    let persisted = storage.engine.get_session("s".into()).await.unwrap();
                    assert_eq!(persisted.status.state, state, "{url}");
                }
            }

            assert!(matches!(
                suspend(&storage, "nope").await,
                Err(FlameError::NotFound(_))
            ));
            let engine = &storage.engine;
            assert!(matches!(
                engine
                    .update_session_state("nope".into(), SessionState::Suspending)
                    .await,
                Err(FlameError::NotFound(_))
            ));

            // Closed is terminal: refused, and nothing is written.
            storage.close_session("s".into()).await.unwrap();
            for state in [
                SessionState::Open,
                SessionState::Suspending,
                SessionState::Suspended,
            ] {
                assert_invalid_state(set(&storage, "s", state).await);
                assert_invalid_state(engine.update_session_state("s".into(), state).await);
            }
            assert!(
                storage.get_session("s".into()).unwrap().is_closed(),
                "{url}"
            );
            if url != "none" {
                assert!(
                    engine.get_session("s".into()).await.unwrap().is_closed(),
                    "{url}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_failed_persist_reverts_the_cached_state() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("revert.db").display());
        let storage = test_storage(&url).await;
        storage.create_session(attr("s")).await.unwrap();
        // The row vanishes behind the cache: the engine reports a failure other than
        // NotFound once its table is gone.
        sqlx::query("DROP TABLE sessions")
            .execute(&sqlx::SqlitePool::connect(&url).await.unwrap())
            .await
            .unwrap();
        assert!(suspend(&storage, "s").await.is_err());
        assert!(storage.get_session("s".into()).unwrap().is_open());
    }

    #[tokio::test]
    async fn close_wins_from_suspending_and_suspended() {
        let dir = tempfile::tempdir().unwrap();
        for url in urls(dir.path()) {
            let storage = test_storage(&url).await;
            storage.create_session(attr("suspended")).await.unwrap();
            storage.create_session(attr("suspending")).await.unwrap();

            suspend(&storage, "suspended").await.unwrap();
            complete(&storage, "suspended").await.unwrap();
            let ssn = storage.close_session("suspended".into()).await.unwrap();
            assert!(ssn.is_closed(), "{url}");
            assert_invalid_state(resume(&storage, "suspended").await);

            suspend(&storage, "suspending").await.unwrap();
            let ssn = storage.close_session("suspending".into()).await.unwrap();
            assert!(ssn.is_closed(), "{url}");
            // The completion of a closed session is stale.
            assert_invalid_state(complete(&storage, "suspending").await);
        }
    }

    #[tokio::test]
    async fn suspend_with_a_running_task_lets_it_finish() {
        let dir = tempfile::tempdir().unwrap();
        for url in urls(dir.path()) {
            let storage = test_storage(&url).await;
            storage.create_session(attr("s")).await.unwrap();
            storage.create_task("s".into(), None, None).await.unwrap();
            storage.create_task("s".into(), None, None).await.unwrap();
            let ssn_ptr = storage.get_session_ptr("s".into()).unwrap();

            let task_ptr = pop(&storage, "s");
            storage
                .update_task_state(ssn_ptr.clone(), task_ptr, TaskState::Running, None)
                .await
                .unwrap();
            let ssn = suspend(&storage, "s").await.unwrap();
            assert!(ssn.is_suspending(), "{url}");

            // The running task still completes while the session is suspending.
            storage
                .update_task_result(
                    ssn_ptr.clone(),
                    storage
                        .get_task_ptr(crate::apis::TaskGID {
                            ssn_id: "s".to_string(),
                            task_id: 1,
                        })
                        .unwrap(),
                    TaskResult {
                        state: TaskState::Succeed,
                        output: None,
                        message: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                storage.get_task("s".into(), 1).unwrap().state,
                TaskState::Succeed,
                "{url}"
            );
            // The other task is still pending and waits for the resume.
            assert_eq!(
                storage.get_task("s".into(), 2).unwrap().state,
                TaskState::Pending
            );
        }
    }

    #[tokio::test]
    async fn create_task_is_accepted_until_the_session_closes() {
        let dir = tempfile::tempdir().unwrap();
        for url in urls(dir.path()) {
            let storage = test_storage(&url).await;
            storage.create_session(attr("s")).await.unwrap();

            suspend(&storage, "s").await.unwrap();
            let task = storage.create_task("s".into(), None, None).await.unwrap();
            assert_eq!((task.id, task.state), (1, TaskState::Pending), "{url}");
            complete(&storage, "s").await.unwrap();
            let task = storage.create_task("s".into(), None, None).await.unwrap();
            assert_eq!((task.id, task.state), (2, TaskState::Pending), "{url}");
            assert_eq!(
                storage.get_task("s".into(), 2).unwrap().state,
                TaskState::Pending
            );

            // They run after the resume.
            resume(&storage, "s").await.unwrap();
            pop(&storage, "s");

            // Closed rejects and writes nothing.
            storage.close_session("s".into()).await.unwrap();
            let before = storage.engine.find_tasks("s".into()).await.unwrap().len();
            assert_invalid_state(storage.create_task("s".into(), None, None).await);
            assert_eq!(
                storage.engine.find_tasks("s".into()).await.unwrap().len(),
                before,
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn scheduler_snapshot_leaves_out_suspended_sessions_only() {
        let storage = test_storage("none").await;
        for id in ["open", "suspending", "suspended"] {
            storage.create_session(attr(id)).await.unwrap();
        }
        suspend(&storage, "suspending").await.unwrap();
        suspend(&storage, "suspended").await.unwrap();
        complete(&storage, "suspended").await.unwrap();

        let ids = |storage: &Storage| {
            let mut ids = storage
                .snapshot()
                .unwrap()
                .sessions
                .into_iter()
                .map(|ssn| ssn.id)
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        assert_eq!(ids(&storage), ["open", "suspending"]);
        assert_eq!(storage.list_sessions(None).unwrap().len(), 3);

        resume(&storage, "suspended").await.unwrap();
        assert_eq!(ids(&storage), ["open", "suspended", "suspending"]);
    }

    #[tokio::test]
    async fn open_session_does_not_resurrect_or_replace_a_suspended_session() {
        let storage = test_storage("none").await;
        let spec = attr("s");
        storage.create_session(spec.clone()).await.unwrap();
        suspend(&storage, "s").await.unwrap();
        assert_invalid_state(storage.open_session("s".into(), Some(spec.clone())).await);
        complete(&storage, "s").await.unwrap();
        assert_invalid_state(storage.open_session("s".into(), Some(spec)).await);
        assert_invalid_state(storage.open_session("s".into(), None).await);
        let ssn = storage.get_session("s".into()).unwrap();
        assert!(ssn.is_suspended());
    }

    #[tokio::test]
    async fn open_session_rejects_a_suspended_session_in_the_engines() {
        let dir = tempfile::tempdir().unwrap();
        for url in &urls(dir.path())[1..] {
            let storage = test_storage(url).await;
            let spec = attr("s");
            storage.create_session(spec.clone()).await.unwrap();
            suspend(&storage, "s").await.unwrap();
            assert_invalid_state(storage.engine.open_session("s".into(), Some(spec)).await);
        }
    }

    #[tokio::test]
    async fn live_sessions_block_an_application_update() {
        let dir = tempfile::tempdir().unwrap();
        // The none engine keeps no session index to check an update against.
        for url in &urls(dir.path())[1..] {
            let storage = test_storage(url).await;
            storage
                .register_application("app".to_string(), ApplicationAttributes::default())
                .await
                .unwrap();
            storage.create_session(attr("s")).await.unwrap();

            suspend(&storage, "s").await.unwrap();
            assert!(
                storage
                    .update_application("app".to_string(), ApplicationAttributes::default())
                    .await
                    .is_err(),
                "suspending: {url}"
            );
            complete(&storage, "s").await.unwrap();
            assert!(
                storage
                    .update_application("app".to_string(), ApplicationAttributes::default())
                    .await
                    .is_err(),
                "suspended: {url}"
            );

            storage.close_session("s".into()).await.unwrap();
            storage
                .update_application("app".to_string(), ApplicationAttributes::default())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn restart_keeps_suspending_and_suspended_sessions() {
        let dir = tempfile::tempdir().unwrap();
        for url in &urls(dir.path())[1..] {
            let storage = test_storage(url).await;
            storage.create_session(attr("suspending")).await.unwrap();
            storage.create_session(attr("suspended")).await.unwrap();
            storage
                .create_task("suspending".into(), None, None)
                .await
                .unwrap();
            storage
                .create_task("suspended".into(), None, None)
                .await
                .unwrap();

            suspend(&storage, "suspending").await.unwrap();
            suspend(&storage, "suspended").await.unwrap();
            complete(&storage, "suspended").await.unwrap();

            let restarted = test_storage(url).await;
            restarted.load_data().await.unwrap();

            let ssn = restarted.get_session("suspending".into()).unwrap();
            // The scheduler completes it once no executor serves the session.
            assert!(ssn.is_suspending(), "{url}");
            assert_eq!(restarted.list_tasks("suspending".into()).unwrap().len(), 1);
            let persisted = restarted
                .engine
                .get_session("suspending".into())
                .await
                .unwrap();
            assert!(persisted.is_suspending(), "{url}");

            let ssn = restarted.get_session("suspended".into()).unwrap();
            assert!(ssn.is_suspended(), "{url}");
            // Its pending task survives the restart.
            assert_eq!(
                restarted.get_task("suspended".into(), 1).unwrap().state,
                TaskState::Pending,
                "{url}"
            );
            let ssn = resume(&restarted, "suspended").await.unwrap();
            assert!(ssn.is_open());

            // A second restart is stable and nothing is evicted.
            let again = test_storage(url).await;
            again.load_data().await.unwrap();
            assert!(again.get_session("suspended".into()).unwrap().is_open());
            assert!(again
                .get_session("suspending".into())
                .unwrap()
                .is_suspending());
        }
    }
}

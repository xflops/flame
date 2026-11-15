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

use super::*;
use crate::events::EventManager;

fn event(message: &str) -> Event {
    Event {
        code: 1,
        message: Some(message.to_string()),
        creation_time: Utc::now(),
    }
}

async fn test_storage(url: &str, events: EventManagerPtr) -> Storage {
    let context = FlameClusterContext {
        cluster: crate::ctx::FlameCluster {
            storage: "none".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut storage = (*new_ptr(&context).await.unwrap()).clone();
    storage.engine = engine::connect(url).await.unwrap();
    storage.context.cluster.storage = url.to_string();
    storage.event_manager = events;
    storage
}

fn test_urls(dir: &std::path::Path) -> [String; 2] {
    [
        "none".to_string(),
        format!("fs://{}", dir.join("filesystem").display()),
    ]
}

/// OpenSession must initialize event storage just like CreateSession does.
#[tokio::test]
async fn open_session_records_session_and_task_events() {
    let engine_dir = tempfile::tempdir().unwrap();
    for url in test_urls(engine_dir.path()) {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());
        let storage = test_storage(&url, events.clone()).await;
        let id = "opened-events".to_string();
        storage
            .open_session(
                id.clone(),
                Some(SessionAttributes {
                    id: id.clone(),
                    application: "test-app".to_string(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        for task_id in [0, 99] {
            let owner = EventOwner {
                session_id: id.clone(),
                task_id,
            };
            storage
                .record_event(owner.clone(), event("opened"))
                .await
                .unwrap();
            assert_eq!(events.find_events(owner).unwrap().len(), 1, "{url}");
        }
        let retained = storage.get_session_ptr(id.clone()).unwrap();
        let task = storage.create_task(id.clone(), None, None).await.unwrap();
        storage.open_session(id.clone(), None).await.unwrap();
        assert!(Arc::ptr_eq(
            &retained,
            &storage.get_session_ptr(id.clone()).unwrap()
        ));
        assert!(lock_ptr!(retained)
            .unwrap()
            .active_task(task.id)
            .unwrap()
            .is_some());
        let task_ptr = storage.get_task_ptr(task.gid()).unwrap();
        for state in [TaskState::Running, TaskState::Succeed] {
            storage
                .update_task_state(
                    storage.get_session_ptr(id.clone()).unwrap(),
                    task_ptr.clone(),
                    state,
                    None,
                )
                .await
                .unwrap();
        }
        let recorded = storage.get_task(id.clone(), task.id).await.unwrap().events;
        let states: Vec<i32> = recorded.iter().map(|event| event.code).collect();
        assert_eq!(
            states,
            vec![
                i32::from(TaskState::Pending),
                i32::from(TaskState::Running),
                i32::from(TaskState::Succeed),
            ],
            "{url}"
        );
        assert_eq!(
            events.find_events(EventOwner::session(id)).unwrap().len(),
            1
        );
    }
}

/// A delayed opener must preserve tasks published by a competing opener,
/// and must not recreate event files after close wins the publication race.
#[tokio::test]
async fn delayed_open_preserves_cached_tasks_and_observes_close() {
    let engine_dir = tempfile::tempdir().unwrap();
    for url in test_urls(engine_dir.path()) {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());
        let storage = test_storage(&url, events).await;
        let id = "delayed-open".to_string();
        // Pause the first OpenSession after the engine returns its snapshot.
        let snapshot = storage
            .engine
            .open_session(
                id.clone(),
                Some(SessionAttributes {
                    id: id.clone(),
                    application: "test-app".to_string(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        storage.open_session(id.clone(), None).await.unwrap();
        let retained = storage.get_session_ptr(id.clone()).unwrap();
        let task = storage.create_task(id.clone(), None, None).await.unwrap();
        // Resume publication after another opener has admitted a task.
        storage.cache_session(snapshot.clone(), None).unwrap();
        assert!(Arc::ptr_eq(
            &retained,
            &storage.get_session_ptr(id.clone()).unwrap()
        ));
        assert!(lock_ptr!(retained)
            .unwrap()
            .active_task(task.id)
            .unwrap()
            .is_some());

        storage.close_session(id.clone()).await.unwrap();
        assert!(!dir.path().join(&id).exists());
        // The same stale engine result cannot override Closed or setup events.
        assert!(matches!(
            storage.cache_session(snapshot, None),
            Err(FlameError::InvalidState(_))
        ));
        assert!(!dir.path().join(&id).exists());
        assert_eq!(
            storage.get_session(id).unwrap().status.state,
            SessionState::Closed
        );
    }
}

/// Checks publication keeps the session locked while setup can access the map.
struct SetupLockCheck {
    sessions: MutexPtr<HashMap<SessionID, SessionPtr>>,
    inner: MemoryEventManager,
    calls: std::sync::atomic::AtomicUsize,
}

impl EventManager for SetupLockCheck {
    /// Checks the fresh pointer is published with its mutex already held.
    fn setup_events(&self, id: SessionID) -> Result<(), FlameError> {
        let ptr = {
            let sessions = self.sessions.try_lock().expect("map held during event IO");
            sessions
                .get(&id)
                .expect("session was not published")
                .clone()
        };
        assert!(ptr.try_lock().is_none(), "unlocked session during setup");
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.setup_events(id)
    }

    /// Delegates diagnostic writes to memory.
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        self.inner.record_event(owner, event)
    }

    /// Delegates diagnostic reads to memory.
    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError> {
        self.inner.find_events(owner)
    }

    /// Delegates cleanup to memory.
    fn remove_events(&self, id: SessionID) -> Result<(), FlameError> {
        self.inner.remove_events(id)
    }

    /// Delegates reset to memory.
    fn clear(&self) -> Result<(), FlameError> {
        self.inner.clear()
    }
}

/// Fresh publication and cached reopen both serialize setup using only
/// the individual session mutex, leaving unrelated session lookups free.
#[tokio::test]
async fn open_session_event_setup_holds_only_its_session_lock() {
    let mut storage = test_storage("none", Arc::new(MemoryEventManager::new())).await;
    let events = Arc::new(SetupLockCheck {
        sessions: storage.sessions.clone(),
        inner: MemoryEventManager::new(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    storage.event_manager = events.clone();
    let id = "setup-lock".to_string();
    storage
        .open_session(
            id.clone(),
            Some(SessionAttributes {
                id: id.clone(),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    storage.open_session(id, None).await.unwrap();
    assert_eq!(events.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn close_removes_session_and_task_events_and_skips_late_writes() {
    let engine_dir = tempfile::tempdir().unwrap();
    for url in test_urls(engine_dir.path()) {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());
        let storage = test_storage(&url, events).await;
        let session = storage
            .create_session(SessionAttributes {
                id: "close-events".to_string(),
                application: "test-app".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let owner = EventOwner::session(session.id.clone());
        storage
            .record_event(owner.clone(), event("session"))
            .await
            .unwrap();
        storage
            .record_event(
                EventOwner {
                    session_id: session.id.clone(),
                    task_id: 1,
                },
                event("task"),
            )
            .await
            .unwrap();
        assert_eq!(
            storage
                .get_session(session.id.clone())
                .unwrap()
                .events
                .len(),
            1
        );

        let closed = storage.close_session(session.id.clone()).await.unwrap();
        assert_eq!(closed.status.state, SessionState::Closed);
        assert!(closed.events.is_empty());
        assert!(storage
            .get_session(session.id.clone())
            .unwrap()
            .events
            .is_empty());
        assert!(!dir.path().join(&session.id).exists());
        storage.record_event(owner, event("late")).await.unwrap();
        assert!(!dir.path().join(&session.id).exists());
    }
}

#[tokio::test]
async fn failed_close_preserves_open_session_events() {
    let engine_dir = tempfile::tempdir().unwrap();
    for url in test_urls(engine_dir.path()) {
        let dir = tempfile::tempdir().unwrap();
        let storage = test_storage(
            &url,
            Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap()),
        )
        .await;
        let id = "running-events".to_string();
        storage
            .create_session(SessionAttributes {
                id: id.clone(),
                application: "test-app".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let owner = EventOwner::session(id.clone());
        storage
            .record_event(owner.clone(), event("before"))
            .await
            .unwrap();
        let task = storage.create_task(id.clone(), None, None).await.unwrap();
        storage
            .update_task_state(
                storage.get_session_ptr(id.clone()).unwrap(),
                storage.get_task_ptr(task.gid()).unwrap(),
                TaskState::Running,
                None,
            )
            .await
            .unwrap();
        let previous = storage
            .get_task(id.clone(), task.id)
            .await
            .unwrap()
            .events
            .len();
        assert!(storage.close_session(id.clone()).await.is_err());
        let session = storage.get_session(id.clone()).unwrap();
        assert_eq!(session.status.state, SessionState::Open);
        assert_eq!(session.events[0].message.as_deref(), Some("before"));
        assert_eq!(
            storage
                .get_task(id.clone(), task.id)
                .await
                .unwrap()
                .events
                .len(),
            previous
        );
        assert!(dir.path().join(&id).exists());
        storage.record_event(owner, event("after")).await.unwrap();
        assert_eq!(storage.get_session(id).unwrap().events.len(), 2);
    }
}

#[tokio::test]
async fn delete_drops_events_from_writers_with_a_retained_cache_pointer() {
    let engine_dir = tempfile::tempdir().unwrap();
    for url in test_urls(engine_dir.path()) {
        let events = Arc::new(MemoryEventManager::new());
        let storage = test_storage(&url, events.clone()).await;
        let id = "delete-events".to_string();
        storage
            .create_session(SessionAttributes {
                id: id.clone(),
                application: "test-app".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let retained = storage.get_session_ptr(id.clone()).unwrap();
        storage
            .record_event(EventOwner::session(id.clone()), event("before"))
            .await
            .unwrap();
        if url != "none" {
            // Persistent engines require Closed before deletion. Keep the
            // retained cache pointer Open to model a delayed cache writer.
            storage.engine.close_session(id.clone()).await.unwrap();
        }
        storage.delete_session(id.clone()).await.unwrap();
        assert_eq!(
            lock_ptr!(retained).unwrap().status.state,
            SessionState::Closed
        );
        assert!(matches!(
            storage
                .record_event(EventOwner::session(id.clone()), event("late"))
                .await,
            Err(FlameError::NotFound(_))
        ));
        assert!(events
            .find_events(EventOwner::session(id))
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn recovery_cleans_closed_events_and_keeps_open_events() {
    let engine_dir = tempfile::tempdir().unwrap();
    for url in test_urls(engine_dir.path()).into_iter().skip(1) {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());
        let storage = test_storage(&url, events).await;
        for id in ["closed", "open"] {
            storage
                .create_session(SessionAttributes {
                    id: id.to_string(),
                    application: "test-app".to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
            storage
                .record_event(EventOwner::session(id.to_string()), event(id))
                .await
                .unwrap();
        }
        // Model an older server or a crash after persisting Closed but before
        // releasing its event files.
        storage
            .engine
            .close_session("closed".to_string())
            .await
            .unwrap();
        let recovered = Storage {
            sessions: stdng::new_ptr(HashMap::new()),
            event_manager: Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap()),
            ..storage.clone()
        };
        recovered.load_data().await.unwrap();
        assert!(recovered
            .get_session("closed".to_string())
            .unwrap()
            .events
            .is_empty());
        assert!(!dir.path().join("closed").exists());
        assert_eq!(
            recovered.get_session("open".to_string()).unwrap().events[0]
                .message
                .as_deref(),
            Some("open")
        );
        assert!(dir.path().join("open").exists());
    }
}

#[tokio::test]
async fn recovery_preserves_running_task_history_in_closed_sessions() {
    let engine_dir = tempfile::tempdir().unwrap();
    let url = test_urls(engine_dir.path())[1].clone();
    let storage = test_storage(&url, Arc::new(MemoryEventManager::new())).await;
    let id = "closed-running-history".to_string();
    storage
        .create_session(SessionAttributes {
            id: id.clone(),
            application: "test-app".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let task = storage.create_task(id.clone(), None, None).await.unwrap();
    storage
        .engine
        .update_task_state(task.clone(), TaskState::Running, None)
        .await
        .unwrap();
    // Model a legacy late writer by persisting Closed while the task record
    // remains Running, then restart the filesystem engine from those files.
    let path = engine_dir
        .path()
        .join("filesystem/sessions")
        .join(&id)
        .join("metadata");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    metadata["state"] = serde_json::json!(SessionState::Closed as i32);
    std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let persisted = storage.engine.get_task(task.gid()).await.unwrap();
    let recovered = Storage {
        engine: engine::connect(&url).await.unwrap(),
        sessions: stdng::new_ptr(HashMap::new()),
        ..storage.clone()
    };
    recovered.load_data().await.unwrap();
    assert_eq!(
        recovered.get_session(id.clone()).unwrap().status.state,
        SessionState::Closed
    );
    let history = recovered.get_task_metadata(id, task.id).await.unwrap();
    assert_eq!(history.state, TaskState::Running);
    assert_eq!(history.version, persisted.version);
    let on_disk = storage.engine.get_task(task.gid()).await.unwrap();
    assert_eq!(on_disk.state, TaskState::Running);
    assert_eq!(on_disk.version, persisted.version);
}

struct CleanupFailure(MemoryEventManager);

impl EventManager for CleanupFailure {
    fn setup_events(&self, id: SessionID) -> Result<(), FlameError> {
        self.0.setup_events(id)
    }
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        self.0.record_event(owner, event)
    }
    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError> {
        self.0.find_events(owner)
    }
    fn remove_events(&self, _id: SessionID) -> Result<(), FlameError> {
        Err(FlameError::Storage("cleanup failed".to_string()))
    }
    fn clear(&self) -> Result<(), FlameError> {
        self.0.clear()
    }
}

#[tokio::test]
async fn cleanup_failure_keeps_cached_session_closed() {
    let storage = test_storage("none", Arc::new(CleanupFailure(MemoryEventManager::new()))).await;
    storage
        .create_session(SessionAttributes {
            id: "failure".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    // Event cleanup failure is logged, not returned.
    storage.close_session("failure".to_string()).await.unwrap();
    assert_eq!(
        storage
            .get_session("failure".to_string())
            .unwrap()
            .status
            .state,
        SessionState::Closed
    );
    storage
        .record_event(EventOwner::session("failure".to_string()), event("late"))
        .await
        .unwrap();
    assert!(storage
        .get_session("failure".to_string())
        .unwrap()
        .events
        .is_empty());
}

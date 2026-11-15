/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use super::*;
use crate::apis::TaskCounts;
use crate::events::EventManager;
use bytes::Bytes;

async fn storage(url: &str) -> Storage {
    let config = FlameClusterContext {
        cluster: crate::ctx::FlameCluster {
            storage: "none".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut storage = (*new_ptr(&config).await.unwrap()).clone();
    storage.engine = engine::connect(url).await.unwrap();
    storage.context.cluster.storage = url.to_string();
    storage
}

fn persistent_urls(dir: &std::path::Path) -> [String; 1] {
    [format!("fs://{}", dir.join("history").display())]
}

async fn session(storage: &Storage) -> SessionPtr {
    storage
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    storage
        .create_session(SessionAttributes {
            id: "s".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage.get_session_ptr("s".into()).unwrap()
}

#[tokio::test]
async fn persistent_terminal_eviction_preserves_results_and_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    for url in persistent_urls(dir.path()) {
        let storage = storage(&url).await;
        let ssn = session(&storage).await;
        for state in [TaskState::Succeed, TaskState::Failed, TaskState::Cancelled] {
            let created = storage
                .create_task("s".into(), Some(Bytes::from_static(b"input")), None)
                .await
                .unwrap();
            let task = storage.get_task_ptr(created.gid()).unwrap();
            if state == TaskState::Cancelled {
                storage
                    .update_task_state(ssn.clone(), task.clone(), state, None)
                    .await
                    .unwrap();
            } else {
                storage
                    .update_task_result(
                        ssn.clone(),
                        task.clone(),
                        TaskResult {
                            state,
                            output: Some(Bytes::from_static(b"result")),
                            message: Some("detail".into()),
                        },
                    )
                    .await
                    .unwrap();
            }
            assert!(storage.get_task_ptr(created.gid()).is_err());
            let history = storage.get_task("s".into(), created.id).await.unwrap();
            assert_eq!(history.state, state);
            assert_eq!(history.input, created.input);
            assert!(!history.events.is_empty());
            let event_count = history.events.len();
            // Duplicate and conflicting result must preserve both counts and original events.
            storage
                .update_task_result(
                    ssn.clone(),
                    task,
                    TaskResult {
                        state: TaskState::Failed,
                        output: Some(Bytes::from_static(b"replacement")),
                        message: None,
                    },
                )
                .await
                .unwrap();
            let duplicate = storage.get_task("s".into(), created.id).await.unwrap();
            assert_eq!(duplicate.state, state);
            assert_eq!(duplicate.output, history.output);
            assert_eq!(duplicate.events.len(), event_count);
            let mut delayed = created;
            delayed.version = duplicate.version + 1;
            storage
                .update_task_state(
                    ssn.clone(),
                    stdng::new_ptr(delayed),
                    TaskState::Running,
                    None,
                )
                .await
                .unwrap();
        }
        let cached = lock_ptr!(ssn).unwrap();
        assert!(cached.tasks.is_empty());
        assert!(cached.tasks_index.values().all(|index| index.is_empty()));
        assert_eq!(
            cached.task_counts,
            TaskCounts {
                succeed: 1,
                failed: 1,
                cancelled: 1,
                ..Default::default()
            }
        );
    }
}

#[tokio::test]
async fn executor_cleanup_does_not_retry_evicted_tasks() {
    let dir = tempfile::tempdir().unwrap();
    for url in persistent_urls(dir.path()) {
        let storage = storage(&url).await;
        let ssn = session(&storage).await;
        lock_ptr!(ssn).unwrap().resreq = Some(Default::default());
        storage
            .register_node(&crate::apis::Node {
                name: "node".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let created = storage.create_task("s".into(), None, None).await.unwrap();
        let task = storage.get_task_ptr(created.gid()).unwrap();
        storage
            .update_task_result(
                ssn,
                task,
                TaskResult {
                    state: TaskState::Succeed,
                    output: Some(Bytes::from_static(b"result")),
                    message: None,
                },
            )
            .await
            .unwrap();
        let history = storage.get_task("s".into(), created.id).await.unwrap();
        for session_id in ["s", "missing"] {
            let mut executor = storage
                .create_executor("node".into(), "s".into())
                .await
                .unwrap();
            executor.ssn_id = Some(session_id.into());
            executor.task_id = Some(created.id);
            let deleted = storage
                .delete_executors(std::slice::from_ref(&executor))
                .await
                .unwrap();
            assert_eq!(deleted, vec![executor.id]);
        }
        let mut executor = storage
            .create_executor("node".into(), "s".into())
            .await
            .unwrap();
        executor.ssn_id = Some("s".into());
        executor.task_id = Some(created.id);
        storage.close_session("s".into()).await.unwrap();
        // A stale assignment in a closed session must not block executor deletion.
        let deleted = storage
            .delete_executors(std::slice::from_ref(&executor))
            .await
            .unwrap();
        assert_eq!(deleted, vec![executor.id]);
        let after = storage.get_task("s".into(), created.id).await.unwrap();
        assert_eq!(after.state, history.state);
        assert_eq!(after.version, history.version);
        assert_eq!(after.output, history.output);
    }
}

#[tokio::test]
async fn close_reconciles_terminal_commit_before_cache_publication() {
    let dir = tempfile::tempdir().unwrap();
    for url in persistent_urls(dir.path()) {
        let storage = storage(&url).await;
        let ssn = session(&storage).await;
        let completed = storage.create_task("s".into(), None, None).await.unwrap();
        storage.create_task("s".into(), None, None).await.unwrap();
        storage
            .update_task_state(
                ssn.clone(),
                storage.get_task_ptr(completed.gid()).unwrap(),
                TaskState::Running,
                None,
            )
            .await
            .unwrap();
        // The result commits in storage while its cached state remains Running.
        let update = storage
            .engine
            .update_task_result(
                completed.clone(),
                TaskResult {
                    state: TaskState::Succeed,
                    output: Some(Bytes::from_static(b"won")),
                    message: None,
                },
            )
            .await
            .unwrap();
        let closed = storage.close_session("s".into()).await.unwrap();
        assert_eq!(
            closed.task_counts,
            TaskCounts {
                succeed: 1,
                cancelled: 1,
                ..Default::default()
            }
        );
        assert!(closed.tasks.is_empty());
        assert!(matches!(
            storage
                .update_task_state(ssn, stdng::new_ptr(update), TaskState::Running, None)
                .await,
            Err(FlameError::InvalidState(_))
        ));
        assert_eq!(
            storage.get_session("s".into()).unwrap().task_counts,
            closed.task_counts
        );
        assert_eq!(
            storage
                .get_task_metadata("s".into(), completed.id)
                .await
                .unwrap()
                .output,
            Some(Bytes::from_static(b"won"))
        );
    }
}

#[tokio::test]
async fn recovery_keeps_only_active_tasks_and_rebuilds_counts() {
    let dir = tempfile::tempdir().unwrap();
    for url in persistent_urls(dir.path()) {
        let storage = storage(&url).await;
        let ssn = session(&storage).await;
        for _ in 0..5 {
            let task = storage.create_task("s".into(), None, None).await.unwrap();
            storage
                .update_task_result(
                    ssn.clone(),
                    storage.get_task_ptr(task.gid()).unwrap(),
                    TaskResult {
                        state: TaskState::Succeed,
                        output: Some(Bytes::from(vec![1; 4096])),
                        message: None,
                    },
                )
                .await
                .unwrap();
        }
        let running = storage.create_task("s".into(), None, None).await.unwrap();
        storage
            .update_task_state(
                ssn,
                storage.get_task_ptr(running.gid()).unwrap(),
                TaskState::Running,
                None,
            )
            .await
            .unwrap();
        storage.create_task("s".into(), None, None).await.unwrap();
        let recovered = Storage {
            sessions: stdng::new_ptr(HashMap::new()),
            ..storage.clone()
        };
        recovered.load_data().await.unwrap();
        let restored = recovered.get_session("s".into()).unwrap();
        assert_eq!(restored.tasks.len(), 2);
        assert_eq!(
            restored.task_counts,
            TaskCounts {
                pending: 2,
                succeed: 5,
                ..Default::default()
            }
        );
        assert_eq!(recovered.list_tasks("s".into()).await.unwrap().len(), 7);
        assert_eq!(
            recovered
                .get_task_metadata("s".into(), 1)
                .await
                .unwrap()
                .output
                .unwrap()
                .len(),
            4096
        );
        assert_eq!(recovered.get_session("s".into()).unwrap().tasks.len(), 2);
    }
}

#[tokio::test]
async fn none_listing_keeps_dequeued_tasks_and_counts() {
    let storage = storage("none").await;
    let ssn = session(&storage).await;
    let created = storage.create_task("s".into(), None, None).await.unwrap();
    let task = lock_ptr!(ssn).unwrap().pop_pending_task().unwrap();
    assert_eq!(lock_ptr!(ssn).unwrap().task_counts.pending, 1);
    storage.create_task("s".into(), None, None).await.unwrap();
    let listed = storage.list_tasks("s".into()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, created.id);
    // Preserve a stale external Arc across Running publication: NoneEngine must
    // derive the completion version from its retained authoritative cache.
    storage
        .update_task_state(ssn.clone(), task.clone(), TaskState::Running, None)
        .await
        .unwrap();
    storage
        .update_task_result(
            ssn.clone(),
            task.clone(),
            TaskResult {
                state: TaskState::Succeed,
                output: None,
                message: None,
            },
        )
        .await
        .unwrap();
    let duplicate = storage
        .update_task_result(
            ssn.clone(),
            task,
            TaskResult {
                state: TaskState::Failed,
                output: None,
                message: None,
            },
        )
        .await;
    assert!(duplicate.is_ok());
    assert_eq!(
        storage
            .get_task_metadata("s".into(), created.id)
            .await
            .unwrap()
            .state,
        TaskState::Succeed
    );
    assert!(matches!(
        storage.get_task_ptr(created.gid()),
        Err(FlameError::NotFound(_))
    ));
    let remaining = storage.list_tasks("s".into()).await.unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].state, TaskState::Pending);
    assert_eq!(remaining[1].state, TaskState::Succeed);
    assert_eq!(
        storage.get_session("s".into()).unwrap().task_counts.succeed,
        1
    );
    // The other task is dequeued but remains logically Pending and must be
    // cancelled on close even though it no longer has a queue index entry.
    lock_ptr!(ssn).unwrap().pop_pending_task().unwrap();
    let closed = storage.close_session("s".into()).await.unwrap();
    assert_eq!(closed.task_counts.pending, 0);
    assert_eq!(closed.task_counts.cancelled, 1);
    assert_eq!(closed.task_counts.succeed, 1);
    let repeated = storage.close_session("s".into()).await.unwrap();
    assert_eq!(repeated.task_counts, closed.task_counts);
    assert!(repeated.tasks.is_empty());
}

/// Observe session ownership without acquiring its mutex from inside event I/O.
/// The task cache owns two Arcs (task map and state index) until publication.
struct TerminalEventProbe {
    events: MemoryEventManager,
    task: parking_lot::Mutex<std::sync::Weak<parking_lot::Mutex<Task>>>,
    owners_at_append: std::sync::atomic::AtomicUsize,
}

impl EventManager for TerminalEventProbe {
    fn setup_events(&self, id: SessionID) -> Result<(), FlameError> {
        self.events.setup_events(id)
    }
    fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        if owner.task_id != 0 && event.code == TaskState::Failed as i32 {
            self.owners_at_append.store(
                self.task.lock().strong_count(),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
        self.events.record_event(owner, event)
    }
    fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError> {
        self.events.find_events(owner)
    }
    fn remove_events(&self, id: SessionID) -> Result<(), FlameError> {
        self.events.remove_events(id)
    }
    fn clear(&self) -> Result<(), FlameError> {
        self.events.clear()
    }
}

#[tokio::test]
async fn final_failure_event_is_appended_before_terminal_cache_publication() {
    let dir = tempfile::tempdir().unwrap();
    for url in persistent_urls(dir.path()) {
        let mut storage = storage(&url).await;
        let ssn = session(&storage).await;
        let created = storage.create_task("s".into(), None, None).await.unwrap();
        let task = storage.get_task_ptr(created.gid()).unwrap();
        let probe = Arc::new(TerminalEventProbe {
            events: MemoryEventManager::new(),
            task: parking_lot::Mutex::new(Arc::downgrade(&task)),
            owners_at_append: std::sync::atomic::AtomicUsize::new(0),
        });
        storage.event_manager = probe.clone();
        storage
            .update_task_result(
                ssn,
                task,
                TaskResult {
                    state: TaskState::Failed,
                    output: None,
                    message: Some("final failure detail".into()),
                },
            )
            .await
            .unwrap();
        // A publish-before-event regression leaves only the updating caller's
        // Arc here; both scheduler-owned Arcs must still exist during append.
        assert!(
            probe
                .owners_at_append
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 3
        );
        assert!(storage.get_task_ptr(created.gid()).is_err());
        let terminal = storage.get_task("s".into(), created.id).await.unwrap();
        assert_eq!(
            terminal.events.last().unwrap().message.as_deref(),
            Some("final failure detail")
        );
    }
}

#[tokio::test]
async fn none_history_survives_session_cache_eviction_until_explicit_deletion() {
    let mut storage = storage("none").await;
    storage.max_sessions = Some(1);
    let ssn = session(&storage).await;
    let created = storage.create_task("s".into(), None, None).await.unwrap();
    storage
        .update_task_result(
            ssn,
            storage.get_task_ptr(created.gid()).unwrap(),
            TaskResult {
                state: TaskState::Succeed,
                output: Some(Bytes::from_static(b"memory result")),
                message: None,
            },
        )
        .await
        .unwrap();
    storage.close_session("s".into()).await.unwrap();
    assert!(matches!(
        storage.get_session_ptr("s".into()),
        Err(FlameError::NotFound(_))
    ));
    assert_eq!(
        storage
            .get_task("s".into(), created.id)
            .await
            .unwrap()
            .output,
        Some(Bytes::from_static(b"memory result"))
    );
    assert_eq!(storage.list_tasks("s".into()).await.unwrap().len(), 1);
    storage.delete_session("s".into()).await.unwrap();
    assert!(matches!(
        storage.get_task("s".into(), created.id).await,
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn none_retains_history_after_cache_eviction_and_skips_duplicate_updates() {
    let storage = storage("none").await;
    let ssn = session(&storage).await;
    for state in [TaskState::Succeed, TaskState::Failed, TaskState::Cancelled] {
        let created = storage.create_task("s".into(), None, None).await.unwrap();
        let task = storage.get_task_ptr(created.gid()).unwrap();
        if state == TaskState::Cancelled {
            storage
                .update_task_state(ssn.clone(), task.clone(), state, None)
                .await
                .unwrap();
        } else {
            storage
                .update_task_result(
                    ssn.clone(),
                    task.clone(),
                    TaskResult {
                        state,
                        output: Some(Bytes::from_static(b"retained result")),
                        message: None,
                    },
                )
                .await
                .unwrap();
        }
        let completed = storage.get_task("s".into(), created.id).await.unwrap();
        assert_eq!(completed.state, state);
        if state != TaskState::Cancelled {
            assert_eq!(
                completed.output,
                Some(Bytes::from_static(b"retained result"))
            );
        }
        assert!(matches!(
            storage.get_task_ptr(created.gid()),
            Err(FlameError::NotFound(_))
        ));
        assert!(storage
            .update_task_result(
                ssn.clone(),
                task.clone(),
                TaskResult {
                    state,
                    output: None,
                    message: None,
                }
            )
            .await
            .is_ok());
        assert!(storage
            .update_task_state(ssn.clone(), task, TaskState::Pending, None)
            .await
            .is_ok());
        let after = storage.get_task("s".into(), created.id).await.unwrap();
        assert_eq!(after.state, completed.state);
        assert_eq!(after.version, completed.version);
        assert_eq!(after.output, completed.output);
    }
    let cached = storage.get_session("s".into()).unwrap();
    assert!(cached.tasks.is_empty());
    assert!(cached.tasks_index.values().all(|index| index.is_empty()));
    assert_eq!(
        cached.task_counts,
        TaskCounts {
            succeed: 1,
            failed: 1,
            cancelled: 1,
            ..Default::default()
        }
    );
    assert_eq!(storage.list_tasks("s".into()).await.unwrap().len(), 3);
}

/*
Copyright 2023 The Flame Authors.
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

use std::sync::Arc;

use async_trait::async_trait;

#[cfg(test)]
use crate::apis::TaskCounts;
use crate::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationState, ExecutorID, ExecutorState,
    Node, Session, SessionAttributes, SessionID, SessionState, Task, TaskGID, TaskInput,
    TaskOptions, TaskResult, TaskState,
};
use crate::apis::{ApplicationFilter, Executor, TaskFilter};
use crate::FlameError;

mod filesystem;
mod none;
mod sqlite;
pub mod types;

#[cfg(test)]
pub use sqlite::SqliteEngine;

pub type EnginePtr = Arc<dyn Engine>;

#[async_trait]
pub trait Engine: Send + Sync + 'static {
    // Application operations
    async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError>;
    async fn update_application_state(
        &self,
        id: ApplicationID,
        state: ApplicationState,
    ) -> Result<Application, FlameError>;
    async fn delete_application(&self, id: ApplicationID) -> Result<(), FlameError>;
    async fn update_application(
        &self,
        id: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError>;
    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError>;
    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError>;
    // Session operations
    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError>;
    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError>;
    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError>;
    /// Close a session and return its final task counts after pending cancellation.
    /// Persistent engines derive counts from records; none maintains aggregate counts.
    /// Repeated close returns the closed counts without changing task states.
    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError>;
    /// Set the state of an existing session. `NotFound` if it does not exist;
    /// `InvalidState` (nothing written) if it is `Closed`, which is terminal.
    async fn update_session_state(
        &self,
        id: SessionID,
        state: SessionState,
    ) -> Result<Session, FlameError>;
    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError>;
    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError>;

    // Task operations
    async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError>;

    #[allow(dead_code)]
    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError>;

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError>;

    /// Apply a mutation using the active cache snapshot's identity and return
    /// the engine's updated record.
    async fn update_task_state(
        &self,
        current: Task,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<Task, FlameError>;

    async fn update_task_result(
        &self,
        current: Task,
        task_result: TaskResult,
    ) -> Result<Task, FlameError>;

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>, FlameError>;

    // Node operations
    async fn create_node(&self, node: &Node) -> Result<Node, FlameError>;
    #[allow(dead_code)]
    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError>;
    async fn update_node(&self, node: &Node) -> Result<Node, FlameError>;
    async fn delete_node(&self, name: &str) -> Result<(), FlameError>;
    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError>;

    // Executor operations
    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError>;
    #[allow(dead_code)]
    async fn get_executor(&self, id: &ExecutorID) -> Result<Option<Executor>, FlameError>;
    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError>;
    #[allow(dead_code)]
    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError>;
    async fn delete_executor(&self, id: &ExecutorID) -> Result<(), FlameError>;
    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError>;
}

/// Connect to a storage engine based on the URL scheme.
///
/// Supported URL schemes:
/// - `sqlite://` or `sqlite:` - SQLite database (default)
/// - `filesystem://`, `file://`, `fs://` - Filesystem-based storage
/// - `none` - In-memory only, no persistence (for non-recoverable workloads)
///
/// Path resolution:
/// - Triple slash (e.g., `fs:///data`) - Absolute path (`/data`)
/// - Double slash (e.g., `fs://data`) - Relative to FLAME_HOME (`${FLAME_HOME}/data`)
///
/// # Examples
///
/// ```ignore
/// // SQLite storage
/// let engine = connect("sqlite:///var/lib/flame/sessions.db").await?;
///
/// // Filesystem storage (absolute path)
/// let engine = connect("fs:///var/lib/flame").await?;
///
/// // Filesystem storage (relative to FLAME_HOME)
/// let engine = connect("fs://data").await?;  // -> ${FLAME_HOME}/data
///
/// // None storage (in-memory only, no persistence)
/// let engine = connect("none").await?;
/// ```
pub async fn connect(url: &str) -> Result<EnginePtr, FlameError> {
    if url == "none" {
        none::NoneEngine::new_ptr(url).await
    } else if url.starts_with("filesystem://")
        || url.starts_with("file://")
        || url.starts_with("fs://")
    {
        tracing::info!("Using filesystem storage engine: {}", url);
        filesystem::FilesystemEngine::new_ptr(url).await
    } else {
        tracing::info!("Using SQLite storage engine: {}", url);
        sqlite::SqliteEngine::new_ptr(url).await
    }
}

#[cfg(test)]
fn task_snapshot(gid: TaskGID) -> Task {
    Task {
        ssn_id: gid.ssn_id,
        id: gid.task_id,
        ..Default::default()
    }
}

#[cfg(test)]
mod task_history_tests {
    use super::*;
    use bytes::Bytes;

    async fn engine(kind: &str) -> (EnginePtr, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let url = if kind == "none" {
            "none".to_string()
        } else if kind == "fs" {
            format!("fs://{}", dir.path().display())
        } else {
            format!("sqlite://{}", dir.path().join("tasks.db").display())
        };
        let engine = connect(&url).await.unwrap();
        engine
            .register_application("history".into(), ApplicationAttributes::default())
            .await
            .unwrap();
        engine
            .create_session(SessionAttributes {
                id: "history".into(),
                application: "history".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        (engine, dir)
    }

    #[tokio::test]
    async fn finished_reads_return_all_terminal_tasks_and_recovery_reads_all_tasks() {
        for kind in ["fs", "sqlite", "none"] {
            let (engine, _dir) = engine(kind).await;
            for state in [TaskState::Succeed, TaskState::Failed, TaskState::Cancelled] {
                let task = engine
                    .create_task("history".into(), Some(Bytes::from("input")), None)
                    .await
                    .unwrap();
                engine
                    .update_task_result(
                        task.clone(),
                        TaskResult {
                            state,
                            output: Some(Bytes::from("output")),
                            message: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            engine
                .create_task("history".into(), None, None)
                .await
                .unwrap();
            let running = engine
                .create_task("history".into(), None, None)
                .await
                .unwrap();
            engine
                .update_task_state(running.clone(), TaskState::Running, None)
                .await
                .unwrap();
            let all = engine
                .find_tasks(&TaskFilter::by_session("history"))
                .await
                .unwrap();
            assert_eq!(all.len(), 5, "{kind}");
            let finished = engine
                .find_tasks(&TaskFilter::terminal("history"))
                .await
                .unwrap();
            assert_eq!(finished.len(), 3, "{kind}");
            let active = engine
                .find_tasks(&TaskFilter::non_terminal("history"))
                .await
                .unwrap();
            assert_eq!(active.len(), 2, "{kind}");
            assert!(active.iter().all(|task| !task.state.is_terminal()));
            let mixed = engine
                .find_tasks(&TaskFilter::by_session_states(
                    "history",
                    vec![TaskState::Pending, TaskState::Failed],
                ))
                .await
                .unwrap();
            assert_eq!(mixed.len(), 2, "{kind}");
            assert!(mixed
                .iter()
                .all(|task| matches!(task.state, TaskState::Pending | TaskState::Failed)));
            let empty = TaskFilter::by_session_states("history", Vec::<TaskState>::new());
            assert!(
                engine.find_tasks(&empty).await.unwrap().is_empty(),
                "{kind}"
            );
            let repeated = TaskFilter::by_session_states(
                "history",
                vec![TaskState::Pending, TaskState::Pending],
            );
            assert_eq!(
                engine.find_tasks(&repeated).await.unwrap().len(),
                1,
                "{kind}"
            );
            for task in finished {
                assert!(task.state.is_terminal());
                assert_eq!(task.input, Some(Bytes::from("input")));
                assert_eq!(task.output, Some(Bytes::from("output")));
            }
            assert!(matches!(
                engine.find_tasks(&TaskFilter::by_session("missing")).await,
                Err(FlameError::NotFound(_))
            ));
            assert!(matches!(
                engine.find_tasks(&TaskFilter::terminal("missing")).await,
                Err(FlameError::NotFound(_))
            ));
            assert!(matches!(
                engine
                    .find_tasks(&TaskFilter::by_session_states(
                        "missing",
                        Vec::<TaskState>::new()
                    ))
                    .await,
                Err(FlameError::NotFound(_))
            ));
        }
    }

    #[tokio::test]
    async fn sqlite_history_and_recovery_propagate_invalid_task_data() {
        let (engine, dir) = engine("sqlite").await;
        let task = engine
            .create_task("history".into(), None, None)
            .await
            .unwrap();
        engine
            .update_task_state(task.clone(), TaskState::Succeed, None)
            .await
            .unwrap();
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("tasks.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("UPDATE tasks SET affinity='invalid' WHERE ssn_id=? AND id=?")
            .bind("history")
            .bind(task.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            engine.find_tasks(&TaskFilter::by_session("history")).await,
            Err(FlameError::Storage(_))
        ));
        assert!(matches!(
            engine.find_tasks(&TaskFilter::terminal("history")).await,
            Err(FlameError::Storage(_))
        ));
        let closed = engine.close_session("history".into()).await.unwrap();
        assert_eq!(closed.task_counts.succeed, 1);
        let repeated = engine.close_session("history".into()).await.unwrap();
        assert_eq!(repeated.task_counts, closed.task_counts);
        pool.close().await;
    }

    #[tokio::test]
    async fn none_retains_task_records_and_payloads_after_completion() {
        let (engine, _dir) = engine("none").await;
        assert!(matches!(
            engine.create_task("missing".into(), None, None).await,
            Err(FlameError::NotFound(_))
        ));
        let created = engine
            .create_task(
                "history".into(),
                Some(Bytes::from_static(b"input")),
                Some(TaskOptions {
                    affinity: [Bytes::from_static(b"affinity")].into_iter().collect(),
                }),
            )
            .await
            .unwrap();
        let running = engine
            .update_task_state(created.clone(), TaskState::Running, None)
            .await
            .unwrap();
        assert_eq!(running.version, created.version + 1);
        assert_eq!(running.state, TaskState::Running);
        assert_eq!(running.completion_time, None);
        assert_eq!(running.input, created.input);
        assert_eq!(running.affinity, created.affinity);
        let finished = engine
            .update_task_result(
                running.clone(),
                TaskResult {
                    state: TaskState::Failed,
                    output: Some(Bytes::from_static(b"output")),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(finished.version, running.version + 1);
        assert_eq!(finished.state, TaskState::Failed);
        assert!(finished.completion_time.is_some());
        assert_eq!(finished.output, Some(Bytes::from_static(b"output")));
        assert_eq!(finished.input, created.input);
        assert_eq!(finished.affinity, created.affinity);
        let cancellable = engine
            .create_task("history".into(), None, None)
            .await
            .unwrap();
        let terminal_state = engine
            .update_task_state(cancellable.clone(), TaskState::Cancelled, None)
            .await
            .unwrap();
        assert!(terminal_state.completion_time.is_some());
        assert_eq!(terminal_state.version, cancellable.version + 1);
        let stored = engine.get_task(created.gid()).await.unwrap();
        assert_eq!(stored.state, finished.state);
        assert_eq!(stored.version, finished.version);
        assert_eq!(stored.input, created.input);
        assert_eq!(stored.output, finished.output);
        assert_eq!(stored.affinity, created.affinity);
        assert_eq!(
            engine
                .find_tasks(&TaskFilter::terminal("history"))
                .await
                .unwrap()
                .len(),
            2
        );
        let metadata = engine.get_session("history".into()).await.unwrap();
        assert_eq!(metadata.task_counts.failed, 1);
        assert_eq!(metadata.task_counts.cancelled, 1);
        assert!(metadata.tasks.is_empty());
        assert!(metadata.tasks_index.is_empty());

        let missing = Task {
            ssn_id: "missing".into(),
            ..running.clone()
        };
        assert!(matches!(
            engine
                .update_task_state(missing.clone(), TaskState::Running, None)
                .await,
            Err(FlameError::NotFound(_))
        ));
        assert!(matches!(
            engine
                .update_task_result(missing, TaskResult::default())
                .await,
            Err(FlameError::NotFound(_))
        ));
        let closed = engine.close_session("history".into()).await.unwrap();
        assert_eq!(
            closed.task_counts,
            TaskCounts {
                failed: 1,
                cancelled: 1,
                ..Default::default()
            }
        );
        assert!(matches!(
            engine.create_task("history".into(), None, None).await,
            Err(FlameError::InvalidState(_))
        ));
        assert!(matches!(
            engine
                .update_task_state(running.clone(), TaskState::Running, None)
                .await,
            Err(FlameError::InvalidState(_))
        ));
        assert!(matches!(
            engine
                .update_task_result(running, TaskResult::default())
                .await,
            Err(FlameError::InvalidState(_))
        ));
    }

    #[tokio::test]
    async fn updates_use_engine_records_instead_of_cached_fields() {
        for kind in ["fs", "sqlite", "none"] {
            let (engine, _dir) = engine(kind).await;
            let created = engine
                .create_task(
                    "history".into(),
                    Some(Bytes::from_static(b"stored input")),
                    None,
                )
                .await
                .unwrap();
            let stale = Task {
                version: 99,
                state: TaskState::Succeed,
                input: Some(Bytes::from_static(b"cached input")),
                output: Some(Bytes::from_static(b"cached output")),
                ..created.clone()
            };
            let running = engine
                .update_task_state(stale.clone(), TaskState::Running, None)
                .await
                .unwrap();
            assert_eq!(running.version, created.version + 1, "{kind}");
            assert_eq!(running.state, TaskState::Running, "{kind}");
            assert_eq!(running.input, created.input, "{kind}");
            assert_eq!(running.output, None, "{kind}");
            let finished = engine
                .update_task_result(
                    stale,
                    TaskResult {
                        state: TaskState::Succeed,
                        output: Some(Bytes::from_static(b"result")),
                        message: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(finished.version, running.version + 1, "{kind}");
            assert_eq!(finished.input, created.input, "{kind}");
            assert_eq!(
                finished.output,
                Some(Bytes::from_static(b"result")),
                "{kind}"
            );
        }
    }

    #[tokio::test]
    async fn close_returns_final_counts_and_repeated_close_keeps_them() {
        for kind in ["fs", "sqlite", "none"] {
            let (engine, _dir) = engine(kind).await;
            let mut running = None;
            for state in [
                TaskState::Pending,
                TaskState::Pending,
                TaskState::Running,
                TaskState::Succeed,
                TaskState::Failed,
                TaskState::Cancelled,
            ] {
                let task = engine
                    .create_task("history".into(), None, None)
                    .await
                    .unwrap();
                let task = engine.update_task_state(task, state, None).await.unwrap();
                if state == TaskState::Running {
                    running = Some(task);
                }
            }
            assert!(
                engine.close_session("history".into()).await.is_err(),
                "{kind}"
            );
            if kind != "none" {
                assert!(
                    !engine
                        .get_session("history".into())
                        .await
                        .unwrap()
                        .is_closed(),
                    "{kind}"
                );
            }
            engine
                .update_task_state(running.unwrap(), TaskState::Failed, None)
                .await
                .unwrap();
            let closed = engine.close_session("history".into()).await.unwrap();
            assert!(closed.is_closed());
            assert_eq!(
                closed.task_counts,
                TaskCounts {
                    pending: 0,
                    running: 0,
                    succeed: 1,
                    failed: 2,
                    cancelled: 3,
                },
                "{kind}"
            );
            for task_id in [1, 2] {
                let task = engine
                    .get_task(TaskGID {
                        ssn_id: "history".into(),
                        task_id,
                    })
                    .await
                    .unwrap();
                assert_eq!(task.state, TaskState::Cancelled, "{kind}");
                assert_eq!(task.version, 3, "{kind}");
                assert!(task.completion_time.is_some(), "{kind}");
            }
            let repeated = engine.close_session("history".into()).await.unwrap();
            assert_eq!(repeated.task_counts, closed.task_counts, "{kind}");
            assert_eq!(repeated.version, closed.version, "{kind}");
            assert_eq!(repeated.completion_time, closed.completion_time, "{kind}");
        }
    }

    #[tokio::test]
    async fn close_and_running_transition_are_serialized() {
        for kind in ["fs", "sqlite", "none"] {
            let (engine, _dir) = engine(kind).await;
            let task = engine
                .create_task("history".into(), None, None)
                .await
                .unwrap();
            let (close, launch) = tokio::join!(
                engine.close_session("history".into()),
                engine.update_task_state(task.clone(), TaskState::Running, None)
            );
            assert_ne!(close.is_ok(), launch.is_ok(), "{kind}");
            let stored = engine.get_task(task.gid()).await.unwrap();
            if close.is_ok() {
                assert_eq!(stored.state, TaskState::Cancelled);
                assert!(engine
                    .create_task("history".into(), None, None)
                    .await
                    .is_err());
            } else {
                assert_eq!(stored.state, TaskState::Running);
                assert!(!engine
                    .get_session("history".into())
                    .await
                    .unwrap()
                    .is_closed());
            }
        }
    }
}

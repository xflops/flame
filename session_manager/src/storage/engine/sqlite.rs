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

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{
    migrate::MigrateDatabase,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    types::Json,
    FromRow, Sqlite, SqliteConnection, SqlitePool,
};
use stdng::{logs::TraceFn, trace_fn};

use common::{
    apis::{
        Application, ApplicationAttributes, ApplicationID, ApplicationSchema, ApplicationState,
        CommonData, Event, ExecutorID, ExecutorState, Node, Role, Session, SessionAttributes,
        SessionID, SessionState, SessionStatus, Shim, Task, TaskGID, TaskID, TaskInput, TaskOutput,
        TaskResult, TaskState, User, Workspace, DEFAULT_DELAY_RELEASE, DEFAULT_MAX_INSTANCES,
    },
    FlameError,
};

use crate::model::Executor;
use crate::storage::engine::types::{
    AppSchemaDao, ApplicationDao, EventDao, ExecutorDao, NodeDao, RoleDao, SessionDao, TaskDao,
    UserDao, WorkspaceDao,
};

use crate::storage::engine::{Engine, EnginePtr};

const SQLITE_SQL: &str = "migrations/sqlite";

pub struct SqliteEngine {
    pool: SqlitePool,
}

impl SqliteEngine {
    pub async fn new_ptr(url: &str) -> Result<EnginePtr, FlameError> {
        tracing::debug!("Try to create and connect to {}", url);

        let options = SqliteConnectOptions::from_str(url)
            .map_err(|e| FlameError::Storage(e.to_string()))?
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(time::Duration::from_secs(15))
            .synchronous(SqliteSynchronous::Normal)
            .create_if_missing(true);

        let db = SqlitePoolOptions::new()
            .max_connections(50)
            .min_connections(3)
            .acquire_timeout(time::Duration::from_secs(30))
            .idle_timeout(time::Duration::from_secs(5 * 60))
            .max_lifetime(time::Duration::from_secs(30 * 60))
            .test_before_acquire(true)
            .connect_with(options)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let migrations = std::path::Path::new(&SQLITE_SQL);
        let migrator = sqlx::migrate::Migrator::new(migrations)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;
        migrator
            .run(&db)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(Arc::new(SqliteEngine { pool: db }))
    }

    async fn _count_open_tasks(
        &self,
        tx: &mut SqliteConnection,
        ssn_id: SessionID,
    ) -> Result<i64, FlameError> {
        let sql = "SELECT count(*) FROM tasks WHERE ssn_id=? AND state NOT IN (?, ?)";
        let count: i64 = sqlx::query_scalar(sql)
            .bind(ssn_id)
            .bind(TaskState::Failed as i32)
            .bind(TaskState::Succeed as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to count open tasks: {e}")))?;
        Ok(count)
    }

    async fn _delete_session(
        &self,
        tx: &mut SqliteConnection,
        id: SessionID,
    ) -> Result<Session, FlameError> {
        let sql = "DELETE FROM tasks WHERE ssn_id=?";
        sqlx::query(sql)
            .bind(id.clone())
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete session: {e}")))?;

        let sql = "DELETE FROM sessions WHERE id=? AND state=? RETURNING *";
        let ssn: SessionDao = sqlx::query_as(sql)
            .bind(id.clone())
            .bind(SessionState::Closed as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to close session: {e}")))?;

        let ssn: Session = ssn.try_into()?;

        Ok(ssn)
    }

    async fn _count_open_sessions(
        &self,
        tx: &mut SqliteConnection,
        app: String,
    ) -> Result<i64, FlameError> {
        let sql = "SELECT count(*) FROM sessions WHERE application=? AND state=?";
        let count: i64 = sqlx::query_scalar(sql)
            .bind(app)
            .bind(SessionState::Open as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to count open sessions: {e}")))?;
        Ok(count)
    }

    async fn _list_session_ids(
        &self,
        tx: &mut SqliteConnection,
        app: String,
    ) -> Result<Vec<SessionID>, FlameError> {
        let sql = "SELECT id FROM sessions WHERE application=?";
        let ids: Vec<SessionID> = sqlx::query_scalar(sql)
            .bind(app)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to list session ids: {e}")))?;
        Ok(ids)
    }

    async fn _delete_application(
        &self,
        tx: &mut SqliteConnection,
        name: String,
    ) -> Result<(), FlameError> {
        let sql = "DELETE FROM applications WHERE name=?";
        sqlx::query(sql)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete application: {e}")))?;
        Ok(())
    }

    /// Internal helper to get session within an existing transaction.
    /// Returns None if session not found.
    async fn _get_session(
        tx: &mut SqliteConnection,
        id: SessionID,
    ) -> Result<Option<Session>, FlameError> {
        let sql = "SELECT * FROM sessions WHERE id=?";
        let ssn: Option<SessionDao> = sqlx::query_as(sql)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        match ssn {
            Some(dao) => Ok(Some(dao.try_into()?)),
            None => Ok(None),
        }
    }

    /// Internal helper to create session within an existing transaction.
    async fn _create_session(
        tx: &mut SqliteConnection,
        attr: SessionAttributes,
    ) -> Result<Session, FlameError> {
        let common_data: Option<Vec<u8>> = attr.common_data.map(Bytes::into);
        let sql = r#"INSERT INTO sessions (id, application, slots, common_data, creation_time, state, min_instances, max_instances)
            VALUES (
                ?,
                (SELECT name FROM applications WHERE name=? AND state=?),
                ?,
                ?,
                ?,
                ?,
                ?,
                ?
            )
            RETURNING *"#;
        let ssn: SessionDao = sqlx::query_as(sql)
            .bind(attr.id.clone())
            .bind(attr.application)
            .bind(ApplicationState::Enabled as i32)
            .bind(attr.slots)
            .bind(common_data)
            .bind(Utc::now().timestamp())
            .bind(SessionState::Open as i32)
            .bind(attr.min_instances as i64)
            .bind(attr.max_instances.map(|v| v as i64))
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        ssn.try_into()
    }

    async fn get_user_role_names(&self, user_name: &str) -> Result<Vec<String>, FlameError> {
        let sql = "SELECT role_name FROM user_roles WHERE user_name = ?";
        let roles: Vec<String> = sqlx::query_scalar(sql)
            .bind(user_name)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get user role names: {e}")))?;
        Ok(roles)
    }
}

#[async_trait]
impl Engine for SqliteEngine {
    async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        trace_fn!("Sqlite::register_application");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let schema: Option<Json<AppSchemaDao>> =
            attr.schema.clone().map(AppSchemaDao::from).map(Json);

        let sql = r#"INSERT INTO applications
            (
                name, 
                description, 
                labels, 
                command, 
                arguments, 
                environments, 
                working_directory, 
                max_instances, 
                delay_release, 
                schema, 
                url,
                creation_time, 
                state)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING *"#;
        let app: ApplicationDao = sqlx::query_as(sql)
            .bind(name)
            .bind(attr.description)
            .bind(Json(attr.labels))
            .bind(attr.command)
            .bind(Json(attr.arguments))
            .bind(Json(attr.environments))
            .bind(attr.working_directory)
            .bind(attr.max_instances)
            .bind(attr.delay_release.num_seconds())
            .bind(schema)
            .bind(attr.url)
            .bind(Utc::now().timestamp())
            .bind(ApplicationState::Enabled as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to register application: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to commit TX: {e}")))?;

        Ok(app.try_into()?)
    }

    async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        trace_fn!("Sqlite::update_application");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let count = self._count_open_sessions(&mut tx, name.clone()).await?;
        if count > 0 {
            return Err(FlameError::Storage(format!(
                "{count} open sessions in the application"
            )));
        }

        let schema: Option<Json<AppSchemaDao>> =
            attr.schema.clone().map(AppSchemaDao::from).map(Json);

        let sql = r#"UPDATE applications
                    SET schema=?,
                        description=?,
                        labels=?,
                        command=?,
                        arguments=?,
                        environments=?,
                        working_directory=?,
                        max_instances=?,
                        delay_release=?,
                        url=?,
                        version=version+1
                    WHERE name=?
                    RETURNING *"#;

        let app: ApplicationDao = sqlx::query_as(sql)
            .bind(schema)
            .bind(attr.description)
            .bind(Json(attr.labels))
            .bind(attr.command)
            .bind(Json(attr.arguments))
            .bind(Json(attr.environments))
            .bind(attr.working_directory)
            .bind(attr.max_instances)
            .bind(attr.delay_release.num_seconds())
            .bind(attr.url)
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update application: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to commit TX: {e}")))?;

        Ok(app.try_into()?)
    }

    async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        trace_fn!("Sqlite::unregister_application");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let count = self._count_open_sessions(&mut tx, name.clone()).await?;
        if count > 0 {
            return Err(FlameError::Storage(format!(
                "{count} open sessions in the application"
            )));
        }

        let ids = self._list_session_ids(&mut tx, name.clone()).await?;
        for id in ids {
            self._delete_session(&mut tx, id).await?;
        }

        self._delete_application(&mut tx, name).await?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to unregister application: {e}")))?;

        Ok(())
    }

    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM applications WHERE name=?";
        let app: ApplicationDao = sqlx::query_as(sql)
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => {
                    FlameError::NotFound(format!("application <{id}> not found"))
                }
                _ => FlameError::Storage(e.to_string()),
            })?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get application: {e}")))?;

        app.try_into()
    }

    async fn find_application(&self) -> Result<Vec<Application>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM applications";
        let app: Vec<ApplicationDao> = sqlx::query_as(sql)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(app
            .iter()
            .map(Application::try_from)
            .filter_map(Result::ok)
            .collect())
    }

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let ssn = Self::_create_session(&mut tx, attr).await?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(ssn)
    }

    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let ssn = Self::_get_session(&mut tx, id.clone())
            .await?
            .ok_or_else(|| FlameError::NotFound(format!("session <{id}> not found")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(ssn)
    }

    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let ssn = match Self::_get_session(&mut tx, id.clone()).await? {
            Some(session) => {
                // Session exists - validate state
                if session.status.state != SessionState::Open {
                    return Err(FlameError::InvalidState(format!(
                        "session <{id}> is not open"
                    )));
                }
                // If spec provided, validate it matches
                if let Some(ref attr) = spec {
                    session.validate_spec(attr)?;
                }
                session
            }
            None => {
                // Session doesn't exist
                match spec {
                    Some(attr) => Self::_create_session(&mut tx, attr).await?,
                    None => return Err(FlameError::NotFound(format!("session <{id}> not found"))),
                }
            }
        };

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(ssn)
    }

    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let count = self._count_open_tasks(&mut tx, id.clone()).await?;
        if count > 0 {
            return Err(FlameError::Storage(format!(
                "{count} open tasks in the session"
            )));
        }

        let ssn = self._delete_session(&mut tx, id).await?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(ssn)
    }

    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let check_running_sql = "SELECT COUNT(*) as cnt FROM tasks WHERE ssn_id=? AND state=?";
        let running_count: (i32,) = sqlx::query_as(check_running_sql)
            .bind(id.clone())
            .bind(TaskState::Running as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        if running_count.0 > 0 {
            return Err(FlameError::Storage(
                "Cannot close session with running tasks".to_string(),
            ));
        }

        let cancel_pending_sql =
            "UPDATE tasks SET state=?, completion_time=? WHERE ssn_id=? AND state=?";
        sqlx::query(cancel_pending_sql)
            .bind(TaskState::Cancelled as i32)
            .bind(Utc::now().timestamp())
            .bind(id.clone())
            .bind(TaskState::Pending as i32)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let close_session_sql = r#"UPDATE sessions 
            SET state=?, completion_time=?, version=version+1
            WHERE id=?
            RETURNING *"#;
        let ssn: SessionDao = sqlx::query_as(close_session_sql)
            .bind(SessionState::Closed as i32)
            .bind(Utc::now().timestamp())
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        ssn.try_into()
    }

    async fn find_session(&self) -> Result<Vec<Session>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM sessions";
        let ssn: Vec<SessionDao> = sqlx::query_as(sql)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(ssn
            .iter()
            .map(Session::try_from)
            .filter_map(Result::ok)
            .collect())
    }

    async fn create_task(
        &self,
        ssn_id: SessionID,
        input: Option<TaskInput>,
    ) -> Result<Task, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let input: Option<Vec<u8>> = input.map(Bytes::into);
        let sql = r#"INSERT INTO tasks (id, ssn_id, input, creation_time, state)
            VALUES (
                COALESCE((SELECT MAX(id)+1 FROM tasks WHERE ssn_id=?), 1),
                (SELECT id FROM sessions WHERE id=? AND state=?),
                ?,
                ?,
                ?)
            RETURNING *"#;
        let task: TaskDao = sqlx::query_as(sql)
            .bind(ssn_id.clone())
            .bind(ssn_id)
            .bind(SessionState::Open as i32)
            .bind(input)
            .bind(Utc::now().timestamp())
            .bind(TaskState::Pending as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        task.try_into()
    }

    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = r#"SELECT * FROM tasks WHERE id=? AND ssn_id=?"#;
        let task: TaskDao = sqlx::query_as(sql)
            .bind(gid.task_id)
            .bind(gid.ssn_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        task.try_into()
    }

    async fn delete_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = r#"DELETE tasks WHERE id=? AND ssn_id=? RETURNING *"#;
        let task: TaskDao = sqlx::query_as(sql)
            .bind(gid.task_id)
            .bind(gid.ssn_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        task.try_into()
    }

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql =
            r#"UPDATE tasks SET state=?, version=version+1 WHERE id=? AND ssn_id=? RETURNING *"#;
        let task: TaskDao = sqlx::query_as(sql)
            .bind(TaskState::Pending as i32)
            .bind(gid.task_id)
            .bind(gid.ssn_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        task.try_into()
    }

    async fn update_task_state(
        &self,
        gid: TaskGID,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<Task, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let completion_time = match task_state {
            TaskState::Failed | TaskState::Succeed | TaskState::Cancelled => {
                Some(Utc::now().timestamp())
            }
            _ => None,
        };

        let sql = r#"UPDATE tasks SET state=?, completion_time=?, version=version+1 WHERE id=? AND ssn_id=? RETURNING *"#;
        let task: TaskDao = sqlx::query_as(sql)
            .bind::<i32>(task_state.into())
            .bind(completion_time)
            .bind(gid.task_id)
            .bind(gid.ssn_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        task.try_into()
    }

    async fn update_task_result(
        &self,
        gid: TaskGID,
        task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let completion_time = match task_result.state {
            TaskState::Failed | TaskState::Succeed => Some(Utc::now().timestamp()),
            _ => {
                tracing::warn!(
                    "Invalid task state <{:?}> for task <{}> when updating task result",
                    task_result.state,
                    gid
                );
                None
            }
        };

        let sql = r#"UPDATE tasks SET state=?, completion_time=?, output=?, version=version+1 WHERE id=? AND ssn_id=? RETURNING *"#;

        let task: TaskDao = sqlx::query_as(sql)
            .bind::<i32>(task_result.state.into())
            .bind(completion_time)
            .bind::<Option<Vec<u8>>>(task_result.output.map(Bytes::into))
            .bind(gid.task_id)
            .bind(gid.ssn_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        task.try_into()
    }

    async fn find_tasks(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM tasks WHERE ssn_id=?";
        let task_list: Vec<TaskDao> = sqlx::query_as(sql)
            .bind(ssn_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let tasks: Vec<Task> = task_list
            .iter()
            .map(Task::try_from)
            .filter_map(Result::ok)
            .collect();

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(tasks)
    }

    // Node operations

    async fn create_node(&self, node: &Node) -> Result<Node, FlameError> {
        trace_fn!("Sqlite::create_node");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let now = Utc::now().timestamp();
        let sql = r#"INSERT INTO nodes 
            (name, state, capacity_cpu, capacity_memory, allocatable_cpu, allocatable_memory, 
             info_arch, info_os, creation_time, last_heartbeat)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING *"#;

        let dao: NodeDao = sqlx::query_as(sql)
            .bind(&node.name)
            .bind(i32::from(node.state))
            .bind(node.capacity.cpu as i64)
            .bind(node.capacity.memory as i64)
            .bind(node.allocatable.cpu as i64)
            .bind(node.allocatable.memory as i64)
            .bind(&node.info.arch)
            .bind(&node.info.os)
            .bind(now)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to create node: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        dao.try_into()
    }

    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM nodes WHERE name=?";
        let dao: Option<NodeDao> = sqlx::query_as(sql)
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        match dao {
            Some(d) => Ok(Some(d.try_into()?)),
            None => Ok(None),
        }
    }

    async fn update_node(&self, node: &Node) -> Result<Node, FlameError> {
        trace_fn!("Sqlite::update_node");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = r#"UPDATE nodes 
            SET state=?, capacity_cpu=?, capacity_memory=?, 
                allocatable_cpu=?, allocatable_memory=?,
                info_arch=?, info_os=?, last_heartbeat=?
            WHERE name=?
            RETURNING *"#;

        let dao: NodeDao = sqlx::query_as(sql)
            .bind(i32::from(node.state))
            .bind(node.capacity.cpu as i64)
            .bind(node.capacity.memory as i64)
            .bind(node.allocatable.cpu as i64)
            .bind(node.allocatable.memory as i64)
            .bind(&node.info.arch)
            .bind(&node.info.os)
            .bind(Utc::now().timestamp())
            .bind(&node.name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update node: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        dao.try_into()
    }

    async fn delete_node(&self, name: &str) -> Result<(), FlameError> {
        trace_fn!("Sqlite::delete_node");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        // Note: executors are automatically deleted via ON DELETE CASCADE foreign key constraint
        let sql = "DELETE FROM nodes WHERE name=?";
        sqlx::query(sql)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete node: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(())
    }

    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM nodes";
        let daos: Vec<NodeDao> = sqlx::query_as(sql)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(daos
            .iter()
            .map(Node::try_from)
            .filter_map(Result::ok)
            .collect())
    }

    // Executor operations

    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        trace_fn!("Sqlite::create_executor");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = r#"INSERT INTO executors 
            (id, node, resreq_cpu, resreq_memory, slots, shim, task_id, ssn_id, creation_time, state)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING *"#;

        let dao: ExecutorDao = sqlx::query_as(sql)
            .bind(&executor.id)
            .bind(&executor.node)
            .bind(executor.resreq.cpu as i64)
            .bind(executor.resreq.memory as i64)
            .bind(executor.slots as i64)
            .bind(i32::from(executor.shim))
            .bind(executor.task_id)
            .bind(&executor.ssn_id)
            .bind(executor.creation_time.timestamp())
            .bind(i32::from(executor.state))
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to create executor: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        dao.try_into()
    }

    async fn get_executor(&self, id: &ExecutorID) -> Result<Option<Executor>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "SELECT * FROM executors WHERE id=?";
        let dao: Option<ExecutorDao> = sqlx::query_as(sql)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        match dao {
            Some(d) => Ok(Some(d.try_into()?)),
            None => Ok(None),
        }
    }

    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        trace_fn!("Sqlite::update_executor");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = r#"UPDATE executors 
            SET node=?, resreq_cpu=?, resreq_memory=?, slots=?, shim=?, 
                task_id=?, ssn_id=?, state=?
            WHERE id=?
            RETURNING *"#;

        let dao: ExecutorDao = sqlx::query_as(sql)
            .bind(&executor.node)
            .bind(executor.resreq.cpu as i64)
            .bind(executor.resreq.memory as i64)
            .bind(executor.slots as i64)
            .bind(i32::from(executor.shim))
            .bind(executor.task_id)
            .bind(&executor.ssn_id)
            .bind(i32::from(executor.state))
            .bind(&executor.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update executor: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        dao.try_into()
    }

    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Sqlite::update_executor_state");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = r#"UPDATE executors SET state=? WHERE id=? RETURNING *"#;

        let dao: ExecutorDao = sqlx::query_as(sql)
            .bind(i32::from(state))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update executor state: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        dao.try_into()
    }

    async fn delete_executor(&self, id: &ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Sqlite::delete_executor");

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let sql = "DELETE FROM executors WHERE id=?";
        sqlx::query(sql)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete executor: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(())
    }

    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        let daos: Vec<ExecutorDao> = match node {
            Some(node_name) => {
                let sql = "SELECT * FROM executors WHERE node=?";
                sqlx::query_as(sql)
                    .bind(node_name)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|e| FlameError::Storage(e.to_string()))?
            }
            None => {
                let sql = "SELECT * FROM executors";
                sqlx::query_as(sql)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|e| FlameError::Storage(e.to_string()))?
            }
        };

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(e.to_string()))?;

        Ok(daos
            .iter()
            .map(Executor::try_from)
            .filter_map(Result::ok)
            .collect())
    }

    async fn get_user(&self, name: &str) -> Result<Option<User>, FlameError> {
        let sql = "SELECT * FROM users WHERE name = ?";
        let dao: Option<UserDao> = sqlx::query_as(sql)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get user: {e}")))?;

        match dao {
            Some(dao) => {
                let mut user: User = dao.try_into()?;
                user.roles = self.get_user_role_names(&user.name).await?;
                Ok(Some(user))
            }
            None => Ok(None),
        }
    }

    async fn get_user_by_cn(&self, cn: &str) -> Result<Option<User>, FlameError> {
        let sql = "SELECT * FROM users WHERE certificate_cn = ?";
        let dao: Option<UserDao> = sqlx::query_as(sql)
            .bind(cn)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get user by cn: {e}")))?;

        match dao {
            Some(dao) => {
                let mut user: User = dao.try_into()?;
                user.roles = self.get_user_role_names(&user.name).await?;
                Ok(Some(user))
            }
            None => Ok(None),
        }
    }

    async fn get_user_roles(&self, user_name: &str) -> Result<Vec<Role>, FlameError> {
        let sql = r#"
            SELECT r.* FROM roles r
            INNER JOIN user_roles ur ON ur.role_name = r.name
            WHERE ur.user_name = ?
        "#;
        let daos: Vec<RoleDao> = sqlx::query_as(sql)
            .bind(user_name)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get user roles: {e}")))?;

        daos.into_iter().map(Role::try_from).collect()
    }

    async fn create_user(&self, user: &User) -> Result<User, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let sql = r#"
            INSERT INTO users (name, display_name, email, certificate_cn, enabled, creation_time)
            VALUES (?, ?, ?, ?, ?, ?)
            RETURNING *
        "#;
        let dao: UserDao = sqlx::query_as(sql)
            .bind(&user.name)
            .bind(&user.display_name)
            .bind(&user.email)
            .bind(&user.certificate_cn)
            .bind(if user.enabled { 1 } else { 0 })
            .bind(Utc::now().timestamp())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to create user: {e}")))?;

        for role_name in &user.roles {
            let sql = "INSERT INTO user_roles (user_name, role_name) VALUES (?, ?)";
            sqlx::query(sql)
                .bind(&user.name)
                .bind(role_name)
                .execute(&mut *tx)
                .await
                .map_err(|e| FlameError::Storage(format!("failed to assign role: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to commit TX: {e}")))?;

        let mut result: User = dao.try_into()?;
        result.roles = user.roles.clone();
        Ok(result)
    }

    async fn update_user(
        &self,
        user: &User,
        assign_roles: &[String],
        revoke_roles: &[String],
    ) -> Result<User, FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let sql = r#"
            UPDATE users SET
                display_name = ?,
                email = ?,
                enabled = ?
            WHERE name = ?
            RETURNING *
        "#;
        let dao: UserDao = sqlx::query_as(sql)
            .bind(&user.display_name)
            .bind(&user.email)
            .bind(if user.enabled { 1 } else { 0 })
            .bind(&user.name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update user: {e}")))?;

        for role_name in revoke_roles {
            let sql = "DELETE FROM user_roles WHERE user_name = ? AND role_name = ?";
            sqlx::query(sql)
                .bind(&user.name)
                .bind(role_name)
                .execute(&mut *tx)
                .await
                .map_err(|e| FlameError::Storage(format!("failed to revoke role: {e}")))?;
        }

        for role_name in assign_roles {
            let sql = "INSERT OR IGNORE INTO user_roles (user_name, role_name) VALUES (?, ?)";
            sqlx::query(sql)
                .bind(&user.name)
                .bind(role_name)
                .execute(&mut *tx)
                .await
                .map_err(|e| FlameError::Storage(format!("failed to assign role: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to commit TX: {e}")))?;

        let mut result: User = dao.try_into()?;
        result.roles = self.get_user_role_names(&result.name).await?;
        Ok(result)
    }

    async fn delete_user(&self, name: &str) -> Result<(), FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let sql = "DELETE FROM user_roles WHERE user_name = ?";
        sqlx::query(sql)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete user roles: {e}")))?;

        let sql = "DELETE FROM users WHERE name = ?";
        sqlx::query(sql)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete user: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to commit TX: {e}")))?;

        Ok(())
    }

    async fn find_users(&self, role_filter: Option<&str>) -> Result<Vec<User>, FlameError> {
        let daos: Vec<UserDao> = match role_filter {
            Some(role) => {
                let sql = r#"
                    SELECT DISTINCT u.* FROM users u
                    INNER JOIN user_roles ur ON ur.user_name = u.name
                    WHERE ur.role_name = ?
                "#;
                sqlx::query_as(sql)
                    .bind(role)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| FlameError::Storage(format!("failed to find users: {e}")))?
            }
            None => {
                let sql = "SELECT * FROM users";
                sqlx::query_as(sql)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| FlameError::Storage(format!("failed to find users: {e}")))?
            }
        };

        let mut users = Vec::with_capacity(daos.len());
        for dao in daos {
            let mut user: User = dao.try_into()?;
            user.roles = self.get_user_role_names(&user.name).await?;
            users.push(user);
        }
        Ok(users)
    }

    async fn get_role(&self, name: &str) -> Result<Option<Role>, FlameError> {
        let sql = "SELECT * FROM roles WHERE name = ?";
        let dao: Option<RoleDao> = sqlx::query_as(sql)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get role: {e}")))?;

        dao.map(Role::try_from).transpose()
    }

    async fn create_role(&self, role: &Role) -> Result<Role, FlameError> {
        let sql = r#"
            INSERT INTO roles (name, description, permissions, workspaces, creation_time)
            VALUES (?, ?, ?, ?, ?)
            RETURNING *
        "#;
        let dao: RoleDao = sqlx::query_as(sql)
            .bind(&role.name)
            .bind(&role.description)
            .bind(Json(&role.permissions))
            .bind(Json(&role.workspaces))
            .bind(Utc::now().timestamp())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to create role: {e}")))?;

        dao.try_into()
    }

    async fn update_role(&self, role: &Role) -> Result<Role, FlameError> {
        let sql = r#"
            UPDATE roles SET
                description = ?,
                permissions = ?,
                workspaces = ?
            WHERE name = ?
            RETURNING *
        "#;
        let dao: RoleDao = sqlx::query_as(sql)
            .bind(&role.description)
            .bind(Json(&role.permissions))
            .bind(Json(&role.workspaces))
            .bind(&role.name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update role: {e}")))?;

        dao.try_into()
    }

    async fn delete_role(&self, name: &str) -> Result<(), FlameError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to begin TX: {e}")))?;

        let sql = "DELETE FROM user_roles WHERE role_name = ?";
        sqlx::query(sql)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete role bindings: {e}")))?;

        let sql = "DELETE FROM roles WHERE name = ?";
        sqlx::query(sql)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete role: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| FlameError::Storage(format!("failed to commit TX: {e}")))?;

        Ok(())
    }

    async fn find_roles(&self, workspace_filter: Option<&str>) -> Result<Vec<Role>, FlameError> {
        let daos: Vec<RoleDao> = match workspace_filter {
            Some(workspace) => {
                let sql = "SELECT * FROM roles WHERE workspaces LIKE ?";
                let pattern = format!("%\"{}\"%", workspace);
                sqlx::query_as(sql)
                    .bind(pattern)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| FlameError::Storage(format!("failed to find roles: {e}")))?
            }
            None => {
                let sql = "SELECT * FROM roles";
                sqlx::query_as(sql)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| FlameError::Storage(format!("failed to find roles: {e}")))?
            }
        };

        daos.into_iter().map(Role::try_from).collect()
    }

    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, FlameError> {
        let sql = "SELECT * FROM workspaces WHERE name = ?";
        let dao: Option<WorkspaceDao> = sqlx::query_as(sql)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to get workspace: {e}")))?;

        dao.map(Workspace::try_from).transpose()
    }

    async fn create_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError> {
        let sql = r#"
            INSERT INTO workspaces (name, description, labels, creation_time)
            VALUES (?, ?, ?, ?)
            RETURNING *
        "#;
        let dao: WorkspaceDao = sqlx::query_as(sql)
            .bind(&workspace.name)
            .bind(&workspace.description)
            .bind(Json(&workspace.labels))
            .bind(Utc::now().timestamp())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to create workspace: {e}")))?;

        dao.try_into()
    }

    async fn update_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError> {
        let sql = r#"
            UPDATE workspaces SET
                description = ?,
                labels = ?
            WHERE name = ?
            RETURNING *
        "#;
        let dao: WorkspaceDao = sqlx::query_as(sql)
            .bind(&workspace.description)
            .bind(Json(&workspace.labels))
            .bind(&workspace.name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to update workspace: {e}")))?;

        dao.try_into()
    }

    async fn delete_workspace(&self, name: &str) -> Result<(), FlameError> {
        let sql = "DELETE FROM workspaces WHERE name = ?";
        sqlx::query(sql)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to delete workspace: {e}")))?;

        Ok(())
    }

    async fn find_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        let sql = "SELECT * FROM workspaces";
        let daos: Vec<WorkspaceDao> = sqlx::query_as(sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| FlameError::Storage(format!("failed to find workspaces: {e}")))?;

        daos.into_iter().map(Workspace::try_from).collect()
    }
}

#[cfg(test)]
mod tests {
    use common::apis::ApplicationState;

    use super::*;

    fn test_get_task_with_events() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_get_task_with_events");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }

        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;
        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_1.id, 1);
        let tasks = tokio_test::block_on(storage.find_tasks(ssn_1.id.clone()))?;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].ssn_id, ssn_1.id.clone());
        assert_eq!(tasks[0].state, TaskState::Pending);
        assert_eq!(tasks[0].input, None);
        assert_eq!(tasks[0].output, None);

        let task_1_1 = tokio_test::block_on(storage.update_task_state(
            task_1_1.gid(),
            TaskState::Succeed,
            Some("Task succeeded".to_string()),
        ))?;
        assert_eq!(task_1_1.state, TaskState::Succeed);
        let tasks = tokio_test::block_on(storage.find_tasks(ssn_1.id.clone()))?;

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].ssn_id, ssn_1.id.clone());
        assert_eq!(tasks[0].state, TaskState::Succeed);

        Ok(())
    }

    #[test]
    fn test_update_application() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_update_application");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }

        let app_1 = tokio_test::block_on(storage.get_application("flmexec".to_string()))?;
        assert_eq!(app_1.name, "flmexec");
        assert_eq!(app_1.state, ApplicationState::Enabled);

        let app_2 = tokio_test::block_on(storage.update_application(
            "flmexec".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                description: Some("This is my agent for testing.".to_string()),
                labels: vec!["test".to_string(), "agent".to_string()],
                image: Some("may-agent".to_string()),
                command: Some("run-agent".to_string()),
                arguments: vec!["--test".to_string(), "--agent".to_string()],
                environments: HashMap::from([("TEST".to_string(), "true".to_string())]),
                working_directory: Some("/tmp".to_string()),
                max_instances: 10,
                delay_release: Duration::seconds(0),
                schema: None,
                url: None,
            },
        ))?;
        assert_eq!(app_2.name, "flmexec");
        assert_eq!(
            app_2.description,
            Some("This is my agent for testing.".to_string())
        );
        assert_eq!(app_2.labels, vec!["test".to_string(), "agent".to_string()]);
        assert_eq!(app_2.command, Some("run-agent".to_string()));
        assert_eq!(
            app_2.arguments,
            vec!["--test".to_string(), "--agent".to_string()]
        );
        assert_eq!(
            app_2.environments,
            HashMap::from([("TEST".to_string(), "true".to_string())])
        );
        assert_eq!(app_2.working_directory, Some("/tmp".to_string()));
        assert_eq!(app_2.max_instances, 10);
        assert_eq!(app_2.delay_release, Duration::seconds(0));
        assert!(app_2.schema.is_none());

        Ok(())
    }

    #[test]
    fn test_unregister_application() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_unregister_application");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }

        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());

        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;
        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id, None))?;
        assert_eq!(task_1_1.id, 1);
        let res = tokio_test::block_on(storage.unregister_application("flmexec".to_string()));
        assert!(res.is_err());

        let task_1_1 = tokio_test::block_on(storage.get_task(task_1_1.gid()))?;
        assert_eq!(task_1_1.state, TaskState::Pending);

        let task_1_1 = tokio_test::block_on(storage.update_task_state(
            task_1_1.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_1.state, TaskState::Succeed);

        let res = tokio_test::block_on(storage.unregister_application("flmexec".to_string()));
        assert!(res.is_err());

        let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);

        let res = tokio_test::block_on(storage.unregister_application("flmexec".to_string()));
        assert!(res.is_ok());

        let app_1 = tokio_test::block_on(storage.get_application("flmexec".to_string()));
        assert!(app_1.is_err());

        let list_ssn = tokio_test::block_on(storage.find_session())?;
        assert_eq!(list_ssn.len(), 0);

        Ok(())
    }

    #[test]
    fn test_register_application() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_register_appl");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        let string_schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "string",
            "description": "The string for testing."
        });

        let apps = vec![
            (
                "my-test-agent-1".to_string(),
                ApplicationAttributes {
                    shim: Shim::Host,
                    image: Some("may-agent".to_string()),
                    description: Some("This is my agent for testing.".to_string()),
                    labels: vec!["test".to_string(), "agent".to_string()],
                    command: Some("my-agent".to_string()),
                    arguments: vec!["--test".to_string(), "--agent".to_string()],
                    environments: HashMap::from([("TEST".to_string(), "true".to_string())]),
                    working_directory: Some("/tmp".to_string()),
                    max_instances: 10,
                    delay_release: Duration::seconds(0),
                    schema: Some(ApplicationSchema {
                        input: Some(string_schema.to_string()),
                        output: Some(string_schema.to_string()),
                        common_data: None,
                    }),
                    url: None,
                },
            ),
            (
                "empty-app".to_string(),
                ApplicationAttributes {
                    shim: Shim::Host,
                    image: None,
                    description: None,
                    labels: vec![],
                    command: None,
                    arguments: vec![],
                    environments: HashMap::new(),
                    working_directory: Some("/tmp".to_string()),
                    max_instances: 10,
                    delay_release: Duration::seconds(0),
                    schema: None,
                    url: None,
                },
            ),
        ];
        for (name, attr) in apps {
            tokio_test::block_on(storage.register_application(name.clone(), attr)).map_err(
                |e| FlameError::Storage(format!("failed to register application <{name}>: {e}")),
            )?;
            let app_1 =
                tokio_test::block_on(storage.get_application(name.clone())).map_err(|e| {
                    FlameError::Storage(format!("failed to get application <{name}>: {e}"))
                })?;

            assert_eq!(app_1.name, name);
            assert_eq!(app_1.state, ApplicationState::Enabled);
        }

        Ok(())
    }

    #[test]
    fn test_get_application() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_app");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }

        let app_1 = tokio_test::block_on(storage.get_application("flmexec".to_string()))?;

        assert_eq!(app_1.name, "flmexec");
        assert_eq!(app_1.state, ApplicationState::Enabled);

        Ok(())
    }

    #[test]
    fn test_register_application_with_url() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_register_app_with_url");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        let test_url = "file:///opt/test-package.whl".to_string();

        // Register application with URL
        let app = tokio_test::block_on(storage.register_application(
            "flmtestapp-url".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                image: None,
                description: Some("Test application with URL".to_string()),
                labels: vec!["test".to_string()],
                command: Some("/usr/bin/uv".to_string()),
                arguments: vec![
                    "run".to_string(),
                    "-n".to_string(),
                    "flamepy.runner.runpy".to_string(),
                ],
                environments: HashMap::new(),
                working_directory: Some("/tmp".to_string()),
                max_instances: 5,
                delay_release: Duration::seconds(10),
                schema: None,
                url: Some(test_url.clone()),
            },
        ))?;

        // Verify application was registered with URL
        assert_eq!(app.name, "flmtestapp-url");
        assert_eq!(app.url, Some(test_url.clone()));
        assert_eq!(
            app.description,
            Some("Test application with URL".to_string())
        );
        assert_eq!(app.state, ApplicationState::Enabled);

        // Retrieve and verify URL persisted
        let retrieved_app =
            tokio_test::block_on(storage.get_application("flmtestapp-url".to_string()))?;
        assert_eq!(retrieved_app.name, "flmtestapp-url");
        assert_eq!(retrieved_app.url, Some(test_url));
        assert_eq!(
            retrieved_app.description,
            Some("Test application with URL".to_string())
        );
        assert_eq!(retrieved_app.state, ApplicationState::Enabled);

        Ok(())
    }

    #[test]
    fn test_register_application_without_url() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_register_app_without_url");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        // Register application without URL (backward compatibility test)
        let app = tokio_test::block_on(storage.register_application(
            "flmtestapp-no-url".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                image: None,
                description: Some("Test application without URL".to_string()),
                labels: vec!["test".to_string()],
                command: Some("/usr/bin/test".to_string()),
                arguments: vec![],
                environments: HashMap::new(),
                working_directory: Some("/tmp".to_string()),
                max_instances: 5,
                delay_release: Duration::seconds(10),
                schema: None,
                url: None,
            },
        ))?;

        // Verify application was registered without URL
        assert_eq!(app.name, "flmtestapp-no-url");
        assert_eq!(app.url, None);
        assert_eq!(
            app.description,
            Some("Test application without URL".to_string())
        );
        assert_eq!(app.state, ApplicationState::Enabled);

        // Retrieve and verify URL is None
        let retrieved_app =
            tokio_test::block_on(storage.get_application("flmtestapp-no-url".to_string()))?;
        assert_eq!(retrieved_app.name, "flmtestapp-no-url");
        assert_eq!(retrieved_app.url, None);
        assert_eq!(retrieved_app.state, ApplicationState::Enabled);

        Ok(())
    }

    #[test]
    fn test_update_application_with_url() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_update_application_with_url");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;

        // Register initial application without URL
        tokio_test::block_on(storage.register_application(
            "flmtestapp-update".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                image: None,
                description: Some("Initial description".to_string()),
                labels: vec![],
                command: Some("/usr/bin/test".to_string()),
                arguments: vec![],
                environments: HashMap::new(),
                working_directory: Some("/tmp".to_string()),
                max_instances: 5,
                delay_release: Duration::seconds(10),
                schema: None,
                url: None,
            },
        ))?;

        let app_before =
            tokio_test::block_on(storage.get_application("flmtestapp-update".to_string()))?;
        assert_eq!(app_before.url, None);

        // Update application with URL
        let test_url = "file:///opt/updated-package.whl".to_string();
        let updated_app = tokio_test::block_on(storage.update_application(
            "flmtestapp-update".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                image: Some("updated-image".to_string()),
                description: Some("Updated description".to_string()),
                labels: vec!["updated".to_string()],
                command: Some("/usr/bin/uv".to_string()),
                arguments: vec!["run".to_string()],
                environments: HashMap::from([("ENV".to_string(), "test".to_string())]),
                working_directory: Some("/opt".to_string()),
                max_instances: 10,
                delay_release: Duration::seconds(20),
                schema: None,
                url: Some(test_url.clone()),
            },
        ))?;

        // Verify update including URL
        assert_eq!(updated_app.name, "flmtestapp-update");
        assert_eq!(updated_app.url, Some(test_url.clone()));
        assert_eq!(
            updated_app.description,
            Some("Updated description".to_string())
        );
        // Note: image field is not updated by update_application method
        assert_eq!(updated_app.working_directory, Some("/opt".to_string()));
        assert_eq!(updated_app.max_instances, 10);

        // Retrieve and verify URL persisted after update
        let retrieved_app =
            tokio_test::block_on(storage.get_application("flmtestapp-update".to_string()))?;
        assert_eq!(retrieved_app.url, Some(test_url));
        assert_eq!(
            retrieved_app.description,
            Some("Updated description".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_single_session() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_single_session");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;
        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }

        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_1.id, 1);

        let task_1_2 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_2.id, 2);

        let task_list = tokio_test::block_on(storage.find_tasks(ssn_1.id))?;
        assert_eq!(task_list.len(), 2);

        let task_1_1 = tokio_test::block_on(storage.update_task_state(
            task_1_1.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_1.state, TaskState::Succeed);

        let task_1_2 = tokio_test::block_on(storage.update_task_state(
            task_1_2.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_2.state, TaskState::Succeed);

        let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);

        Ok(())
    }

    #[test]
    fn test_multiple_session() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_multiple_session");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;
        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }

        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_1.id, 1);

        let task_1_2 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_2.id, 2);

        let task_1_1 = tokio_test::block_on(storage.update_task_state(
            task_1_1.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_1.state, TaskState::Succeed);

        let task_1_2 = tokio_test::block_on(storage.update_task_state(
            task_1_2.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_2.state, TaskState::Succeed);

        let ssn_2_id = format!("ssn-2-{}", Utc::now().timestamp());
        let ssn_2 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_2_id.clone(),
            application: "flmping".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_2.id, ssn_2_id);
        assert_eq!(ssn_2.application, "flmping");
        assert_eq!(ssn_2.status.state, SessionState::Open);

        let task_2_1 = tokio_test::block_on(storage.create_task(ssn_2.id.clone(), None))?;
        assert_eq!(task_2_1.id, 1);

        let task_2_2 = tokio_test::block_on(storage.create_task(ssn_2.id.clone(), None))?;
        assert_eq!(task_2_2.id, 2);

        let task_2_1 = tokio_test::block_on(storage.update_task_state(
            task_2_1.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_2_1.state, TaskState::Succeed);

        let task_2_2 = tokio_test::block_on(storage.update_task_state(
            task_2_2.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_2_2.state, TaskState::Succeed);

        let ssn_list = tokio_test::block_on(storage.find_session())?;
        assert_eq!(ssn_list.len(), 2);

        let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);
        let ssn_2 = tokio_test::block_on(storage.close_session(ssn_2_id.clone()))?;
        assert_eq!(ssn_2.status.state, SessionState::Closed);

        Ok(())
    }

    #[test]
    fn test_close_session_with_open_tasks() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_close_session_with_open_tasks");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;
        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }
        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_1.id, 1);

        let task_1_2 = tokio_test::block_on(storage.create_task(ssn_1.id, None))?;
        assert_eq!(task_1_2.id, 2);

        let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);

        let task_1_1 = tokio_test::block_on(storage.get_task(task_1_1.gid()))?;
        assert_eq!(task_1_1.state, TaskState::Cancelled);

        let task_1_2 = tokio_test::block_on(storage.get_task(task_1_2.gid()))?;
        assert_eq!(task_1_2.state, TaskState::Cancelled);

        Ok(())
    }

    #[test]
    fn test_close_session_with_running_tasks() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_close_session_with_running_tasks");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;
        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }
        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_1.state, TaskState::Pending);

        tokio_test::block_on(storage.update_task_state(task_1_1.gid(), TaskState::Running, None))?;

        let res = tokio_test::block_on(storage.close_session(ssn_1_id.clone()));
        assert!(res.is_err());

        Ok(())
    }

    #[test]
    fn test_create_task_for_close_session() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_create_task_for_close_session");

        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;
        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }
        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id, None))?;
        assert_eq!(task_1_1.id, 1);

        let task_1_1 = tokio_test::block_on(storage.update_task_state(
            task_1_1.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_1.state, TaskState::Succeed);

        let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);

        let res = tokio_test::block_on(storage.create_task(ssn_1.id, None));
        assert!(res.is_err());

        Ok(())
    }

    #[test]
    fn test_delete_session_with_open_tasks() -> Result<(), FlameError> {
        let url = common::temp_sqlite_url("flame_test_delete_session_with_open_tasks");
        let storage = tokio_test::block_on(SqliteEngine::new_ptr(&url))?;
        for (name, attr) in common::default_applications() {
            tokio_test::block_on(storage.register_application(name.clone(), attr))?;
        }
        let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
        let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
            id: ssn_1_id.clone(),
            application: "flmexec".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }))?;

        assert_eq!(ssn_1.id, ssn_1_id);
        assert_eq!(ssn_1.application, "flmexec");
        assert_eq!(ssn_1.status.state, SessionState::Open);

        let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None))?;
        assert_eq!(task_1_1.id, 1);

        // It should be failed because the session is open and there are open tasks
        let res = tokio_test::block_on(storage.delete_session(ssn_1_id.clone()));
        assert!(res.is_err());

        let task_1_1 = tokio_test::block_on(storage.get_task(task_1_1.gid()))?;
        assert_eq!(task_1_1.state, TaskState::Pending);

        let task_1_1 = tokio_test::block_on(storage.update_task_state(
            task_1_1.gid(),
            TaskState::Succeed,
            None,
        ))?;
        assert_eq!(task_1_1.state, TaskState::Succeed);

        // It should be failed because the session is open
        let res = tokio_test::block_on(storage.delete_session(ssn_1_id.clone()));
        assert!(res.is_err());

        let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);

        let ssn_1 = tokio_test::block_on(storage.delete_session(ssn_1_id.clone()))?;
        assert_eq!(ssn_1.status.state, SessionState::Closed);

        Ok(())
    }
}

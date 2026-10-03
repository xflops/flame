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

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    types::Json,
    SqlitePool,
};
use std::{str::FromStr, sync::Arc, time::Duration as StdDuration};
use uuid::Uuid;

use super::{
    types::{AppSchemaDao, ApplicationDao, ExecutorDao, NodeDao, SessionDao, TaskDao},
    Engine, EnginePtr,
};
use crate::{
    apis::{
        Application, ApplicationAttributes, ApplicationFilter, ApplicationState, Executor,
        ExecutorGID, ExecutorState, Node, Session, SessionAttributes, SessionGID, SessionState,
        Task, TaskInput, TaskName, TaskOptions, TaskResult, TaskState, Workspace,
    },
    FlameError,
};

pub struct SqliteEngine {
    pool: SqlitePool,
}

fn storage(error: impl std::fmt::Display) -> FlameError {
    FlameError::Storage(error.to_string())
}
fn missing(kind: &str, name: &str) -> FlameError {
    FlameError::NotFound(format!("{kind} {name}"))
}
fn unique(error: sqlx::Error, kind: &str) -> FlameError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        FlameError::AlreadyExist(kind.to_string())
    } else {
        storage(error)
    }
}
fn timestamp(value: i64) -> Result<DateTime<Utc>, FlameError> {
    DateTime::<Utc>::from_timestamp(value, 0).ok_or_else(|| storage("invalid timestamp"))
}

impl SqliteEngine {
    pub async fn new_ptr(url: &str) -> Result<EnginePtr, FlameError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage)?
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(StdDuration::from_secs(15))
            .synchronous(SqliteSynchronous::Normal)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(50)
            .min_connections(3)
            .connect_with(options)
            .await
            .map_err(storage)?;
        let installed = std::path::Path::new("migrations/sqlite");
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/sqlite");
        let migrations = if installed.exists() {
            installed
        } else {
            source.as_path()
        };
        sqlx::migrate::Migrator::new(migrations)
            .await
            .map_err(storage)?
            .run(&pool)
            .await
            .map_err(storage)?;
        Ok(Arc::new(Self { pool }))
    }
    async fn application(&self, workspace: &str, name: &str) -> Result<ApplicationDao, FlameError> {
        sqlx::query_as("SELECT * FROM applications WHERE workspace=? AND name=?")
            .bind(workspace)
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                if matches!(e, sqlx::Error::RowNotFound) {
                    missing("application", name)
                } else {
                    storage(e)
                }
            })
    }
    async fn session(&self, gid: &SessionGID) -> Result<SessionDao, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        sqlx::query_as("SELECT * FROM sessions WHERE workspace=? AND name=?")
            .bind(workspace)
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                if matches!(e, sqlx::Error::RowNotFound) {
                    missing("session", name)
                } else {
                    storage(e)
                }
            })
    }
    async fn task(&self, gid: &SessionGID, name: &str) -> Result<TaskDao, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        sqlx::query_as("SELECT * FROM tasks WHERE workspace=? AND session=? AND name=?")
            .bind(workspace)
            .bind(session)
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                if matches!(e, sqlx::Error::RowNotFound) {
                    missing("task", name)
                } else {
                    storage(e)
                }
            })
    }
}

#[async_trait]
impl Engine for SqliteEngine {
    async fn create_workspace(&self, name: String) -> Result<Workspace, FlameError> {
        let now = Utc::now();
        sqlx::query("INSERT INTO workspaces(name,create_at) VALUES (?,?)")
            .bind(&name)
            .bind(now.timestamp())
            .execute(&self.pool)
            .await
            .map_err(|e| unique(e, "workspace"))?;
        Ok(Workspace {
            name,
            create_at: now,
        })
    }
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT name,create_at FROM workspaces ORDER BY name")
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
        rows.into_iter()
            .map(|(name, create_at)| {
                Ok(Workspace {
                    name,
                    create_at: timestamp(create_at)?,
                })
            })
            .collect()
    }
    async fn register_application(
        &self,
        workspace: String,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        crate::apis::validate_application_url(&workspace, attr.url.as_deref())?;
        let id = Uuid::new_v4().to_string();
        let schema = attr.schema.map(|x| Json(AppSchemaDao::from(x)));
        let dao: ApplicationDao = sqlx::query_as("INSERT INTO applications (id,workspace,name,version,shim,image,description,labels,command,arguments,environments,working_directory,max_instances,delay_release,schema,url,installer,creation_time,state) VALUES (?,?,?,1,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING *")
            .bind(id).bind(workspace).bind(name).bind(attr.shim as i32).bind(attr.image).bind(attr.description)
            .bind(Json(attr.labels)).bind(attr.command).bind(Json(attr.arguments)).bind(Json(attr.environments))
            .bind(attr.working_directory).bind(attr.max_instances as i64).bind(attr.delay_release.num_seconds())
            .bind(schema).bind(attr.url).bind(attr.installer).bind(Utc::now().timestamp())
            .bind(ApplicationState::Enabled as i32).fetch_one(&self.pool).await.map_err(|e| unique(e,"application"))?;
        dao.try_into()
    }
    async fn update_application_state(
        &self,
        workspace: &str,
        name: &str,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let dao: ApplicationDao = sqlx::query_as("UPDATE applications SET state=?,version=version+1 WHERE workspace=? AND name=? RETURNING *")
            .bind(state as i32).bind(workspace).bind(name).fetch_one(&self.pool).await
            .map_err(|e| if matches!(e,sqlx::Error::RowNotFound) {missing("application",name)} else {storage(e)})?;
        dao.try_into()
    }
    async fn delete_application(&self, workspace: &str, name: &str) -> Result<(), FlameError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let state: Option<i32> =
            sqlx::query_scalar("SELECT state FROM applications WHERE workspace=? AND name=?")
                .bind(workspace)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        match state {
            None => return Err(missing("application", name)),
            Some(value) if value != ApplicationState::Disabled as i32 => {
                return Err(FlameError::InvalidState(format!(
                    "application {name} is not disabled"
                )))
            }
            _ => {}
        }
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sessions WHERE workspace=? AND application=?")
                .bind(workspace)
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        if count > 0 {
            return Err(FlameError::InvalidState(format!(
                "application {name} still has {count} sessions"
            )));
        }
        sqlx::query("DELETE FROM applications WHERE workspace=? AND name=?")
            .bind(workspace)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)
    }
    async fn update_application(
        &self,
        workspace: &str,
        name: &str,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        crate::apis::validate_application_url(workspace, attr.url.as_deref())?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current: ApplicationDao =
            sqlx::query_as("SELECT * FROM applications WHERE workspace=? AND name=?")
                .bind(workspace)
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| {
                    if matches!(e, sqlx::Error::RowNotFound) {
                        missing("application", name)
                    } else {
                        storage(e)
                    }
                })?;
        if current.state != ApplicationState::Enabled as i32 {
            return Err(FlameError::InvalidState(format!(
                "application {name} is not enabled"
            )));
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE workspace=? AND application=? AND state=?",
        )
        .bind(workspace)
        .bind(name)
        .bind(SessionState::Open as i32)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if active > 0 {
            return Err(FlameError::InvalidState(format!(
                "{active} open sessions in application"
            )));
        }
        let schema = attr.schema.map(|x| Json(AppSchemaDao::from(x)));
        let dao: ApplicationDao = sqlx::query_as("UPDATE applications SET shim=?,image=?,description=?,labels=?,command=?,arguments=?,environments=?,working_directory=?,max_instances=?,delay_release=?,schema=?,url=?,installer=?,version=version+1 WHERE workspace=? AND name=? RETURNING *")
            .bind(attr.shim as i32).bind(attr.image).bind(attr.description).bind(Json(attr.labels)).bind(attr.command)
            .bind(Json(attr.arguments)).bind(Json(attr.environments)).bind(attr.working_directory)
            .bind(attr.max_instances as i64).bind(attr.delay_release.num_seconds()).bind(schema).bind(attr.url)
            .bind(attr.installer).bind(workspace).bind(name).fetch_one(&mut *tx).await.map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        dao.try_into()
    }
    async fn get_application(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Application, FlameError> {
        self.application(workspace, name).await?.try_into()
    }
    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        let rows: Vec<ApplicationDao> =
            sqlx::query_as("SELECT * FROM applications ORDER BY workspace,name")
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
        rows.into_iter()
            .filter(|x| {
                filter.is_none_or(|f| {
                    f.workspace == x.workspace && f.state.is_none_or(|s| s as i32 == x.state)
                })
            })
            .map(Application::try_from)
            .collect()
    }
    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        let SessionAttributes {
            workspace,
            name,
            application,
            common_data,
            tokens,
            min_instances,
            max_instances,
            batch_size,
            priority,
            resreq,
        } = attr;
        let resreq_cpu = resreq.as_ref().map(|x| x.cpu as i64);
        let resreq_memory = resreq.as_ref().map(|x| x.memory as i64);
        let resreq_gpu = resreq.as_ref().map(|x| x.gpu as i64);
        let tokens = serde_json::to_string(&tokens).map_err(storage)?;
        let dao: SessionDao = sqlx::query_as("INSERT INTO sessions (id,workspace,name,application,version,common_data,tokens,creation_time,state,min_instances,max_instances,batch_size,priority,resreq_cpu,resreq_memory,resreq_gpu) VALUES (?,?,?,?,1,?,?,?,?,?,?,?,?,?,?,?) RETURNING *")
            .bind(Uuid::new_v4().to_string()).bind(workspace).bind(name).bind(application)
            .bind(common_data.map(|x| x.to_vec())).bind(tokens).bind(Utc::now().timestamp())
            .bind(SessionState::Open as i32).bind(min_instances as i64).bind(max_instances.map(i64::from))
            .bind(batch_size as i64).bind(priority as i64).bind(resreq_cpu).bind(resreq_memory).bind(resreq_gpu)
            .fetch_one(&self.pool).await.map_err(|e| unique(e,"session"))?;
        dao.try_into()
    }
    async fn get_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        self.session(gid).await?.try_into()
    }
    async fn open_session(
        &self,
        gid: &SessionGID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        match self.session(gid).await {
            Ok(dao) => {
                if dao.state == SessionState::Open as i32 {
                    return dao.try_into();
                }
                let dao: SessionDao = sqlx::query_as("UPDATE sessions SET state=?,completion_time=NULL,version=version+1 WHERE workspace=? AND name=? RETURNING *")
                    .bind(SessionState::Open as i32).bind(workspace).bind(name).fetch_one(&self.pool).await.map_err(storage)?;
                dao.try_into()
            }
            Err(FlameError::NotFound(_)) => match spec {
                Some(attr) => self.create_session(attr).await,
                None => Err(missing("session", name)),
            },
            Err(e) => Err(e),
        }
    }
    async fn close_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let running: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM tasks WHERE workspace=? AND session=? AND state=?",
        )
        .bind(workspace)
        .bind(name)
        .bind(TaskState::Running as i32)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if running > 0 {
            return Err(FlameError::InvalidState(
                "cannot close session with running tasks".into(),
            ));
        }
        sqlx::query("UPDATE tasks SET state=?,completion_time=? WHERE workspace=? AND session=? AND state=?")
            .bind(TaskState::Cancelled as i32).bind(Utc::now().timestamp()).bind(workspace).bind(name)
            .bind(TaskState::Pending as i32).execute(&mut *tx).await.map_err(storage)?;
        let dao:SessionDao=sqlx::query_as("UPDATE sessions SET state=?,completion_time=?,version=version+1 WHERE workspace=? AND name=? RETURNING *")
            .bind(SessionState::Closed as i32).bind(Utc::now().timestamp()).bind(workspace).bind(name)
            .fetch_one(&mut *tx).await.map_err(|e| if matches!(e,sqlx::Error::RowNotFound){missing("session",name)}else{storage(e)})?;
        tx.commit().await.map_err(storage)?;
        dao.try_into()
    }
    async fn delete_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let dao: SessionDao = sqlx::query_as(
            "DELETE FROM sessions WHERE workspace=? AND name=? AND state=? RETURNING *",
        )
        .bind(workspace)
        .bind(name)
        .bind(SessionState::Closed as i32)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if matches!(e, sqlx::Error::RowNotFound) {
                FlameError::InvalidState(format!("session {name} is not closed"))
            } else {
                storage(e)
            }
        })?;
        tx.commit().await.map_err(storage)?;
        dao.try_into()
    }
    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError> {
        let rows: Vec<SessionDao> =
            sqlx::query_as("SELECT * FROM sessions ORDER BY workspace,name")
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
        rows.into_iter().map(Session::try_from).collect()
    }
    async fn create_task(
        &self,
        gid: &SessionGID,
        input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let state: Option<i32> =
            sqlx::query_scalar("SELECT state FROM sessions WHERE workspace=? AND name=?")
                .bind(workspace)
                .bind(session)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        if state != Some(SessionState::Open as i32) {
            return Err(FlameError::InvalidState(format!(
                "session {session} is not open"
            )));
        }
        let latest: Option<String> = sqlx::query_scalar(
            "SELECT name FROM tasks WHERE workspace=? AND session=? ORDER BY length(name) DESC, name DESC LIMIT 1",
        )
        .bind(workspace)
        .bind(session)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let next = latest
            .as_deref()
            .map(|name| name.parse::<TaskName>().map_err(storage))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| storage("task number overflow"))?;
        let affinity = serde_json::to_string(
            &options
                .unwrap_or_default()
                .affinity
                .iter()
                .map(|x| x.to_vec())
                .collect::<Vec<_>>(),
        )
        .map_err(storage)?;
        let dao:TaskDao=sqlx::query_as("INSERT INTO tasks (id,workspace,session,name,version,input,affinity,creation_time,state) VALUES (?,?,?,?,1,?,?,?,?) RETURNING *")
            .bind(Uuid::new_v4().to_string()).bind(workspace).bind(session).bind(next.to_string())
            .bind(input.map(|x| x.to_vec())).bind(affinity).bind(Utc::now().timestamp()).bind(TaskState::Pending as i32)
            .fetch_one(&mut *tx).await.map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        dao.try_into()
    }
    async fn get_task(&self, gid: &SessionGID, task: &str) -> Result<Task, FlameError> {
        self.task(gid, task).await?.try_into()
    }
    async fn retry_task(&self, gid: &SessionGID, task: &str) -> Result<Task, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        let dao:TaskDao=sqlx::query_as("UPDATE tasks SET state=?,output=NULL,completion_time=NULL,version=version+1 WHERE workspace=? AND session=? AND name=? RETURNING *")
            .bind(TaskState::Pending as i32).bind(workspace).bind(session).bind(task).fetch_one(&self.pool).await
            .map_err(|e|if matches!(e,sqlx::Error::RowNotFound){missing("task",task)}else{storage(e)})?;
        dao.try_into()
    }
    async fn update_task_state(
        &self,
        gid: &SessionGID,
        task: &str,
        state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        let completion = state.is_terminal().then(|| Utc::now().timestamp());
        let dao:TaskDao=sqlx::query_as("UPDATE tasks SET state=?,completion_time=?,version=version+1 WHERE workspace=? AND session=? AND name=? RETURNING *")
            .bind(state as i32).bind(completion).bind(workspace).bind(session).bind(task).fetch_one(&self.pool).await
            .map_err(|e|if matches!(e,sqlx::Error::RowNotFound){missing("task",task)}else{storage(e)})?;
        dao.try_into()
    }
    async fn update_task_result(
        &self,
        gid: &SessionGID,
        task: &str,
        result: TaskResult,
    ) -> Result<Task, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        let completion = result.state.is_terminal().then(|| Utc::now().timestamp());
        let dao:TaskDao=sqlx::query_as("UPDATE tasks SET state=?,output=?,completion_time=?,version=version+1 WHERE workspace=? AND session=? AND name=? RETURNING *")
            .bind(result.state as i32).bind(result.output.map(|x| x.to_vec())).bind(completion)
            .bind(workspace).bind(session).bind(task).fetch_one(&self.pool).await
            .map_err(|e|if matches!(e,sqlx::Error::RowNotFound){missing("task",task)}else{storage(e)})?;
        dao.try_into()
    }
    async fn find_tasks(&self, gid: &SessionGID) -> Result<Vec<Task>, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        let rows: Vec<TaskDao> = sqlx::query_as(
            "SELECT * FROM tasks WHERE workspace=? AND session=? ORDER BY length(name), name",
        )
        .bind(workspace)
        .bind(session)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter().map(Task::try_from).collect()
    }
    async fn create_node(&self, node: &Node) -> Result<Node, FlameError> {
        let dao:NodeDao=sqlx::query_as("INSERT INTO nodes (id,name,state,capacity_cpu,capacity_memory,capacity_gpu,allocatable_cpu,allocatable_memory,allocatable_gpu,info_arch,info_os,creation_time,last_heartbeat) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING *")
            .bind(&node.id).bind(&node.name).bind(i32::from(node.state))
            .bind(node.capacity.cpu as i64).bind(node.capacity.memory as i64).bind(node.capacity.gpu as i64)
            .bind(node.allocatable.cpu as i64).bind(node.allocatable.memory as i64).bind(node.allocatable.gpu as i64)
            .bind(&node.info.arch).bind(&node.info.os).bind(Utc::now().timestamp()).bind(Utc::now().timestamp())
            .fetch_one(&self.pool).await.map_err(|e|unique(e,"node"))?;
        dao.try_into()
    }
    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        let dao: Option<NodeDao> = sqlx::query_as("SELECT * FROM nodes WHERE name=?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        dao.map(Node::try_from).transpose()
    }
    async fn update_node(&self, node: &Node) -> Result<Node, FlameError> {
        let dao:NodeDao=sqlx::query_as("UPDATE nodes SET state=?,capacity_cpu=?,capacity_memory=?,capacity_gpu=?,allocatable_cpu=?,allocatable_memory=?,allocatable_gpu=?,info_arch=?,info_os=?,last_heartbeat=? WHERE name=? RETURNING *")
            .bind(i32::from(node.state)).bind(node.capacity.cpu as i64).bind(node.capacity.memory as i64).bind(node.capacity.gpu as i64)
            .bind(node.allocatable.cpu as i64).bind(node.allocatable.memory as i64).bind(node.allocatable.gpu as i64)
            .bind(&node.info.arch).bind(&node.info.os).bind(Utc::now().timestamp()).bind(&node.name)
            .fetch_one(&self.pool).await.map_err(|e|if matches!(e,sqlx::Error::RowNotFound){missing("node",&node.name)}else{storage(e)})?;
        dao.try_into()
    }
    async fn delete_node(&self, name: &str) -> Result<(), FlameError> {
        sqlx::query("DELETE FROM nodes WHERE name=?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError> {
        let rows: Vec<NodeDao> = sqlx::query_as("SELECT * FROM nodes")
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
        rows.into_iter().map(Node::try_from).collect()
    }
    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        let dao:ExecutorDao=sqlx::query_as("INSERT INTO executors (id,workspace,name,node,application,resreq_cpu,resreq_memory,resreq_gpu,shim,task,session,creation_time,state) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING *")
            .bind(&executor.id).bind(&executor.workspace).bind(&executor.name).bind(&executor.node).bind(&executor.application)
            .bind(executor.resreq.cpu as i64).bind(executor.resreq.memory as i64).bind(executor.resreq.gpu as i64)
            .bind(i32::from(executor.shim)).bind(&executor.task).bind(&executor.session).bind(executor.creation_time.timestamp())
            .bind(i32::from(executor.state)).fetch_one(&self.pool).await.map_err(|e|unique(e,"executor"))?;
        dao.try_into()
    }
    async fn get_executor(&self, gid: &ExecutorGID) -> Result<Option<Executor>, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.executor.as_str();
        let dao: Option<ExecutorDao> =
            sqlx::query_as("SELECT * FROM executors WHERE workspace=? AND name=?")
                .bind(workspace)
                .bind(name)
                .fetch_optional(&self.pool)
                .await
                .map_err(storage)?;
        dao.map(Executor::try_from).transpose()
    }
    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        let dao:ExecutorDao=sqlx::query_as("UPDATE executors SET node=?,application=?,resreq_cpu=?,resreq_memory=?,resreq_gpu=?,shim=?,task=?,session=?,state=? WHERE workspace=? AND name=? RETURNING *")
            .bind(&executor.node).bind(&executor.application).bind(executor.resreq.cpu as i64)
            .bind(executor.resreq.memory as i64).bind(executor.resreq.gpu as i64).bind(i32::from(executor.shim))
            .bind(&executor.task).bind(&executor.session).bind(i32::from(executor.state))
            .bind(&executor.workspace).bind(&executor.name).fetch_one(&self.pool).await
            .map_err(|e|if matches!(e,sqlx::Error::RowNotFound){missing("executor",&executor.name)}else{storage(e)})?;
        dao.try_into()
    }
    async fn update_executor_state(
        &self,
        gid: &ExecutorGID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.executor.as_str();
        let dao: ExecutorDao =
            sqlx::query_as("UPDATE executors SET state=? WHERE workspace=? AND name=? RETURNING *")
                .bind(i32::from(state))
                .bind(workspace)
                .bind(name)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| {
                    if matches!(e, sqlx::Error::RowNotFound) {
                        missing("executor", name)
                    } else {
                        storage(e)
                    }
                })?;
        dao.try_into()
    }
    async fn delete_executor(&self, gid: &ExecutorGID) -> Result<(), FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.executor.as_str();
        sqlx::query("DELETE FROM executors WHERE workspace=? AND name=?")
            .bind(workspace)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        let rows: Vec<ExecutorDao> = match node {
            Some(n) => sqlx::query_as("SELECT * FROM executors WHERE node=?")
                .bind(n)
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?,
            None => sqlx::query_as("SELECT * FROM executors")
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?,
        };
        rows.into_iter().map(Executor::try_from).collect()
    }
}

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

use crate::model::Executor;
use crate::FlameError;
use common::apis::{
    Application, ApplicationAttributes, ApplicationID, CommonData, Event, ExecutorID,
    ExecutorState, Node, Role, Session, SessionAttributes, SessionID, Task, TaskGID, TaskInput,
    TaskOutput, TaskResult, TaskState, User, Workspace,
};

mod filesystem;
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
    async fn unregister_application(&self, id: String) -> Result<(), FlameError>;
    async fn update_application(
        &self,
        id: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError>;
    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError>;
    async fn find_application(&self) -> Result<Vec<Application>, FlameError>;

    // Session operations
    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError>;
    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError>;
    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError>;
    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError>;
    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError>;
    async fn find_session(&self) -> Result<Vec<Session>, FlameError>;

    // Task operations
    async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
    ) -> Result<Task, FlameError>;

    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError>;

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError>;

    async fn delete_task(&self, gid: TaskGID) -> Result<Task, FlameError>;

    async fn update_task_state(
        &self,
        gid: TaskGID,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<Task, FlameError>;

    async fn update_task_result(
        &self,
        gid: TaskGID,
        task_result: TaskResult,
    ) -> Result<Task, FlameError>;

    async fn find_tasks(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError>;

    // Node operations
    async fn create_node(&self, node: &Node) -> Result<Node, FlameError>;
    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError>;
    async fn update_node(&self, node: &Node) -> Result<Node, FlameError>;
    async fn delete_node(&self, name: &str) -> Result<(), FlameError>;
    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError>;

    // Executor operations
    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError>;
    async fn get_executor(&self, id: &ExecutorID) -> Result<Option<Executor>, FlameError>;
    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError>;
    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError>;
    async fn delete_executor(&self, id: &ExecutorID) -> Result<(), FlameError>;
    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError>;

    // User operations
    async fn get_user(&self, name: &str) -> Result<Option<User>, FlameError>;
    async fn get_user_by_cn(&self, cn: &str) -> Result<Option<User>, FlameError>;
    async fn get_user_roles(&self, user_name: &str) -> Result<Vec<Role>, FlameError>;
    async fn create_user(&self, user: &User) -> Result<User, FlameError>;
    async fn update_user(
        &self,
        user: &User,
        assign_roles: &[String],
        revoke_roles: &[String],
    ) -> Result<User, FlameError>;
    async fn delete_user(&self, name: &str) -> Result<(), FlameError>;
    async fn find_users(&self, role_filter: Option<&str>) -> Result<Vec<User>, FlameError>;

    // Role operations
    async fn get_role(&self, name: &str) -> Result<Option<Role>, FlameError>;
    async fn create_role(&self, role: &Role) -> Result<Role, FlameError>;
    async fn update_role(&self, role: &Role) -> Result<Role, FlameError>;
    async fn delete_role(&self, name: &str) -> Result<(), FlameError>;
    async fn find_roles(&self, workspace_filter: Option<&str>) -> Result<Vec<Role>, FlameError>;

    // Workspace operations
    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, FlameError>;
    async fn create_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError>;
    async fn update_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError>;
    async fn delete_workspace(&self, name: &str) -> Result<(), FlameError>;
    async fn find_workspaces(&self) -> Result<Vec<Workspace>, FlameError>;
}

/// Connect to a storage engine based on the URL scheme.
///
/// Supported URL schemes:
/// - `sqlite://` or `sqlite:` - SQLite database (default)
/// - `filesystem://`, `file://`, `fs://` - Filesystem-based storage
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
/// ```
pub async fn connect(url: &str) -> Result<EnginePtr, FlameError> {
    if url.starts_with("filesystem://") || url.starts_with("file://") || url.starts_with("fs://") {
        tracing::info!("Using filesystem storage engine: {}", url);
        filesystem::FilesystemEngine::new_ptr(url).await
    } else {
        tracing::info!("Using SQLite storage engine: {}", url);
        sqlite::SqliteEngine::new_ptr(url).await
    }
}

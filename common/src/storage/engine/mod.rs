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

use crate::apis::{
    Application, ApplicationAttributes, ApplicationState, ExecutorGID, ExecutorState, Node,
    Session, SessionAttributes, SessionGID, Task, TaskInput, TaskOptions, TaskResult, TaskState,
    Workspace,
};
use crate::apis::{ApplicationFilter, Executor};
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
    async fn create_workspace(&self, name: String) -> Result<Workspace, FlameError>;
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, FlameError>;
    // Application operations
    async fn register_application(
        &self,
        workspace: String,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError>;
    async fn update_application_state(
        &self,
        workspace: &str,
        name: &str,
        state: ApplicationState,
    ) -> Result<Application, FlameError>;
    async fn delete_application(&self, workspace: &str, name: &str) -> Result<(), FlameError>;
    async fn update_application(
        &self,
        workspace: &str,
        name: &str,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError>;
    async fn get_application(&self, workspace: &str, name: &str)
        -> Result<Application, FlameError>;
    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError>;
    // Session operations
    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError>;
    async fn get_session(&self, session: &SessionGID) -> Result<Session, FlameError>;
    async fn open_session(
        &self,
        session: &SessionGID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError>;
    async fn close_session(&self, session: &SessionGID) -> Result<Session, FlameError>;
    async fn delete_session(&self, session: &SessionGID) -> Result<Session, FlameError>;
    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError>;

    // Task operations
    async fn create_task(
        &self,
        session: &SessionGID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError>;

    #[allow(dead_code)]
    async fn get_task(&self, session: &SessionGID, task: &str) -> Result<Task, FlameError>;

    async fn retry_task(&self, session: &SessionGID, task: &str) -> Result<Task, FlameError>;

    async fn update_task_state(
        &self,
        session: &SessionGID,
        task: &str,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<Task, FlameError>;

    async fn update_task_result(
        &self,
        session: &SessionGID,
        task: &str,
        task_result: TaskResult,
    ) -> Result<Task, FlameError>;

    async fn find_tasks(&self, session: &SessionGID) -> Result<Vec<Task>, FlameError>;

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
    async fn get_executor(&self, executor: &ExecutorGID) -> Result<Option<Executor>, FlameError>;
    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError>;
    #[allow(dead_code)]
    async fn update_executor_state(
        &self,
        executor: &ExecutorGID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError>;
    async fn delete_executor(&self, executor: &ExecutorGID) -> Result<(), FlameError>;
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

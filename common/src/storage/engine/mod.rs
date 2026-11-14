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
    Application, ApplicationAttributes, ApplicationID, ApplicationState, ExecutorID, ExecutorState,
    Node, Session, SessionAttributes, SessionID, SessionState, Task, TaskGID, TaskInput,
    TaskOptions, TaskResult, TaskState,
};
use crate::apis::{ApplicationFilter, Executor, TaskFilter};
use crate::FlameError;

mod filesystem;
mod none;

#[cfg(test)]
mod tests;

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
        Err(FlameError::InvalidConfig(format!(
            "Unsupported storage URL <{url}>; use fs://, file://, filesystem:// or none"
        )))
    }
}

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

//! None Storage Engine - A minimal engine for non-recoverable workloads.
//!
//! This engine does NOT persist any data. The controller's in-memory cache is the
//! source of truth. The NoneEngine only maintains task ID counters for allocation.
//!
//! Use cases:
//! - Real-time processing where task results are consumed immediately
//! - High-throughput scenarios where disk I/O is a bottleneck
//! - Development and testing
//!
//! Limitations:
//! - All data is lost on session-manager restart
//! - Evicted sessions are permanently lost (no persistence to fall back to)

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use stdng::{lock_ptr, MutexPtr};

use crate::model::Executor;
use crate::FlameError;
use common::apis::{
    Application, ApplicationAttributes, ApplicationID, ExecutorID, ExecutorState, Node, Session,
    SessionAttributes, SessionID, SessionState, SessionStatus, Task, TaskGID, TaskID, TaskInput,
    TaskOutput, TaskResult, TaskState,
};

use super::{Engine, EnginePtr};

/// None Storage Engine - stores nothing, only allocates task IDs.
///
/// The controller cache is the source of truth for all data.
/// This engine only maintains per-session task ID counters.
pub struct NoneEngine {
    /// Per-session task ID counters for allocation
    task_counters: MutexPtr<HashMap<SessionID, Arc<AtomicI64>>>,
    /// In-memory application cache (required for get_application)
    applications: MutexPtr<HashMap<ApplicationID, Application>>,
}

impl NoneEngine {
    /// Create a new NoneEngine instance.
    pub async fn new_ptr(_url: &str) -> Result<EnginePtr, FlameError> {
        tracing::info!("Using none storage engine (no persistence)");
        Ok(Arc::new(Self {
            task_counters: stdng::new_ptr(HashMap::new()),
            applications: stdng::new_ptr(HashMap::new()),
        }))
    }

    /// Allocate the next task ID for a session.
    /// Task IDs are sequential starting from 1.
    fn next_task_id(&self, ssn_id: &SessionID) -> Result<TaskID, FlameError> {
        let mut counters = lock_ptr!(self.task_counters)?;
        let counter = counters
            .entry(ssn_id.clone())
            .or_insert_with(|| Arc::new(AtomicI64::new(0)));
        Ok(counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Initialize task counter for a new session.
    fn init_task_counter(&self, ssn_id: &SessionID) -> Result<(), FlameError> {
        let mut counters = lock_ptr!(self.task_counters)?;
        counters.insert(ssn_id.clone(), Arc::new(AtomicI64::new(0)));
        Ok(())
    }

    /// Clean up task counter when session is deleted.
    fn remove_task_counter(&self, ssn_id: &SessionID) -> Result<(), FlameError> {
        let mut counters = lock_ptr!(self.task_counters)?;
        counters.remove(ssn_id);
        Ok(())
    }
}

#[async_trait]
impl Engine for NoneEngine {
    // ========== Application operations ==========

    async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        let app = Application {
            name: name.clone(),
            version: 1,
            state: common::apis::ApplicationState::Enabled,
            creation_time: Utc::now(),
            shim: attr.shim,
            image: attr.image,
            description: attr.description,
            labels: attr.labels,
            command: attr.command,
            arguments: attr.arguments,
            environments: attr.environments,
            working_directory: attr.working_directory,
            max_instances: attr.max_instances,
            delay_release: attr.delay_release,
            schema: attr.schema,
            url: attr.url,
            installer: attr.installer,
        };

        let mut apps = lock_ptr!(self.applications)?;
        apps.insert(name, app.clone());

        Ok(app)
    }

    async fn unregister_application(&self, id: String) -> Result<(), FlameError> {
        let mut apps = lock_ptr!(self.applications)?;
        apps.remove(&id);
        Ok(())
    }

    async fn update_application(
        &self,
        id: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        let mut apps = lock_ptr!(self.applications)?;
        let app = apps
            .get(&id)
            .ok_or_else(|| FlameError::NotFound(format!("application <{}>", id)))?;

        let updated = Application {
            name: id.clone(),
            version: app.version + 1,
            state: app.state,
            creation_time: app.creation_time,
            shim: attr.shim,
            image: attr.image,
            description: attr.description,
            labels: attr.labels,
            command: attr.command,
            arguments: attr.arguments,
            environments: attr.environments,
            working_directory: attr.working_directory,
            max_instances: attr.max_instances,
            delay_release: attr.delay_release,
            schema: attr.schema,
            url: attr.url,
            installer: attr.installer,
        };

        apps.insert(id, updated.clone());
        Ok(updated)
    }

    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        let apps = lock_ptr!(self.applications)?;
        apps.get(&id)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("application <{}>", id)))
    }

    async fn find_application(&self) -> Result<Vec<Application>, FlameError> {
        let apps = lock_ptr!(self.applications)?;
        Ok(apps.values().cloned().collect())
    }

    // ========== Session operations ==========

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        self.init_task_counter(&attr.id)?;

        Ok(Session {
            id: attr.id,
            application: attr.application,
            slots: attr.slots,
            common_data: attr.common_data,
            min_instances: attr.min_instances,
            max_instances: attr.max_instances,
            batch_size: attr.batch_size.max(1),
            priority: attr.priority,
            status: SessionStatus {
                state: SessionState::Open,
            },
            creation_time: Utc::now(),
            completion_time: None,
            version: 1,
            tasks: HashMap::new(),
            tasks_index: HashMap::new(),
            events: vec![],
        })
    }

    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        Err(FlameError::NotFound(format!("session <{}>", id)))
    }

    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        match spec {
            Some(attr) => self.create_session(attr).await,
            None => Err(FlameError::NotFound(format!("session <{}>", id))),
        }
    }

    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        Err(FlameError::NotFound(format!("session <{}>", id)))
    }

    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        self.remove_task_counter(&id)?;
        Err(FlameError::NotFound(format!("session <{}>", id)))
    }

    async fn find_session(&self) -> Result<Vec<Session>, FlameError> {
        Ok(vec![])
    }

    // ========== Task operations ==========

    async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
    ) -> Result<Task, FlameError> {
        let task_id = self.next_task_id(&ssn_id)?;

        Ok(Task {
            ssn_id,
            id: task_id,
            version: 1,
            state: TaskState::Pending,
            creation_time: Utc::now(),
            completion_time: None,
            input: task_input,
            output: None,
            events: vec![],
        })
    }

    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        Err(FlameError::NotFound(format!("task <{}>", gid)))
    }

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        Err(FlameError::NotFound(format!("task <{}>", gid)))
    }

    async fn delete_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        Err(FlameError::NotFound(format!("task <{}>", gid)))
    }

    async fn update_task_state(
        &self,
        gid: TaskGID,
        _task_state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        Err(FlameError::NotFound(format!("task <{}>", gid)))
    }

    async fn update_task_result(
        &self,
        gid: TaskGID,
        _task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        Err(FlameError::NotFound(format!("task <{}>", gid)))
    }

    async fn find_tasks(&self, _ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        Ok(vec![])
    }

    // ========== Node operations ==========

    async fn create_node(&self, node: &Node) -> Result<Node, FlameError> {
        Ok(node.clone())
    }

    async fn get_node(&self, _name: &str) -> Result<Option<Node>, FlameError> {
        Ok(None)
    }

    async fn update_node(&self, node: &Node) -> Result<Node, FlameError> {
        Ok(node.clone())
    }

    async fn delete_node(&self, _name: &str) -> Result<(), FlameError> {
        Ok(())
    }

    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError> {
        Ok(vec![])
    }

    // ========== Executor operations ==========

    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        Ok(executor.clone())
    }

    async fn get_executor(&self, _id: &ExecutorID) -> Result<Option<Executor>, FlameError> {
        Ok(None)
    }

    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        Ok(executor.clone())
    }

    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        _state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        Err(FlameError::NotFound(format!("executor <{}>", id)))
    }

    async fn delete_executor(&self, _id: &ExecutorID) -> Result<(), FlameError> {
        Ok(())
    }

    async fn find_executors(&self, _node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_none_engine_create_session() {
        let engine = NoneEngine::new_ptr("none").await.unwrap();

        let attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 1,
            max_instances: None,
            batch_size: 1,
            priority: 0,
        };

        let session = engine.create_session(attr).await.unwrap();
        assert_eq!(session.id, "test-session");
        assert_eq!(session.application, "test-app");
        assert_eq!(session.status.state, SessionState::Open);
    }

    #[tokio::test]
    async fn test_none_engine_get_session_returns_not_found() {
        let engine = NoneEngine::new_ptr("none").await.unwrap();

        let result = engine.get_session("test-session".to_string()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FlameError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_none_engine_find_session_returns_empty() {
        let engine = NoneEngine::new_ptr("none").await.unwrap();

        let sessions = engine.find_session().await.unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_none_engine_task_id_allocation() {
        let engine = NoneEngine::new_ptr("none").await.unwrap();

        let attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 1,
            max_instances: None,
            batch_size: 1,
            priority: 0,
        };
        engine.create_session(attr).await.unwrap();

        let task1 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task1.id, 1);

        let task2 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task2.id, 2);

        let task3 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task3.id, 3);
    }

    #[tokio::test]
    async fn test_none_engine_task_id_per_session() {
        let engine = NoneEngine::new_ptr("none").await.unwrap();

        let attr1 = SessionAttributes {
            id: "session-1".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 1,
            max_instances: None,
            batch_size: 1,
            priority: 0,
        };
        engine.create_session(attr1).await.unwrap();

        let attr2 = SessionAttributes {
            id: "session-2".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 1,
            max_instances: None,
            batch_size: 1,
            priority: 0,
        };
        engine.create_session(attr2).await.unwrap();

        let task1_s1 = engine
            .create_task("session-1".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task1_s1.id, 1);

        let task1_s2 = engine
            .create_task("session-2".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task1_s2.id, 1);

        let task2_s1 = engine
            .create_task("session-1".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task2_s1.id, 2);
    }

    #[tokio::test]
    async fn test_none_engine_delete_session_cleans_counter() {
        let engine = NoneEngine::new_ptr("none").await.unwrap();

        let attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 1,
            max_instances: None,
            batch_size: 1,
            priority: 0,
        };
        engine.create_session(attr.clone()).await.unwrap();

        let task1 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task1.id, 1);

        let result = engine.delete_session("test-session".to_string()).await;
        assert!(result.is_err());

        engine.create_session(attr).await.unwrap();

        let task_new = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task_new.id, 1);
    }
}

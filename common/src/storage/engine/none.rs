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

//! In-memory storage engine for workloads that do not need restart persistence.
//!
//! All task records and payloads remain readable until their session is deleted
//! or the session-manager process exits. The scheduling cache can evict finished
//! tasks while this engine retains their history without disk I/O.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use stdng::{ensure, lock_ptr, MutexPtr};

use crate::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationState, ExecutorID, ExecutorState,
    Node, Session, SessionAttributes, SessionID, SessionState, SessionStatus, Task, TaskGID,
    TaskID, TaskInput, TaskOptions, TaskResult, TaskState,
};
use crate::apis::{ApplicationFilter, Executor, TaskFilter};
use crate::FlameError;

use super::{Engine, EnginePtr};

/// Session metadata and task history share one lock for atomic lifecycle changes.
struct SessionEntry {
    metadata: Session,
    tasks: BTreeMap<TaskID, Task>,
    next_task_id: TaskID,
}

/// Stores complete task history in memory without disk persistence.
pub struct NoneEngine {
    applications: MutexPtr<HashMap<ApplicationID, Application>>,
    sessions: MutexPtr<HashMap<SessionID, MutexPtr<SessionEntry>>>,
}

impl NoneEngine {
    pub async fn new_ptr(_url: &str) -> Result<EnginePtr, FlameError> {
        tracing::info!("Using none storage engine (in-memory task history)");
        Ok(Arc::new(Self {
            applications: stdng::new_ptr(HashMap::new()),
            sessions: stdng::new_ptr(HashMap::new()),
        }))
    }

    fn session_entry(&self, id: &str) -> Result<MutexPtr<SessionEntry>, FlameError> {
        lock_ptr!(self.sessions)?
            .get(id)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("session <{id}>")))
    }

    fn validate_session(&self, id: &str, entry: &MutexPtr<SessionEntry>) -> Result<(), FlameError> {
        ensure!(
            lock_ptr!(self.sessions)?
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, entry)),
            FlameError::NotFound(format!("session <{id}>"))
        )
    }

    /// Never wait for an entry while holding the directory lock. Validate after
    /// locking so a waiter cannot mutate a deleted or replaced session.
    fn with_session<T>(
        &self,
        id: &str,
        op: impl FnOnce(&mut SessionEntry) -> Result<T, FlameError>,
    ) -> Result<T, FlameError> {
        let shared = self.session_entry(id)?;
        let mut entry = lock_ptr!(shared)?;
        self.validate_session(id, &shared)?;
        op(&mut entry)
    }

    fn mutate_task(
        &self,
        gid: TaskGID,
        state: TaskState,
        output: Option<Option<bytes::Bytes>>,
        retry: bool,
    ) -> Result<Task, FlameError> {
        self.with_session(&gid.ssn_id, |entry| {
            ensure!(
                !entry.metadata.is_closed() && (!retry || entry.metadata.is_open()),
                FlameError::InvalidState(format!(
                    "Session <{}> does not accept task updates",
                    gid.ssn_id
                ))
            )?;
            let old_state = entry
                .tasks
                .get(&gid.task_id)
                .ok_or_else(|| FlameError::NotFound(format!("task <{gid}>")))?
                .state;
            entry.metadata.task_counts.transition(old_state, state)?;
            let task = entry.tasks.get_mut(&gid.task_id).unwrap();
            task.state = state;
            task.version += 1;
            task.completion_time = (output.is_some() || state.is_terminal()).then(Utc::now);
            if let Some(output) = output {
                task.output = output;
            }
            Ok(task.clone())
        })
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
            state: ApplicationState::Enabled,
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
        if apps.contains_key(&name) {
            return Err(FlameError::AlreadyExist(format!(
                "application <{name}> already exists"
            )));
        }
        apps.insert(name, app.clone());

        Ok(app)
    }

    async fn update_application_state(
        &self,
        id: ApplicationID,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let mut apps = lock_ptr!(self.applications)?;
        let app = apps
            .get_mut(&id)
            .ok_or_else(|| FlameError::NotFound(format!("application <{id}>")))?;
        if app.state != state {
            app.state = state;
            app.version += 1;
        }
        Ok(app.clone())
    }

    async fn delete_application(&self, id: ApplicationID) -> Result<(), FlameError> {
        let mut apps = lock_ptr!(self.applications)?;
        let app = apps
            .get(&id)
            .ok_or_else(|| FlameError::NotFound(format!("application <{id}>")))?;
        if app.state != ApplicationState::Disabled {
            return Err(FlameError::InvalidState(format!(
                "application <{id}> is not disabled"
            )));
        }

        let sessions = {
            let directory = lock_ptr!(self.sessions)?;
            directory
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect::<Vec<_>>()
        };
        for (session_id, shared) in sessions {
            let entry = lock_ptr!(shared)?;
            match self.validate_session(&session_id, &shared) {
                Ok(()) => {}
                Err(FlameError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            }
            if entry.metadata.application == id {
                return Err(FlameError::InvalidState(format!(
                    "application <{id}> still has sessions"
                )));
            }
        }
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
        if app.state != ApplicationState::Enabled {
            return Err(FlameError::InvalidState(format!(
                "application <{id}> is not enabled"
            )));
        }

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

    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        let apps = lock_ptr!(self.applications)?;
        Ok(apps
            .values()
            .filter(|app| {
                filter.is_none_or(|filter| filter.state.is_none_or(|state| app.state == state))
            })
            .cloned()
            .collect())
    }

    // ========== Session operations ==========

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        let session = Session {
            id: attr.id,
            application: attr.application,
            common_data: attr.common_data,
            tokens: attr.tokens,
            min_instances: attr.min_instances,
            max_instances: attr.max_instances,
            batch_size: attr.batch_size,
            priority: attr.priority,
            resreq: attr.resreq,
            status: SessionStatus {
                state: SessionState::Open,
            },
            creation_time: Utc::now(),
            completion_time: None,
            version: 1,
            tasks: Default::default(),
            task_counts: Default::default(),
            tasks_index: HashMap::new(),
            events: vec![],
            retry_count: 0,
        };
        let mut sessions = lock_ptr!(self.sessions)?;
        ensure!(
            !sessions.contains_key(&session.id),
            FlameError::AlreadyExist(format!("session <{}> already exists", session.id))
        )?;
        sessions.insert(
            session.id.clone(),
            stdng::new_ptr(SessionEntry {
                metadata: session.clone(),
                tasks: BTreeMap::new(),
                next_task_id: 1,
            }),
        );
        Ok(session)
    }

    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        self.with_session(&id, |entry| Ok(entry.metadata.clone()))
    }

    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        match self.get_session(id.clone()).await {
            Ok(session) => {
                ensure!(
                    session.is_open(),
                    FlameError::InvalidState(format!("Session <{id}> is not open"))
                )?;
                if let Some(ref attr) = spec {
                    session.validate_spec(attr)?;
                }
                Ok(session)
            }
            Err(FlameError::NotFound(_)) => match spec {
                Some(attr) => self.create_session(attr).await,
                None => Err(FlameError::NotFound(format!("session <{id}>"))),
            },
            Err(error) => Err(error),
        }
    }

    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        self.with_session(&id, |entry| {
            if !entry.metadata.is_closed() {
                ensure!(
                    entry.metadata.task_counts.running == 0,
                    FlameError::Storage("Cannot close session with running tasks".to_string())
                )?;
                let now = Utc::now();
                for task in entry.tasks.values_mut() {
                    if task.state == TaskState::Pending {
                        entry
                            .metadata
                            .task_counts
                            .transition(TaskState::Pending, TaskState::Cancelled)?;
                        task.state = TaskState::Cancelled;
                        task.completion_time = Some(now);
                        task.version += 1;
                    }
                }
                entry.metadata.status.state = SessionState::Closed;
                entry.metadata.completion_time = Some(now);
                entry.metadata.version += 1;
            }
            Ok(entry.metadata.clone())
        })
    }

    async fn update_session_state(
        &self,
        id: SessionID,
        state: SessionState,
    ) -> Result<Session, FlameError> {
        self.with_session(&id, |entry| {
            ensure!(
                !entry.metadata.is_closed(),
                FlameError::InvalidState(format!("session <{id}> is closed"))
            )?;
            entry.metadata.status.state = state;
            Ok(entry.metadata.clone())
        })
    }

    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let shared = self.session_entry(&id)?;
        let metadata = {
            let entry = lock_ptr!(shared)?;
            {
                let mut directory = lock_ptr!(self.sessions)?;
                ensure!(
                    directory
                        .get(&id)
                        .is_some_and(|current| Arc::ptr_eq(current, &shared)),
                    FlameError::NotFound(format!("session <{id}>"))
                )?;
                directory.remove(&id);
            }
            entry.metadata.clone()
        };
        Ok(metadata)
    }

    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError> {
        let entries = {
            let directory = lock_ptr!(self.sessions)?;
            directory
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect::<Vec<_>>()
        };
        let mut sessions = Vec::with_capacity(entries.len());
        for (id, shared) in entries {
            let entry = lock_ptr!(shared)?;
            match self.validate_session(&id, &shared) {
                Ok(()) => sessions.push(entry.metadata.clone()),
                Err(FlameError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(sessions)
    }

    // ========== Task operations ==========

    async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        self.with_session(&ssn_id, |entry| {
            ensure!(
                !entry.metadata.is_closed(),
                FlameError::InvalidState(format!("Session <{ssn_id}> is closed"))
            )?;
            let task = Task {
                ssn_id: ssn_id.clone(),
                id: entry.next_task_id,
                version: 1,
                state: TaskState::Pending,
                creation_time: Utc::now(),
                completion_time: None,
                input: task_input,
                output: None,
                affinity: options.unwrap_or_default().affinity,
                events: vec![],
            };
            entry.tasks.insert(task.id, task.clone());
            entry.next_task_id += 1;
            entry.metadata.task_counts.increment(TaskState::Pending);
            Ok(task)
        })
    }

    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        self.with_session(&gid.ssn_id, |entry| {
            entry
                .tasks
                .get(&gid.task_id)
                .cloned()
                .ok_or_else(|| FlameError::NotFound(format!("task <{gid}>")))
        })
    }

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        self.mutate_task(gid, TaskState::Pending, None, true)
    }

    async fn update_task_state(
        &self,
        current: Task,
        task_state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        self.mutate_task(current.gid(), task_state, None, false)
    }

    async fn update_task_result(
        &self,
        current: Task,
        task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        self.mutate_task(
            current.gid(),
            task_result.state,
            Some(task_result.output),
            false,
        )
    }

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>, FlameError> {
        self.with_session(&filter.session, |entry| {
            Ok(entry
                .tasks
                .values()
                .filter(|task| {
                    filter
                        .states
                        .as_ref()
                        .is_none_or(|states| states.contains(&task.state))
                })
                .cloned()
                .collect())
        })
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
mod tests;

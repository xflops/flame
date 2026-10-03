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

//! Non-persistent engine. It retains lifecycle names and allocates task numbers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use stdng::{lock_ptr, MutexPtr};

use super::{Engine, EnginePtr};
use crate::apis::{
    new_metadata_id, Application, ApplicationAttributes, ApplicationFilter, ApplicationState,
    Executor, ExecutorGID, ExecutorState, Node, Session, SessionAttributes, SessionGID,
    SessionState, SessionStatus, Task, TaskInput, TaskName, TaskOptions, TaskResult, TaskState,
    Workspace, DEFAULT_WORKSPACE,
};
use crate::FlameError;

type ScopedName = (String, String);

pub struct NoneEngine {
    workspaces: MutexPtr<HashMap<String, Workspace>>,
    applications: MutexPtr<HashMap<ScopedName, Application>>,
    sessions: MutexPtr<HashMap<ScopedName, Session>>,
    task_counters: MutexPtr<HashMap<ScopedName, TaskName>>,
}

impl NoneEngine {
    pub async fn new_ptr(_url: &str) -> Result<EnginePtr, FlameError> {
        let default = Workspace {
            name: DEFAULT_WORKSPACE.into(),
            create_at: Utc::now(),
        };
        Ok(Arc::new(Self {
            workspaces: stdng::new_ptr(HashMap::from([(default.name.clone(), default)])),
            applications: stdng::new_ptr(HashMap::new()),
            sessions: stdng::new_ptr(HashMap::new()),
            task_counters: stdng::new_ptr(HashMap::new()),
        }))
    }

    fn key(workspace: &str, name: &str) -> ScopedName {
        (workspace.to_string(), name.to_string())
    }

    fn require_workspace(&self, name: &str) -> Result<(), FlameError> {
        if lock_ptr!(self.workspaces)?.contains_key(name) {
            Ok(())
        } else {
            Err(FlameError::NotFound(format!("workspace {name}")))
        }
    }
}

#[async_trait]
impl Engine for NoneEngine {
    async fn create_workspace(&self, name: String) -> Result<Workspace, FlameError> {
        let mut workspaces = lock_ptr!(self.workspaces)?;
        if workspaces.contains_key(&name) {
            return Err(FlameError::AlreadyExist(format!("workspace {name}")));
        }
        let workspace = Workspace {
            name: name.clone(),
            create_at: Utc::now(),
        };
        workspaces.insert(name, workspace.clone());
        Ok(workspace)
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        let mut result: Vec<_> = lock_ptr!(self.workspaces)?.values().cloned().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    async fn register_application(
        &self,
        workspace: String,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        crate::apis::validate_application_url(&workspace, attr.url.as_deref())?;
        self.require_workspace(&workspace)?;
        let mut apps = lock_ptr!(self.applications)?;
        let key = Self::key(&workspace, &name);
        if apps.contains_key(&key) {
            return Err(FlameError::AlreadyExist(format!(
                "application {workspace}/{name}"
            )));
        }
        let app = Application {
            id: new_metadata_id(),
            workspace,
            name,
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
        apps.insert(key, app.clone());
        Ok(app)
    }

    async fn update_application_state(
        &self,
        workspace: &str,
        name: &str,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let mut apps = lock_ptr!(self.applications)?;
        let app = apps
            .get_mut(&Self::key(workspace, name))
            .ok_or_else(|| FlameError::NotFound(format!("application {workspace}/{name}")))?;
        if app.state != state {
            app.state = state;
            app.version += 1;
        }
        Ok(app.clone())
    }

    async fn delete_application(&self, workspace: &str, name: &str) -> Result<(), FlameError> {
        let key = Self::key(workspace, name);
        let mut apps = lock_ptr!(self.applications)?;
        let app = apps
            .get(&key)
            .ok_or_else(|| FlameError::NotFound(format!("application {workspace}/{name}")))?;
        if app.state != ApplicationState::Disabled {
            return Err(FlameError::InvalidState(format!(
                "application {workspace}/{name} is enabled"
            )));
        }
        if lock_ptr!(self.sessions)?
            .values()
            .any(|ssn| ssn.workspace == workspace && ssn.application == name)
        {
            return Err(FlameError::InvalidState(format!(
                "application {workspace}/{name} still has sessions"
            )));
        }
        apps.remove(&key);
        Ok(())
    }

    async fn update_application(
        &self,
        workspace: &str,
        name: &str,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        crate::apis::validate_application_url(workspace, attr.url.as_deref())?;
        let mut apps = lock_ptr!(self.applications)?;
        let app = apps
            .get_mut(&Self::key(workspace, name))
            .ok_or_else(|| FlameError::NotFound(format!("application {workspace}/{name}")))?;
        if app.state != ApplicationState::Enabled {
            return Err(FlameError::InvalidState(format!(
                "application {workspace}/{name} is disabled"
            )));
        }
        if lock_ptr!(self.sessions)?.values().any(|ssn| {
            ssn.workspace == workspace
                && ssn.application == name
                && ssn.status.state == SessionState::Open
        }) {
            return Err(FlameError::InvalidState(format!(
                "application {workspace}/{name} has open sessions"
            )));
        }
        app.version += 1;
        app.shim = attr.shim;
        app.image = attr.image;
        app.description = attr.description;
        app.labels = attr.labels;
        app.command = attr.command;
        app.arguments = attr.arguments;
        app.environments = attr.environments;
        app.working_directory = attr.working_directory;
        app.max_instances = attr.max_instances;
        app.delay_release = attr.delay_release;
        app.schema = attr.schema;
        app.url = attr.url;
        app.installer = attr.installer;
        Ok(app.clone())
    }

    async fn get_application(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Application, FlameError> {
        lock_ptr!(self.applications)?
            .get(&Self::key(workspace, name))
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("application {workspace}/{name}")))
    }

    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        Ok(lock_ptr!(self.applications)?
            .values()
            .filter(|app| {
                filter.is_none_or(|f| {
                    f.workspace == app.workspace && f.state.is_none_or(|s| s == app.state)
                })
            })
            .cloned()
            .collect())
    }

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        self.require_workspace(&attr.workspace)?;
        self.get_application(&attr.workspace, &attr.application)
            .await?;
        let key = Self::key(&attr.workspace, &attr.name);
        let mut sessions = lock_ptr!(self.sessions)?;
        if sessions.contains_key(&key) {
            return Err(FlameError::AlreadyExist(format!(
                "session {}/{}",
                attr.workspace, attr.name
            )));
        }
        let ssn = Session {
            id: new_metadata_id(),
            workspace: attr.workspace,
            name: attr.name,
            application: attr.application,
            version: 1,
            common_data: attr.common_data,
            tokens: attr.tokens,
            tasks: HashMap::new(),
            tasks_index: HashMap::new(),
            creation_time: Utc::now(),
            completion_time: None,
            events: vec![],
            status: SessionStatus {
                state: SessionState::Open,
            },
            min_instances: attr.min_instances,
            max_instances: attr.max_instances,
            batch_size: attr.batch_size,
            priority: attr.priority,
            resreq: attr.resreq,
            retry_count: 0,
        };
        sessions.insert(key.clone(), ssn.clone());
        lock_ptr!(self.task_counters)?.insert(key, 0);
        Ok(ssn)
    }

    async fn get_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let name = gid.session.as_str();
        Err(FlameError::NotFound(format!(
            "session {name} not retained by none engine"
        )))
    }

    async fn open_session(
        &self,
        gid: &SessionGID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        match spec {
            Some(attr) if attr.workspace == workspace && attr.name == name => {
                self.create_session(attr).await
            }
            _ => Err(FlameError::NotFound(format!("session {workspace}/{name}"))),
        }
    }

    async fn close_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        if let Some(ssn) = lock_ptr!(self.sessions)?.get_mut(&Self::key(workspace, name)) {
            if ssn.status.state == SessionState::Open {
                ssn.status.state = SessionState::Closed;
                ssn.completion_time = Some(Utc::now());
                ssn.version += 1;
            }
        }
        Err(FlameError::NotFound(format!(
            "session {workspace}/{name} not retained by none engine"
        )))
    }

    async fn delete_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let workspace = gid.workspace.as_str();
        let name = gid.session.as_str();
        let key = Self::key(workspace, name);
        let ssn = lock_ptr!(self.sessions)?
            .remove(&key)
            .ok_or_else(|| FlameError::NotFound(format!("session {workspace}/{name}")))?;
        lock_ptr!(self.task_counters)?.remove(&key);
        Ok(ssn)
    }

    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError> {
        Ok(vec![])
    }

    async fn create_task(
        &self,
        gid: &SessionGID,
        input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        let key = Self::key(workspace, session);
        let ssn = lock_ptr!(self.sessions)?
            .get(&key)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("session {workspace}/{session}")))?;
        if ssn.status.state != SessionState::Open {
            return Err(FlameError::InvalidState("session closed".into()));
        }
        let mut counters = lock_ptr!(self.task_counters)?;
        let number = counters.entry(key).or_default();
        *number = number
            .checked_add(1)
            .ok_or_else(|| FlameError::Storage("task number overflow".into()))?;
        Ok(Task {
            id: new_metadata_id(),
            workspace: workspace.into(),
            session: session.into(),
            name: *number,
            version: 1,
            input,
            output: None,
            affinity: options.unwrap_or_default().affinity,
            creation_time: Utc::now(),
            completion_time: None,
            events: vec![],
            state: TaskState::Pending,
        })
    }

    async fn get_task(&self, gid: &SessionGID, task: &str) -> Result<Task, FlameError> {
        let workspace = gid.workspace.as_str();
        let session = gid.session.as_str();
        Err(FlameError::NotFound(format!(
            "task {workspace}/{session}/{task} not retained by none engine"
        )))
    }
    async fn retry_task(&self, gid: &SessionGID, task: &str) -> Result<Task, FlameError> {
        self.get_task(gid, task).await
    }
    async fn update_task_state(
        &self,
        gid: &SessionGID,
        task: &str,
        _state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        self.get_task(gid, task).await
    }
    async fn update_task_result(
        &self,
        gid: &SessionGID,
        task: &str,
        _result: TaskResult,
    ) -> Result<Task, FlameError> {
        self.get_task(gid, task).await
    }
    async fn find_tasks(&self, _gid: &SessionGID) -> Result<Vec<Task>, FlameError> {
        Ok(vec![])
    }

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
    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        Ok(executor.clone())
    }
    async fn get_executor(&self, _gid: &ExecutorGID) -> Result<Option<Executor>, FlameError> {
        Ok(None)
    }
    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        Ok(executor.clone())
    }
    async fn update_executor_state(
        &self,
        gid: &ExecutorGID,
        _state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        let name = gid.executor.as_str();
        Err(FlameError::NotFound(format!("executor {name}")))
    }
    async fn delete_executor(&self, _gid: &ExecutorGID) -> Result<(), FlameError> {
        Ok(())
    }
    async fn find_executors(&self, _node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        Ok(vec![])
    }
}

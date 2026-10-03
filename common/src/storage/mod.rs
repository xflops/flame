/*
Copyright 2025 The xflops Authors.
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

mod data;
mod object;

pub use data::DataStorage;
pub use data::Index;
pub use object::Filter;
pub use object::Object;
pub use object::ObjectId;
pub use object::ObjectStorage;

use chrono::Utc;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use uuid::Uuid;

use stdng::{lock_ptr, trace_fn, MutexPtr};

use crate::apis::{
    Application, ApplicationAttributes, ApplicationPtr, ApplicationState, Event, EventOwner,
    ExecutorGID, ExecutorState, Node, NodePtr, Session, SessionAttributes, SessionGID, SessionPtr,
    SessionState, Shim, Task, TaskInput, TaskName, TaskOptions, TaskPtr, TaskResult, TaskState,
    Workspace,
};
use crate::ctx::FlameClusterContext;
use crate::FlameError;

use crate::apis::{ApplicationFilter, Executor, ExecutorFilter, ExecutorPtr, SessionFilter};

use crate::events::{EventManagerPtr, FsEventManager, MemoryEventManager};
use crate::storage::engine::EnginePtr;

mod engine;

pub type StoragePtr = Arc<Storage>;

/// Domain records used by the controller to build its scheduler snapshot.
pub struct StorageSnapshot {
    pub applications: Vec<Application>,
    pub sessions: Vec<Session>,
    pub executors: Vec<Executor>,
    pub nodes: Vec<Node>,
}

#[derive(Clone)]
pub struct Storage {
    context: FlameClusterContext,
    engine: EnginePtr,
    sessions: MutexPtr<HashMap<(String, String), SessionPtr>>,
    executors: MutexPtr<HashMap<String, ExecutorPtr>>,
    nodes: MutexPtr<HashMap<String, NodePtr>>,
    applications: MutexPtr<HashMap<(String, String), ApplicationPtr>>,
    workspaces: MutexPtr<HashMap<String, Workspace>>,
    event_manager: EventManagerPtr,
    max_sessions: Option<usize>,
}

pub async fn new_ptr(config: &FlameClusterContext) -> Result<StoragePtr, FlameError> {
    let event_manager: EventManagerPtr = if config.cluster.storage == "none" {
        Arc::new(MemoryEventManager::new())
    } else {
        let events_path = derive_events_path(&config.cluster.storage);
        Arc::new(FsEventManager::new(&events_path)?)
    };

    let engine = engine::connect(&config.cluster.storage).await?;
    let workspaces = engine
        .list_workspaces()
        .await?
        .into_iter()
        .map(|workspace| (workspace.name.clone(), workspace))
        .collect();
    Ok(Arc::new(Storage {
        context: config.clone(),
        engine,
        sessions: stdng::new_ptr(HashMap::new()),
        executors: stdng::new_ptr(HashMap::new()),
        nodes: stdng::new_ptr(HashMap::new()),
        applications: stdng::new_ptr(HashMap::new()),
        workspaces: stdng::new_ptr(workspaces),
        event_manager,
        max_sessions: config.cluster.limits.max_sessions,
    }))
}

fn derive_events_path(storage_url: &str) -> String {
    events_path(storage_url, std::env::var_os("FLAME_TEST_DIR").as_deref())
}

fn events_path(storage_url: &str, test_dir: Option<&std::ffi::OsStr>) -> String {
    if let Some(test_dir) = test_dir {
        use std::hash::{Hash, Hasher};
        // Concurrent test stores must not read or append each other's event logs.
        let mut key = std::collections::hash_map::DefaultHasher::new();
        storage_url.hash(&mut key);
        return std::path::Path::new(test_dir)
            .join("events")
            .join(format!("{:016x}", key.finish()))
            .to_string_lossy()
            .to_string();
    }

    "events".to_string()
}

impl Storage {
    pub async fn create_workspace(&self, name: String) -> Result<Workspace, FlameError> {
        let workspace = self.engine.create_workspace(name).await?;
        lock_ptr!(self.workspaces)?.insert(workspace.name.clone(), workspace.clone());
        Ok(workspace)
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        let mut workspaces: Vec<_> = lock_ptr!(self.workspaces)?.values().cloned().collect();
        workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(workspaces)
    }

    pub fn workspace_exists(&self, workspace: &str) -> Result<bool, FlameError> {
        Ok(lock_ptr!(self.workspaces)?.contains_key(workspace))
    }

    pub fn session_retry_limits(&self) -> u32 {
        self.context.cluster.recovery.session.retry_limits
    }

    pub fn snapshot(&self) -> Result<StorageSnapshot, FlameError> {
        let applications = lock_ptr!(self.applications)?
            .values()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        let sessions = lock_ptr!(self.sessions)?
            .values()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        let executors = lock_ptr!(self.executors)?
            .values()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        let nodes = lock_ptr!(self.nodes)?
            .values()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        Ok(StorageSnapshot {
            applications,
            sessions,
            executors,
            nodes,
        })
    }

    pub async fn load_data(&self) -> Result<(), FlameError> {
        let ssn_list = self.engine.find_sessions().await?;
        for ssn in ssn_list {
            let task_list = self.engine.find_tasks(&ssn.gid()).await?;
            let mut ssn = ssn.clone();
            for task in task_list {
                let task = match task.state {
                    TaskState::Running => {
                        self.engine
                            .retry_task(&task.session(), &task.name.to_string())
                            .await?
                    }
                    _ => task,
                };

                ssn.update_task(&task)?;
            }

            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(
                (ssn.workspace.clone(), ssn.name.clone()),
                SessionPtr::new(ssn.into()),
            );
        }

        let app_list = self.engine.find_applications(None).await?;
        for app in app_list {
            let mut app_map = lock_ptr!(self.applications)?;
            app_map.insert(
                (app.workspace.clone(), app.name.clone()),
                ApplicationPtr::new(app.into()),
            );
        }

        let node_list = self.engine.find_nodes().await?;
        for node in node_list {
            let mut node_map = lock_ptr!(self.nodes)?;
            node_map.insert(node.name.clone(), stdng::new_ptr(node));
        }

        let executor_list = self.engine.find_executors(None).await?;
        for executor in executor_list {
            // Reset executors stuck in Binding state back to Idle.
            // Binding is a transitional state during the binding handshake.
            // If session manager restarted mid-binding, the binding was never completed,
            // so the executor should return to Idle for re-scheduling.
            let executor = if executor.state == ExecutorState::Binding {
                tracing::warn!(
                    "Executor <{}> was in Binding state during recovery, resetting to Idle",
                    executor.id
                );
                let mut recovered = executor.clone();
                recovered.set_state(ExecutorState::Idle);
                recovered.session = None;
                recovered.task = None;
                self.engine.update_executor(&recovered).await?;
                recovered
            } else {
                executor
            };

            let mut exe_map = lock_ptr!(self.executors)?;
            exe_map.insert(executor.name.clone(), ExecutorPtr::new(executor.into()));
        }

        Ok(())
    }

    fn evict_sessions(&self) -> Result<(), FlameError> {
        let Some(max) = self.max_sessions else {
            return Ok(());
        };

        let mut ssn_map = lock_ptr!(self.sessions)?;

        // Loop until we're within the limit
        while ssn_map.len() >= max {
            // Collect all closed sessions with their completion times
            let mut closed_sessions: Vec<((String, String), chrono::DateTime<Utc>)> = ssn_map
                .iter()
                .filter_map(|(id, ssn_ptr)| {
                    let ssn = lock_ptr!(ssn_ptr).ok()?;
                    if ssn.status.state == SessionState::Closed {
                        // Use completion_time if available, otherwise use creation_time
                        let time = ssn.completion_time.unwrap_or(ssn.creation_time);
                        Some((id.clone(), time))
                    } else {
                        None
                    }
                })
                .collect();

            if closed_sessions.is_empty() {
                // No closed sessions to evict, cannot reduce further
                tracing::warn!(
                    "Session limit ({}) reached but no closed sessions to evict. Current: {}",
                    max,
                    ssn_map.len()
                );
                break;
            }

            // Sort by completion time (oldest first)
            closed_sessions.sort_by_key(|a| a.1);

            // Evict the oldest closed session
            if let Some((session_key, _)) = closed_sessions.first() {
                ssn_map.remove(session_key);
                tracing::debug!(
                    "Evicted closed session <{:?}> from cache (limit: {}, current: {})",
                    session_key,
                    max,
                    ssn_map.len()
                );
            }
        }

        Ok(())
    }

    pub async fn register_node(&self, node: &Node) -> Result<(), FlameError> {
        trace_fn!("Storage::register_node");

        let existing = {
            let node_map = lock_ptr!(self.nodes)?;
            node_map.get(&node.name).cloned()
        };
        let mut node = node.clone();
        if let Some(existing) = existing {
            node.id = lock_ptr!(existing)?.id.clone();
            node = self.engine.update_node(&node).await?;
        } else {
            node.id = crate::apis::new_metadata_id();
            node = self.engine.create_node(&node).await?;
        }

        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.insert(node.name.clone(), stdng::new_ptr(node));
        Ok(())
    }

    /// Gets a node by name. Returns None if the node doesn't exist.
    pub fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        let node_map = lock_ptr!(self.nodes)?;
        match node_map.get(name) {
            Some(node_ptr) => {
                let node = lock_ptr!(node_ptr)?;
                Ok(Some(node.clone()))
            }
            None => Ok(None),
        }
    }

    /// Gets a node pointer by name. Returns error if the node doesn't exist.
    pub fn get_node_ptr(&self, name: &str) -> Result<NodePtr, FlameError> {
        let node_map = lock_ptr!(self.nodes)?;
        node_map
            .get(name)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("node <{}> not found", name)))
    }

    /// Updates the state of a node in storage first, then in memory.
    pub async fn update_node_state(
        &self,
        name: &str,
        state: crate::apis::NodeState,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_node_state");

        // Get node clone with new state for persistence
        let node_clone = {
            let node_map = lock_ptr!(self.nodes)?;
            if let Some(node_ptr) = node_map.get(name) {
                let node = lock_ptr!(node_ptr)?;
                let mut updated = node.clone();
                updated.state = state;
                Some(updated)
            } else {
                None
            }
        };

        // Persist to storage first
        if let Some(node) = node_clone {
            self.engine.update_node(&node).await?;

            // Only update in-memory state after successful persistence
            let node_map = lock_ptr!(self.nodes)?;
            if let Some(node_ptr) = node_map.get(name) {
                let mut node = lock_ptr!(node_ptr)?;
                node.state = state;
            }
            tracing::info!("Updated node {} state to {:?}", name, state);
        }
        Ok(())
    }

    /// Lists all registered nodes.
    pub fn list_nodes(&self) -> Result<Vec<Node>, FlameError> {
        let mut node_list = vec![];
        let node_map = lock_ptr!(self.nodes)?;

        for node in node_map.deref().values() {
            let node = lock_ptr!(node)?;
            node_list.push(node.clone());
        }

        Ok(node_list)
    }

    /// # Deprecated
    /// Use `WatchNode` streaming RPC instead for better efficiency.
    #[deprecated(since = "0.6.0", note = "Use WatchNode streaming RPC instead")]
    pub async fn sync_node(
        &self,
        node: &Node,
        _: &Vec<Executor>,
    ) -> Result<Vec<Executor>, FlameError> {
        let existing = {
            let node_map = lock_ptr!(self.nodes)?;
            node_map.get(&node.name).cloned()
        };
        let mut node = node.clone();
        if let Some(existing) = existing {
            node.id = lock_ptr!(existing)?.id.clone();
            node = self.engine.update_node(&node).await?;
        } else {
            node.id = crate::apis::new_metadata_id();
            node = self.engine.create_node(&node).await?;
        }

        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.insert(node.name.clone(), stdng::new_ptr(node.clone()));

        let mut res = vec![];

        let exe_map = lock_ptr!(self.executors)?;
        let execs = exe_map.values();
        for exec in execs {
            let exec = lock_ptr!(exec)?;
            if exec.node == node.name {
                res.push(exec.clone());
            }
        }

        tracing::debug!("There are {} executors in node {}", res.len(), node.name);

        Ok(res)
    }

    pub async fn release_node(&self, node_name: &str) -> Result<(), FlameError> {
        self.engine.delete_node(node_name).await?;

        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.remove(node_name);
        Ok(())
    }

    /// Deletes multiple executors and retries their running tasks.
    /// Returns the names of executors that were successfully deleted.
    pub async fn delete_executors(
        &self,
        executors: &[Executor],
    ) -> Result<Vec<String>, FlameError> {
        trace_fn!("Storage::delete_executors");

        let mut deleted_executor_names = Vec::new();

        for executor in executors {
            // If executor has a running task, retry it
            if let (Some(task), Some(gid)) = (&executor.task, executor.session()) {
                let session = &gid.session;
                match self.engine.retry_task(&gid, task).await {
                    Ok(task) => {
                        // Update the in-memory session with the retried task
                        if let Ok(ssn_ptr) = self.get_session_ptr(&executor.workspace, session) {
                            if let Ok(mut ssn) = lock_ptr!(ssn_ptr) {
                                let _ = ssn.update_task(&task);
                            }
                        }
                        tracing::info!(
                            "Retried task {} for session {} due to executor {} cleanup",
                            task.name,
                            session,
                            executor.id
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to retry task {} for session {}: {}",
                            task,
                            session,
                            e
                        );
                    }
                }
            }

            // Delete the executor
            if let Err(e) = self
                .delete_executor(&executor.workspace, &executor.name)
                .await
            {
                tracing::warn!("Failed to delete executor {}: {}", executor.id, e);
            } else {
                deleted_executor_names.push(executor.name.clone());
            }
        }

        tracing::info!("Deleted {} executors", deleted_executor_names.len());

        Ok(deleted_executor_names)
    }

    pub async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        trace_fn!("Storage::create_session");
        let ssn = self.engine.create_session(attr).await?;

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(
                (ssn.workspace.clone(), ssn.name.clone()),
                SessionPtr::new(ssn.clone().into()),
            );
        }

        self.evict_sessions()?;

        Ok(ssn)
    }

    pub async fn close_session(&self, workspace: &str, name: &str) -> Result<Session, FlameError> {
        trace_fn!("Storage::close_session");

        let ssn_ptr = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map
                .get(&(workspace.to_string(), name.to_string()))
                .cloned()
                .ok_or(FlameError::NotFound(format!(
                    "session <{workspace}/{name}>"
                )))?
        };

        {
            let ssn = lock_ptr!(ssn_ptr)?;
            if ssn
                .tasks_index
                .get(&TaskState::Running)
                .is_some_and(|tasks| !tasks.is_empty())
            {
                return Err(FlameError::Storage(
                    "Cannot close session with running tasks".to_string(),
                ));
            }
        }

        let persisted_ssn = match self
            .engine
            .close_session(&SessionGID::new(workspace, name))
            .await
        {
            Ok(ssn) => Some(ssn),
            Err(FlameError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };

        let completion_time = persisted_ssn
            .as_ref()
            .and_then(|ssn| ssn.completion_time)
            .unwrap_or_else(Utc::now);

        let result_ssn = {
            let mut ssn = lock_ptr!(ssn_ptr)?;
            ssn.status.state = persisted_ssn
                .as_ref()
                .map(|ssn| ssn.status.state)
                .unwrap_or(SessionState::Closed);
            ssn.completion_time = Some(completion_time);
            ssn.version = persisted_ssn
                .as_ref()
                .map(|ssn| ssn.version)
                .unwrap_or(ssn.version + 1);

            let pending_tasks = ssn
                .tasks_index
                .get(&TaskState::Pending)
                .map(|tasks| tasks.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();

            for task_ptr in pending_tasks {
                let task = {
                    let task = lock_ptr!(task_ptr)?;
                    Task {
                        state: TaskState::Cancelled,
                        version: task.version + 1,
                        completion_time: Some(completion_time),
                        ..task.clone()
                    }
                };
                ssn.update_task(&task)?;
            }

            ssn.clone()
        };

        self.evict_sessions()?;

        Ok(result_ssn)
    }

    pub fn get_session(&self, workspace: &str, name: &str) -> Result<Session, FlameError> {
        let ssn_ptr = self.get_session_ptr(workspace, name)?;
        let ssn = lock_ptr!(ssn_ptr)?;
        let mut ssn = ssn.clone();
        ssn.events = self
            .event_manager
            .find_events(EventOwner::session(ssn.workspace.clone(), ssn.name.clone()))?;
        Ok(ssn)
    }

    pub fn get_session_ptr(&self, workspace: &str, name: &str) -> Result<SessionPtr, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;

        ssn_map
            .get(&(workspace.to_string(), name.to_string()))
            .ok_or(FlameError::NotFound(format!("session {workspace}/{name}")))
            .cloned()
    }

    pub async fn open_session(
        &self,
        workspace: &str,
        name: &str,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        trace_fn!("Storage::open_session");

        // Check if session already exists in cache - if so, return it directly
        // to preserve in-memory task state
        {
            let ssn_map = lock_ptr!(self.sessions)?;
            if let Some(ssn_ptr) = ssn_map.get(&(workspace.to_string(), name.to_string())) {
                let ssn = lock_ptr!(ssn_ptr)?;
                // Verify the session is still open before returning cached version
                if ssn.status.state == SessionState::Open {
                    // If spec provided, validate it matches the existing session
                    if let Some(ref attr) = spec {
                        ssn.validate_spec(attr)?;
                    }
                    tracing::debug!(
                        "Session <{}> already exists in cache with {} tasks, returning cached version",
                        name,
                        ssn.tasks.len()
                    );
                    return Ok(ssn.clone());
                }
            }
        }

        // Session not in cache or not open, delegate to engine for atomic get-or-create operation
        let ssn = self
            .engine
            .open_session(&SessionGID::new(workspace, name), spec)
            .await?;

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(
                (ssn.workspace.clone(), ssn.name.clone()),
                SessionPtr::new(ssn.clone().into()),
            );
        }

        self.evict_sessions()?;

        Ok(ssn)
    }

    pub fn get_task_ptr(
        &self,
        workspace: &str,
        session: &str,
        task: &str,
    ) -> Result<TaskPtr, FlameError> {
        let ssn_ptr = self.get_session_ptr(workspace, session)?;
        let ssn = lock_ptr!(ssn_ptr)?;
        let task_number = task
            .parse::<TaskName>()
            .ok()
            .filter(|number| *number != 0 && number.to_string() == task)
            .ok_or_else(|| FlameError::NotFound(format!("task {workspace}/{session}/{task}")))?;
        ssn.tasks
            .get(&task_number)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("task {workspace}/{session}/{task}")))
    }

    pub async fn delete_session(&self, workspace: &str, name: &str) -> Result<Session, FlameError> {
        let cached = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map
                .get(&(workspace.to_string(), name.to_string()))
                .cloned()
        };

        let ssn = match cached {
            Some(ssn_ptr) => {
                let ssn = lock_ptr!(ssn_ptr)?.clone();
                if let Err(error) = self
                    .engine
                    .delete_session(&SessionGID::new(workspace, name))
                    .await
                {
                    if !matches!(error, FlameError::NotFound(_)) {
                        return Err(error);
                    }
                }
                ssn
            }
            None => {
                self.engine
                    .delete_session(&SessionGID::new(workspace, name))
                    .await?
            }
        };

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.remove(&(workspace.to_string(), name.to_string()));
        }

        self.event_manager
            .remove_events(&SessionGID::new(workspace, name))?;

        Ok(ssn)
    }

    pub fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<Session>, FlameError> {
        if filter.limit == Some(0) {
            return Ok(Vec::new());
        }
        let mut ssn_list = vec![];
        let ssn_map = lock_ptr!(self.sessions)?;

        for ((workspace, _), ssn) in ssn_map.iter() {
            if workspace != &filter.workspace {
                continue;
            }
            let ssn = lock_ptr!(ssn)?;
            let mut ssn = ssn.clone();
            ssn.events = self
                .event_manager
                .find_events(EventOwner::session(ssn.workspace.clone(), ssn.name.clone()))?;
            let matches = {
                ssn.workspace == filter.workspace
                    && filter
                        .application
                        .as_ref()
                        .is_none_or(|application| ssn.application == *application)
                    && filter.state.is_none_or(|state| ssn.status.state == state)
                    && filter
                        .names
                        .as_ref()
                        .is_none_or(|names| names.contains(&ssn.name))
            };
            if !matches {
                continue;
            }
            if let Some(predicate) = filter.predicate {
                if !predicate.matches(ssn.retry_count, self.session_retry_limits()) {
                    continue;
                }
            }
            ssn_list.push(ssn);
            if filter.limit.is_some_and(|limit| ssn_list.len() >= limit) {
                break;
            }
        }

        Ok(ssn_list)
    }

    /// Lists executors with optional filtering.
    ///
    /// # Arguments
    /// * `filter` - Filter criteria (state, node, and/or executor names).
    ///   - `None` filter means return all executors
    ///   - For each field in filter:
    ///     - `None` = ignore this field (match all)
    ///     - `Some(value)` = match exactly (empty vec/string matches nothing)
    ///   - All specified filters use AND logic.
    pub fn list_executors(
        &self,
        filter: Option<&ExecutorFilter>,
    ) -> Result<Vec<Executor>, FlameError> {
        let exe_map = lock_ptr!(self.executors)?;

        // None filter means return all
        let Some(filter) = filter else {
            return Ok(exe_map
                .values()
                .filter_map(|exe_ptr| exe_ptr.lock().ok().map(|e| e.clone()))
                .collect());
        };

        let exe_list: Vec<Executor> = exe_map
            .values()
            .filter_map(|exe_ptr| {
                let exe = exe_ptr.lock().ok()?;

                // Filter by state if specified
                if let Some(state) = filter.state {
                    if exe.state != state {
                        return None;
                    }
                }

                // Filter by node if specified
                // Some("") matches nothing, Some("x") matches node "x", None matches all
                if let Some(ref node_name) = filter.node {
                    if &exe.node != node_name {
                        return None;
                    }
                }

                // Filter by names if specified
                // Some([]) matches nothing, Some([a,b]) matches a or b, None matches all
                if let Some(ref names) = filter.names {
                    if !names.contains(&exe.name) {
                        return None;
                    }
                }

                Some(exe.clone())
            })
            .collect();

        Ok(exe_list)
    }

    pub async fn create_task(
        &self,
        workspace: &str,
        session: &str,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        trace_fn!("Storage::create_task");
        let task = self
            .engine
            .create_task(&SessionGID::new(workspace, session), task_input, options)
            .await?;

        let ssn = self.get_session_ptr(workspace, session)?;
        let mut ssn = lock_ptr!(ssn)?;
        ssn.update_task(&task)?;

        self.event_manager.record_event(
            EventOwner::from(&task),
            Event {
                code: task.state.into(),
                message: Some(format!("Task was created with state <{:?}>", task.state)),
                creation_time: Utc::now(),
            },
        )?;

        Ok(task)
    }

    pub fn get_task(&self, workspace: &str, session: &str, task: &str) -> Result<Task, FlameError> {
        let mut task = self.get_task_metadata(workspace, session, task)?;
        let events = self.event_manager.find_events(EventOwner::from(&task))?;
        task.events = events;

        Ok(task)
    }

    /// Clone the current task state without loading historical events from disk.
    /// Nonterminal watch updates need current state without serializing
    /// unrelated sessions behind event storage I/O.
    pub fn get_task_metadata(
        &self,
        workspace: &str,
        session: &str,
        task: &str,
    ) -> Result<Task, FlameError> {
        let task_ptr = self.get_task_ptr(workspace, session, task)?;
        let mut task = lock_ptr!(task_ptr)?.clone();
        task.events.clear();
        Ok(task)
    }

    pub fn list_tasks(&self, workspace: &str, session: &str) -> Result<Vec<Task>, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;
        let ssn = ssn_map
            .get(&(workspace.to_string(), session.to_string()))
            .ok_or_else(|| FlameError::NotFound(format!("session {workspace}/{session}")))?;

        let ssn = lock_ptr!(ssn)?;
        let task_list = ssn
            .tasks
            .values()
            .map(|task_ptr| {
                let task = lock_ptr!(task_ptr)?;
                Ok(task.clone())
            })
            .collect::<Result<Vec<Task>, FlameError>>()?;

        Ok(task_list)
    }

    pub async fn get_application(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Application, FlameError> {
        self.engine.get_application(workspace, name).await
    }

    pub fn get_application_ptr(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<ApplicationPtr, FlameError> {
        lock_ptr!(self.applications)?
            .get(&(workspace.to_string(), name.to_string()))
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("application {workspace}/{name}")))
    }

    pub async fn register_application(
        &self,
        workspace: String,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        let app = self
            .engine
            .register_application(workspace, name, attr)
            .await?;

        let mut app_map = lock_ptr!(self.applications)?;
        // just lock the sessions to avoid cache mismatch.
        let _unused = lock_ptr!(self.sessions)?;

        app_map.insert(
            (app.workspace.clone(), app.name.clone()),
            stdng::new_ptr(app.clone()),
        );

        Ok(app)
    }

    pub async fn update_application_state(
        &self,
        workspace: &str,
        name: &str,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let app = self
            .engine
            .update_application_state(workspace, name, state)
            .await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.insert(
            (workspace.to_string(), name.to_string()),
            stdng::new_ptr(app.clone()),
        );
        Ok(app)
    }

    pub async fn delete_application(&self, workspace: &str, name: &str) -> Result<(), FlameError> {
        self.engine.delete_application(workspace, name).await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.remove(&(workspace.to_string(), name.to_string()));

        Ok(())
    }

    pub async fn update_application(
        &self,
        workspace: &str,
        name: &str,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let app = self
            .engine
            .update_application(workspace, name, attr)
            .await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.insert(
            (workspace.to_string(), name.to_string()),
            stdng::new_ptr(app.clone()),
        );

        Ok(())
    }

    pub async fn list_applications(
        &self,
        filter: &ApplicationFilter,
    ) -> Result<Vec<Application>, FlameError> {
        self.engine.find_applications(Some(filter)).await
    }

    pub async fn session_application(
        &self,
        workspace: &str,
        session: &str,
    ) -> Result<String, FlameError> {
        let cached = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map
                .get(&(workspace.to_string(), session.to_string()))
                .cloned()
        };
        if let Some(ssn) = cached {
            return Ok(lock_ptr!(ssn)?.application.clone());
        }

        Ok(self
            .engine
            .get_session(&SessionGID::new(workspace, session))
            .await?
            .application)
    }

    pub fn count_session(&self, filter: &SessionFilter) -> Result<usize, FlameError> {
        Ok(self.list_sessions(filter)?.len())
    }

    pub async fn update_task_state(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_state");
        let current = lock_ptr!(task)?.clone();
        {
            let owner = lock_ptr!(ssn)?;
            if owner.workspace != current.workspace || owner.name != current.session {
                return Err(FlameError::InvalidConfig(format!(
                    "task {}/{}/{} does not belong to session {}/{}",
                    current.workspace, current.session, current.name, owner.workspace, owner.name
                )));
            }
        }

        let updated_task = match self
            .engine
            .update_task_state(
                &current.session(),
                &current.name.to_string(),
                task_state,
                message,
            )
            .await
        {
            Ok(task) => task,
            Err(FlameError::NotFound(_)) => {
                // Clone first without mutating the Arc in self.tasks.
                // Mutating in-place would advance the version on the shared Arc,
                // causing update_task() to see equal versions and skip tasks_index update.
                let current = {
                    let task_guard = lock_ptr!(task)?;
                    task_guard.clone()
                };
                crate::apis::Task {
                    state: task_state,
                    version: current.version + 1,
                    ..current
                }
            }
            Err(e) => return Err(e),
        };

        let mut ssn_ptr = lock_ptr!(ssn)?;
        ssn_ptr.update_task(&updated_task)?;

        self.event_manager.record_event(
            EventOwner::from(&updated_task),
            Event {
                code: task_state.into(),
                message: Some(format!("Task state was updated to <{:?}>", task_state)),
                creation_time: Utc::now(),
            },
        )?;

        Ok(())
    }

    pub async fn update_task_result(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_result");
        let current = lock_ptr!(task)?.clone();
        {
            let owner = lock_ptr!(ssn)?;
            if owner.workspace != current.workspace || owner.name != current.session {
                return Err(FlameError::InvalidConfig(format!(
                    "task {}/{}/{} does not belong to session {}/{}",
                    current.workspace, current.session, current.name, owner.workspace, owner.name
                )));
            }
        }

        let task_state = task_result.state;
        let task_message = task_result.message.clone();
        let task_output = task_result.output.clone();

        let updated_task = match self
            .engine
            .update_task_result(&current.session(), &current.name.to_string(), task_result)
            .await
        {
            Ok(task) => task,
            Err(FlameError::NotFound(_)) => {
                // Clone first without mutating the Arc in self.tasks.
                // Mutating in-place would advance the version on the shared Arc,
                // causing update_task() to see equal versions and skip tasks_index update.
                let current = {
                    let task_guard = lock_ptr!(task)?;
                    task_guard.clone()
                };
                crate::apis::Task {
                    state: task_state,
                    version: current.version + 1,
                    completion_time: Some(Utc::now()),
                    output: task_output,
                    ..current
                }
            }
            Err(e) => return Err(e),
        };

        let mut ssn_ptr = lock_ptr!(ssn)?;
        ssn_ptr.update_task(&updated_task)?;

        let event_message = match task_state {
            TaskState::Failed => {
                task_message.unwrap_or_else(|| format!("Task failed with state <{:?}>", task_state))
            }
            _ => format!("Task was completed with state <{:?}>", task_state),
        };

        self.event_manager.record_event(
            EventOwner::from(&updated_task),
            Event {
                code: updated_task.state.into(),
                message: Some(event_message),
                creation_time: Utc::now(),
            },
        )?;

        Ok(())
    }

    pub async fn create_executor(
        &self,
        node_name: String,
        workspace: &str,
        session: &str,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Storage::create_executor");
        let ssn = self.get_session_ptr(workspace, session)?;

        let (application, resreq) = {
            let ssn = lock_ptr!(ssn)?;
            let resreq = ssn.resreq.clone().ok_or_else(|| {
                FlameError::InvalidState(format!(
                    "session <{}> has no resreq; resolve_session_resreq must populate it",
                    session
                ))
            })?;
            (ssn.application.clone(), resreq)
        };
        let shim = {
            let applications = lock_ptr!(self.applications)?;
            match applications.get(&(workspace.to_string(), application.clone())) {
                Some(application) => lock_ptr!(application)?.shim,
                None => {
                    tracing::warn!(
                        "Application <{}> is missing while creating an executor; using the default shim",
                        application
                    );
                    Shim::default()
                }
            }
        };

        let now = Utc::now();
        let name = Uuid::new_v4().to_string();
        let e = Executor {
            id: Uuid::new_v4().to_string(),
            name: name.clone(),
            node: node_name.clone(),
            resreq,
            shim,
            application,
            workspace: workspace.to_string(),
            task: None,
            session: None,
            attributes: Default::default(),
            creation_time: now,
            latest_updated_timestamp: now,
            state: ExecutorState::Void,
        };

        self.engine.create_executor(&e).await?;

        let mut exe_map = lock_ptr!(self.executors)?;
        let exe = ExecutorPtr::new(e.clone().into());
        exe_map.insert(e.name.clone(), exe.clone());

        Ok(e.clone())
    }

    pub fn get_executor_ptr(&self, name: &str) -> Result<ExecutorPtr, FlameError> {
        let exe_map = lock_ptr!(self.executors)?;
        let exe = exe_map
            .get(name)
            .ok_or(FlameError::NotFound(name.to_string()))?;

        Ok(exe.clone())
    }

    pub async fn update_executor(&self, executor: &Executor) -> Result<(), FlameError> {
        trace_fn!("Storage::update_executor");
        self.engine.update_executor(executor).await?;

        let exe_map = lock_ptr!(self.executors)?;
        if let Some(exe_ptr) = exe_map.get(&executor.name) {
            let mut exe = lock_ptr!(exe_ptr)?;
            exe.state = executor.state;
            exe.task = executor.task.clone();
            exe.session = executor.session.clone();
            exe.latest_updated_timestamp = executor.latest_updated_timestamp;
        }

        Ok(())
    }

    pub async fn delete_executor(&self, workspace: &str, name: &str) -> Result<(), FlameError> {
        trace_fn!("Storage::delete_executor");
        self.engine
            .delete_executor(&ExecutorGID::new(workspace, name))
            .await?;

        let mut exe_map = lock_ptr!(self.executors)?;
        exe_map.remove(name);

        Ok(())
    }

    pub async fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        trace_fn!("Storage::record_event");
        self.event_manager.record_event(owner, event)
    }
}

#[cfg(test)]
mod node_tests;

#[cfg(test)]
mod node_executor_tests;

#[cfg(test)]
mod application_tests;

#[cfg(test)]
mod executor_tests;

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod task_tests;

#[cfg(test)]
mod session_limit_tests;

#[cfg(test)]
mod load_data_tests;

#[cfg(test)]
mod derive_events_path_tests;

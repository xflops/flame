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

use stdng::{ensure, lock_ptr, trace_fn, MutexPtr};

use crate::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationPtr, ApplicationState, Event,
    EventOwner, ExecutorID, ExecutorState, Node, NodePtr, Session, SessionAttributes, SessionID,
    SessionPtr, SessionState, Shim, Task, TaskGID, TaskID, TaskInput, TaskOptions, TaskPtr,
    TaskResult, TaskState,
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
    sessions: MutexPtr<HashMap<SessionID, SessionPtr>>,
    executors: MutexPtr<HashMap<ExecutorID, ExecutorPtr>>,
    nodes: MutexPtr<HashMap<String, NodePtr>>,
    applications: MutexPtr<HashMap<String, ApplicationPtr>>,
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

    Ok(Arc::new(Storage {
        context: config.clone(),
        engine: engine::connect(&config.cluster.storage).await?,
        sessions: stdng::new_ptr(HashMap::new()),
        executors: stdng::new_ptr(HashMap::new()),
        nodes: stdng::new_ptr(HashMap::new()),
        applications: stdng::new_ptr(HashMap::new()),
        event_manager,
        max_sessions: config.cluster.limits.max_sessions,
    }))
}

fn derive_events_path(_storage_url: &str) -> String {
    if let Ok(test_dir) = std::env::var("FLAME_TEST_DIR") {
        return std::path::Path::new(&test_dir)
            .join("events")
            .to_string_lossy()
            .to_string();
    }

    "events".to_string()
}

impl Storage {
    pub fn session_retry_limits(&self) -> u32 {
        self.context.cluster.recovery.session.retry_limits
    }

    pub fn snapshot(&self) -> Result<StorageSnapshot, FlameError> {
        let applications = lock_ptr!(self.applications)?
            .values()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        // A suspended session holds no executor and has no demand.
        let mut sessions = Vec::new();
        for ptr in lock_ptr!(self.sessions)?.values() {
            let ssn = lock_ptr!(ptr)?;
            if !ssn.is_suspended() {
                sessions.push(ssn.clone());
            }
        }
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
            if ssn.status.state == SessionState::Closed {
                event_result(
                    self.event_manager.remove_events(ssn.id.clone()),
                    "remove",
                    &ssn.id,
                );
            } else {
                event_result(
                    self.event_manager.setup_events(ssn.id.clone()),
                    "create",
                    &ssn.id,
                );
            }
            let task_list = self.engine.find_tasks(ssn.id.clone()).await?;
            let mut ssn = ssn.clone();
            for task in task_list {
                let task = match task.state {
                    TaskState::Running if ssn.status.state == SessionState::Open => {
                        self.engine.retry_task(task.gid()).await?
                    }
                    _ => task,
                };

                ssn.update_task(&task)?;
            }

            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.into()));
        }

        let app_list = self.engine.find_applications(None).await?;
        for app in app_list {
            let mut app_map = lock_ptr!(self.applications)?;
            app_map.insert(app.name.clone(), ApplicationPtr::new(app.into()));
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
                recovered.ssn_id = None;
                self.engine.update_executor(&recovered).await?;
                recovered
            } else {
                executor
            };

            let mut exe_map = lock_ptr!(self.executors)?;
            exe_map.insert(executor.id.clone(), ExecutorPtr::new(executor.into()));
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
            let mut closed_sessions: Vec<(SessionID, chrono::DateTime<Utc>)> = ssn_map
                .iter()
                .filter_map(|(id, ssn_ptr)| {
                    let ssn = lock_ptr!(ssn_ptr).ok()?;
                    if ssn.is_closed() {
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
            if let Some((ssn_id, _)) = closed_sessions.first() {
                ssn_map.remove(ssn_id);
                tracing::debug!(
                    "Evicted closed session <{}> from cache (limit: {}, current: {})",
                    ssn_id,
                    max,
                    ssn_map.len()
                );
            }
        }

        Ok(())
    }

    pub async fn register_node(&self, node: &Node) -> Result<(), FlameError> {
        trace_fn!("Storage::register_node");

        let exists = {
            let node_map = lock_ptr!(self.nodes)?;
            node_map.contains_key(&node.name)
        };

        if exists {
            self.engine.update_node(node).await?;
        } else {
            self.engine.create_node(node).await?;
        }

        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.insert(node.name.clone(), stdng::new_ptr(node.clone()));
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
        let exists = {
            let node_map = lock_ptr!(self.nodes)?;
            node_map.contains_key(&node.name)
        };

        if exists {
            self.engine.update_node(node).await?;
        } else {
            self.engine.create_node(node).await?;
        }

        let mut node_map = lock_ptr!(self.nodes)?;
        node_map.insert(node.name.clone(), stdng::new_ptr(node.clone()));

        let mut res = vec![];

        let exe_map = lock_ptr!(self.executors)?;
        let execs = exe_map.values();
        for exec in execs {
            let exec = lock_ptr!(exec)?;
            if exec.node == node.name {
                res.push(Executor {
                    id: exec.id.clone(),
                    node: exec.node.clone(),
                    resreq: exec.resreq.clone(),
                    shim: exec.shim,
                    application: exec.application.clone(),
                    task_id: exec.task_id,
                    ssn_id: exec.ssn_id.clone(),
                    signal: exec.signal,
                    attributes: exec.attributes.clone(),
                    creation_time: exec.creation_time,
                    latest_updated_timestamp: exec.latest_updated_timestamp,
                    state: exec.state,
                });
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
    /// Returns the list of executor IDs that were successfully deleted.
    pub async fn delete_executors(
        &self,
        executors: &[Executor],
    ) -> Result<Vec<ExecutorID>, FlameError> {
        trace_fn!("Storage::delete_executors");

        let mut deleted_executor_ids = Vec::new();

        for executor in executors {
            // If executor has a running task, retry it
            if let (Some(task_id), Some(ref ssn_id)) = (executor.task_id, &executor.ssn_id) {
                let gid = TaskGID {
                    ssn_id: ssn_id.clone(),
                    task_id,
                };
                match self.engine.retry_task(gid.clone()).await {
                    Ok(task) => {
                        // Update the in-memory session with the retried task
                        if let Ok(ssn_ptr) = self.get_session_ptr(ssn_id.clone()) {
                            if let Ok(mut ssn) = lock_ptr!(ssn_ptr) {
                                let _ = ssn.update_task(&task);
                            }
                        }
                        tracing::info!(
                            "Retried task {} for session {} due to executor {} cleanup",
                            task_id,
                            ssn_id,
                            executor.id
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to retry task {} for session {}: {}",
                            task_id,
                            ssn_id,
                            e
                        );
                    }
                }
            }

            // Delete the executor
            if let Err(e) = self.delete_executor(executor.id.clone()).await {
                tracing::warn!("Failed to delete executor {}: {}", executor.id, e);
            } else {
                deleted_executor_ids.push(executor.id.clone());
            }
        }

        tracing::info!("Deleted {} executors", deleted_executor_ids.len());

        Ok(deleted_executor_ids)
    }

    pub async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        trace_fn!("Storage::create_session");
        let ssn = self.engine.create_session(attr).await?;
        event_result(
            self.event_manager.setup_events(ssn.id.clone()),
            "create",
            &ssn.id,
        );

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.clone().into()));
        }

        self.evict_sessions()?;

        Ok(ssn)
    }

    /// Close a session in any non-closed state.
    pub async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        trace_fn!("Storage::close_session");

        let ssn_ptr = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map
                .get(&id)
                .cloned()
                .ok_or(FlameError::NotFound(format!("session <{}>", id)))?
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

        let persisted_ssn = match self.engine.close_session(id.clone()).await {
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

            // Publish Closed before cleanup so a cleanup failure cannot allow
            // later event writers to recreate the session's files.
            ssn.events.clear();
            event_result(self.event_manager.remove_events(id.clone()), "remove", &id);
            ssn.clone()
        };

        self.evict_sessions()?;

        Ok(result_ssn)
    }

    /// Sets the state of a session that is not `Closed` (`InvalidState` otherwise). It does
    /// not check which transition is legal; the caller decides that from the state it read.
    ///
    /// Running tasks of a `Suspending` session keep running and finish normally; only new
    /// launches stop, because `wait_for_task` hands out tasks to open sessions only.
    /// Pending tasks wait for the resume.
    pub async fn update_session_state(
        &self,
        id: SessionID,
        state: SessionState,
    ) -> Result<Session, FlameError> {
        trace_fn!("Storage::update_session_state");

        let ssn_ptr = self.get_session_ptr(id.clone())?;

        // Admission and the cached flip happen in one lock section.
        let previous = {
            let mut ssn = lock_ptr!(ssn_ptr)?;
            ensure!(
                !ssn.is_closed(),
                FlameError::InvalidState(format!("Session <{id}> is closed"))
            )?;
            std::mem::replace(&mut ssn.status.state, state)
        };

        match self.engine.update_session_state(id, state).await {
            Ok(_) | Err(FlameError::NotFound(_)) => {}
            Err(e) => {
                let mut ssn = lock_ptr!(ssn_ptr)?;
                // A close that won the race stays closed.
                if ssn.status.state == state {
                    ssn.status.state = previous;
                }
                return Err(e);
            }
        }

        // A close that won the race already published Closed; it is returned as is.
        let ssn = lock_ptr!(ssn_ptr)?;
        Ok(ssn.clone())
    }

    pub fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let ssn_ptr = self.get_session_ptr(id)?;
        let ssn = lock_ptr!(ssn_ptr)?;
        let mut ssn = ssn.clone();
        ssn.events = event_result(
            self.event_manager
                .find_events(EventOwner::session(ssn.id.clone())),
            "load",
            &ssn.id,
        );
        Ok(ssn)
    }

    pub fn get_session_ptr(&self, id: SessionID) -> Result<SessionPtr, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;

        ssn_map
            .get(&id)
            .ok_or(FlameError::NotFound(id.to_string()))
            .cloned()
    }

    pub async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        trace_fn!("Storage::open_session");

        // Check if session already exists in cache - if so, return it directly
        // to preserve in-memory task state
        {
            let ssn_map = lock_ptr!(self.sessions)?;
            if let Some(ssn_ptr) = ssn_map.get(&id) {
                let ssn = lock_ptr!(ssn_ptr)?;
                // A suspended session is resumed, never reopened or replaced.
                if ssn.is_suspending() || ssn.is_suspended() {
                    return Err(FlameError::InvalidState(format!(
                        "Session <{id}> is suspended"
                    )));
                }
                // Verify the session is still open before returning cached version
                if ssn.status.state == SessionState::Open {
                    // If spec provided, validate it matches the existing session
                    if let Some(ref attr) = spec {
                        ssn.validate_spec(attr)?;
                    }
                    tracing::debug!(
                        "Session <{}> already exists in cache with {} tasks, returning cached version",
                        id,
                        ssn.tasks.len()
                    );
                    return Ok(ssn.clone());
                }
            }
        }

        // Session not in cache or not open, delegate to engine for atomic get-or-create operation
        let ssn = self.engine.open_session(id.clone(), spec).await?;

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.clone().into()));
        }

        self.evict_sessions()?;

        Ok(ssn)
    }

    pub fn get_task_ptr(&self, gid: TaskGID) -> Result<TaskPtr, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;
        let ssn_ptr = ssn_map
            .get(&gid.ssn_id)
            .ok_or(FlameError::NotFound(gid.ssn_id.to_string()))?;

        let ssn = lock_ptr!(ssn_ptr)?;
        let task_ptr = ssn
            .tasks
            .get(&gid.task_id)
            .ok_or(FlameError::NotFound(gid.to_string()))?;

        Ok(task_ptr.clone())
    }

    pub async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let cached = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.get(&id).cloned()
        };

        let ssn = match cached.as_ref() {
            Some(ssn_ptr) => {
                let ssn = lock_ptr!(ssn_ptr)?.clone();
                if let Err(error) = self.engine.delete_session(id.clone()).await {
                    if !matches!(error, FlameError::NotFound(_)) {
                        return Err(error);
                    }
                }
                ssn
            }
            None => self.engine.delete_session(id.clone()).await?,
        };

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.remove(&id);
        }

        if let Some(ssn_ptr) = cached {
            let mut cached_ssn = lock_ptr!(ssn_ptr)?;
            cached_ssn.status.state = SessionState::Closed;
            cached_ssn.events.clear();
            event_result(self.event_manager.remove_events(id.clone()), "remove", &id);
        } else {
            event_result(self.event_manager.remove_events(id.clone()), "remove", &id);
        }

        Ok(ssn)
    }

    pub fn list_sessions(
        &self,
        filter: Option<&SessionFilter>,
    ) -> Result<Vec<Session>, FlameError> {
        if filter.and_then(|filter| filter.limit) == Some(0) {
            return Ok(Vec::new());
        }
        let mut ssn_list = vec![];
        let ssn_map = lock_ptr!(self.sessions)?;

        for ssn in ssn_map.deref().values() {
            let ssn = lock_ptr!(ssn)?;
            let mut ssn = ssn.clone();
            ssn.events = event_result(
                self.event_manager
                    .find_events(EventOwner::session(ssn.id.clone())),
                "load",
                &ssn.id,
            );
            let matches = filter.is_none_or(|filter| {
                filter
                    .application
                    .as_ref()
                    .is_none_or(|application| ssn.application == *application)
                    && filter.state.is_none_or(|state| ssn.status.state == state)
                    && filter.ids.as_ref().is_none_or(|ids| ids.contains(&ssn.id))
            });
            if !matches {
                continue;
            }
            if let Some(predicate) = filter.and_then(|filter| filter.predicate) {
                if !predicate.matches(ssn.retry_count, self.session_retry_limits()) {
                    continue;
                }
            }
            ssn_list.push(ssn);
            if filter
                .and_then(|filter| filter.limit)
                .is_some_and(|limit| ssn_list.len() >= limit)
            {
                break;
            }
        }

        Ok(ssn_list)
    }

    /// Lists executors with optional filtering.
    ///
    /// # Arguments
    /// * `filter` - Filter criteria (state, node, and/or executor IDs).
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
                .filter_map(|exe_ptr| lock_ptr!(exe_ptr).ok().map(|e| e.clone()))
                .collect());
        };

        let exe_list: Vec<Executor> = exe_map
            .values()
            .filter_map(|exe_ptr| {
                let exe = lock_ptr!(exe_ptr).ok()?;

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

                // Filter by ids if specified
                // Some([]) matches nothing, Some([a,b]) matches a or b, None matches all
                if let Some(ref ids) = filter.ids {
                    if !ids.contains(&exe.id) {
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
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        trace_fn!("Storage::create_task");
        let ssn = self.get_session_ptr(ssn_id.clone())?;
        // Check before the engine writes, so a rejected task leaves nothing behind. A
        // suspending or suspended session accepts tasks; they run after the resume.
        {
            let ssn = lock_ptr!(ssn)?;
            if ssn.is_closed() {
                return Err(FlameError::InvalidState(format!(
                    "Session <{}> is closed",
                    ssn.id
                )));
            }
        }
        let task = self
            .engine
            .create_task(ssn_id.clone(), task_input, options)
            .await?;

        {
            let mut ssn = lock_ptr!(ssn)?;
            if ssn.is_closed() {
                return Err(FlameError::InvalidState(format!(
                    "Session <{}> is closed",
                    ssn.id
                )));
            }
            ssn.update_task(&task)?;
        }

        self.record_event(
            EventOwner::from(&task),
            Event {
                code: task.state.into(),
                message: Some(format!("Task was created with state <{:?}>", task.state)),
                creation_time: Utc::now(),
            },
        )
        .await?;

        Ok(task)
    }

    pub fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        let mut task = self.get_task_metadata(ssn_id, id)?;
        task.events = event_result(
            self.event_manager.find_events(EventOwner::from(task.gid())),
            "load",
            &task.ssn_id,
        );

        Ok(task)
    }

    /// Clone the current task state without loading historical events from disk.
    /// Nonterminal watch updates need current state without serializing
    /// unrelated sessions behind event storage I/O.
    pub fn get_task_metadata(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        let task_ptr = self.get_task_ptr(TaskGID {
            ssn_id,
            task_id: id,
        })?;
        let mut task = lock_ptr!(task_ptr)?.clone();
        task.events.clear();
        Ok(task)
    }

    pub fn list_tasks(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;
        let ssn = ssn_map
            .get(&ssn_id)
            .ok_or(FlameError::NotFound(ssn_id.to_string()))?;

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

    pub async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        self.engine.get_application(id).await
    }

    pub async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let app = self.engine.register_application(name, attr).await?;

        let mut app_map = lock_ptr!(self.applications)?;
        // just lock the sessions to avoid cache mismatch.
        let _unused = lock_ptr!(self.sessions)?;

        app_map.insert(app.name.clone(), stdng::new_ptr(app.clone()));

        Ok(())
    }

    pub async fn update_application_state(
        &self,
        name: ApplicationID,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let app = self
            .engine
            .update_application_state(name.clone(), state)
            .await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.insert(name, stdng::new_ptr(app.clone()));
        Ok(app)
    }

    pub async fn delete_application(&self, name: ApplicationID) -> Result<(), FlameError> {
        self.engine.delete_application(name.clone()).await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.remove(&name);

        Ok(())
    }

    pub async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        let app = self.engine.update_application(name.clone(), attr).await?;

        let mut app_map = lock_ptr!(self.applications)?;
        app_map.insert(name.clone(), stdng::new_ptr(app.clone()));

        Ok(())
    }

    pub async fn list_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        self.engine.find_applications(filter).await
    }

    pub async fn session_application(&self, id: SessionID) -> Result<ApplicationID, FlameError> {
        let cached = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.get(&id).cloned()
        };
        if let Some(ssn) = cached {
            return Ok(lock_ptr!(ssn)?.application.clone());
        }

        Ok(self.engine.get_session(id).await?.application)
    }

    pub fn count_session(&self, filter: &SessionFilter) -> Result<usize, FlameError> {
        Ok(self.list_sessions(Some(filter))?.len())
    }

    pub async fn update_task_state(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_state");
        let gid = TaskGID {
            ssn_id: {
                let ssn_ptr = lock_ptr!(ssn)?;
                ssn_ptr.id.clone()
            },
            task_id: {
                let task_ptr = lock_ptr!(task)?;
                task_ptr.id
            },
        };

        // Check before the engine writes, so a rejected update leaves nothing behind.
        // A suspending session lets its running tasks finish; only a closed one rejects.
        {
            let ssn_ptr = lock_ptr!(ssn)?;
            if ssn_ptr.is_closed() {
                return Err(FlameError::InvalidState(format!(
                    "Session <{}> is closed",
                    ssn_ptr.id
                )));
            }
        }

        let updated_task = match self
            .engine
            .update_task_state(gid.clone(), task_state, message)
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

        {
            let mut ssn_ptr = lock_ptr!(ssn)?;
            if ssn_ptr.is_closed() {
                return Err(FlameError::InvalidState(format!(
                    "Session <{}> is closed",
                    ssn_ptr.id
                )));
            }
            ssn_ptr.update_task(&updated_task)?;
        }

        self.record_event(
            EventOwner::from(updated_task.gid()),
            Event {
                code: task_state.into(),
                message: Some(format!("Task state was updated to <{:?}>", task_state)),
                creation_time: Utc::now(),
            },
        )
        .await?;

        Ok(())
    }

    pub async fn update_task_result(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_result");
        let gid = TaskGID {
            ssn_id: {
                let ssn_ptr = lock_ptr!(ssn)?;
                ssn_ptr.id.clone()
            },
            task_id: {
                let task_ptr = lock_ptr!(task)?;
                task_ptr.id
            },
        };

        let task_state = task_result.state;
        let task_message = task_result.message.clone();
        let task_output = task_result.output.clone();

        // Check before the engine writes, so a rejected update leaves nothing behind.
        // A suspending session lets its running tasks finish; only a closed one rejects.
        {
            let ssn_ptr = lock_ptr!(ssn)?;
            if ssn_ptr.is_closed() {
                return Err(FlameError::InvalidState(format!(
                    "Session <{}> is closed",
                    ssn_ptr.id
                )));
            }
        }

        let updated_task = match self
            .engine
            .update_task_result(gid.clone(), task_result)
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

        {
            let mut ssn_ptr = lock_ptr!(ssn)?;
            if ssn_ptr.is_closed() {
                return Err(FlameError::InvalidState(format!(
                    "Session <{}> is closed",
                    ssn_ptr.id
                )));
            }
            ssn_ptr.update_task(&updated_task)?;
        }

        let event_message = match task_state {
            TaskState::Failed => {
                task_message.unwrap_or_else(|| format!("Task failed with state <{:?}>", task_state))
            }
            _ => format!("Task was completed with state <{:?}>", task_state),
        };

        self.record_event(
            EventOwner::from(updated_task.gid()),
            Event {
                code: updated_task.state.into(),
                message: Some(event_message),
                creation_time: Utc::now(),
            },
        )
        .await?;

        Ok(())
    }

    pub async fn create_executor(
        &self,
        node_name: String,
        ssn_id: SessionID,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Storage::create_executor");
        let ssn = self.get_session_ptr(ssn_id.clone())?;

        let (application, resreq) = {
            let ssn = lock_ptr!(ssn)?;
            let resreq = ssn.resreq.clone().ok_or_else(|| {
                FlameError::InvalidState(format!(
                    "session <{}> has no resreq; resolve_session_resreq must populate it",
                    ssn_id
                ))
            })?;
            (ssn.application.clone(), resreq)
        };
        let shim = {
            let applications = lock_ptr!(self.applications)?;
            match applications.get(&application) {
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
        let e = Executor {
            id: Uuid::new_v4().to_string(),
            node: node_name.clone(),
            resreq,
            shim,
            application,
            task_id: None,
            ssn_id: None,
            signal: None,
            attributes: Default::default(),
            creation_time: now,
            latest_updated_timestamp: now,
            state: ExecutorState::Void,
        };

        self.engine.create_executor(&e).await?;

        let mut exe_map = lock_ptr!(self.executors)?;
        let exe = ExecutorPtr::new(e.clone().into());
        exe_map.insert(e.id.clone(), exe.clone());

        Ok(e.clone())
    }

    pub fn get_executor_ptr(&self, id: ExecutorID) -> Result<ExecutorPtr, FlameError> {
        let exe_map = lock_ptr!(self.executors)?;
        let exe = exe_map
            .get(&id)
            .ok_or(FlameError::NotFound(id.to_string()))?;

        Ok(exe.clone())
    }

    pub async fn update_executor(&self, executor: &Executor) -> Result<(), FlameError> {
        trace_fn!("Storage::update_executor");
        self.engine.update_executor(executor).await?;

        let exe_map = lock_ptr!(self.executors)?;
        if let Some(exe_ptr) = exe_map.get(&executor.id) {
            let mut exe = lock_ptr!(exe_ptr)?;
            exe.set_state(executor.state);
            exe.task_id = executor.task_id;
            exe.ssn_id = executor.ssn_id.clone();
            exe.latest_updated_timestamp = executor.latest_updated_timestamp;
        }

        Ok(())
    }

    pub async fn delete_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Storage::delete_executor");
        self.engine.delete_executor(&id).await?;

        let mut exe_map = lock_ptr!(self.executors)?;
        exe_map.remove(&id);

        Ok(())
    }

    pub async fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        trace_fn!("Storage::record_event");
        let session = self.get_session_ptr(owner.session_id.clone())?;
        let session = lock_ptr!(session)?;
        // Serialize event recording with cached Closed publication and cleanup.
        // Late diagnostic notifications must not block executor cleanup.
        if session.status.state == SessionState::Closed {
            return Ok(());
        }
        let session_id = owner.session_id.clone();
        event_result(
            self.event_manager.record_event(owner, event),
            "record",
            &session_id,
        );
        Ok(())
    }
}

/// Events are diagnostic: a failure is logged and never fails the operation.
fn event_result<T: Default>(result: Result<T, FlameError>, action: &str, session_id: &str) -> T {
    result.unwrap_or_else(|e| {
        tracing::error!("Failed to {action} events of session <{session_id}>: {e}");
        T::default()
    })
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
mod suspend_tests;

#[cfg(test)]
mod task_tests;

#[cfg(test)]
mod session_limit_tests;

#[cfg(test)]
mod load_data_tests;

#[cfg(test)]
mod derive_events_path_tests;

#[cfg(test)]
mod event_lifecycle_tests {
    use super::*;
    use crate::events::EventManager;

    fn event(message: &str) -> Event {
        Event {
            code: 1,
            message: Some(message.to_string()),
            creation_time: Utc::now(),
        }
    }

    async fn test_storage(url: &str, events: EventManagerPtr) -> Storage {
        let context = FlameClusterContext {
            cluster: crate::ctx::FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut storage = (*new_ptr(&context).await.unwrap()).clone();
        storage.engine = engine::connect(url).await.unwrap();
        storage.event_manager = events;
        storage
    }

    fn test_urls(dir: &std::path::Path) -> [String; 3] {
        [
            "none".to_string(),
            format!("sqlite://{}", dir.join("events.db").display()),
            format!("fs://{}", dir.join("filesystem").display()),
        ]
    }

    #[tokio::test]
    async fn close_removes_session_and_task_events_and_skips_late_writes() {
        let engine_dir = tempfile::tempdir().unwrap();
        for url in test_urls(engine_dir.path()) {
            let dir = tempfile::tempdir().unwrap();
            let events = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());
            let storage = test_storage(&url, events).await;
            let session = storage
                .create_session(SessionAttributes {
                    id: "close-events".to_string(),
                    application: "test-app".to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let owner = EventOwner::session(session.id.clone());
            storage
                .record_event(owner.clone(), event("session"))
                .await
                .unwrap();
            storage
                .record_event(
                    EventOwner {
                        session_id: session.id.clone(),
                        task_id: 1,
                    },
                    event("task"),
                )
                .await
                .unwrap();
            assert_eq!(
                storage
                    .get_session(session.id.clone())
                    .unwrap()
                    .events
                    .len(),
                1
            );

            let closed = storage.close_session(session.id.clone()).await.unwrap();
            assert_eq!(closed.status.state, SessionState::Closed);
            assert!(closed.events.is_empty());
            assert!(storage
                .get_session(session.id.clone())
                .unwrap()
                .events
                .is_empty());
            assert!(!dir.path().join(&session.id).exists());
            storage.record_event(owner, event("late")).await.unwrap();
            assert!(!dir.path().join(&session.id).exists());
        }
    }

    #[tokio::test]
    async fn failed_close_preserves_open_session_events() {
        let engine_dir = tempfile::tempdir().unwrap();
        for url in test_urls(engine_dir.path()) {
            let dir = tempfile::tempdir().unwrap();
            let storage = test_storage(
                &url,
                Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap()),
            )
            .await;
            let id = "running-events".to_string();
            storage
                .create_session(SessionAttributes {
                    id: id.clone(),
                    application: "test-app".to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let owner = EventOwner::session(id.clone());
            storage
                .record_event(owner.clone(), event("before"))
                .await
                .unwrap();
            let task = storage.create_task(id.clone(), None, None).await.unwrap();
            storage
                .update_task_state(
                    storage.get_session_ptr(id.clone()).unwrap(),
                    storage.get_task_ptr(task.gid()).unwrap(),
                    TaskState::Running,
                    None,
                )
                .await
                .unwrap();
            let previous = storage.get_task(id.clone(), task.id).unwrap().events.len();
            assert!(storage.close_session(id.clone()).await.is_err());
            let session = storage.get_session(id.clone()).unwrap();
            assert_eq!(session.status.state, SessionState::Open);
            assert_eq!(session.events[0].message.as_deref(), Some("before"));
            assert_eq!(
                storage.get_task(id.clone(), task.id).unwrap().events.len(),
                previous
            );
            assert!(dir.path().join(&id).exists());
            storage.record_event(owner, event("after")).await.unwrap();
            assert_eq!(storage.get_session(id).unwrap().events.len(), 2);
        }
    }

    #[tokio::test]
    async fn delete_drops_events_from_writers_with_a_retained_cache_pointer() {
        let engine_dir = tempfile::tempdir().unwrap();
        for url in test_urls(engine_dir.path()) {
            let events = Arc::new(MemoryEventManager::new());
            let storage = test_storage(&url, events.clone()).await;
            let id = "delete-events".to_string();
            storage
                .create_session(SessionAttributes {
                    id: id.clone(),
                    application: "test-app".to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let retained = storage.get_session_ptr(id.clone()).unwrap();
            storage
                .record_event(EventOwner::session(id.clone()), event("before"))
                .await
                .unwrap();
            if url != "none" {
                // Persistent engines require Closed before deletion. Keep the
                // retained cache pointer Open to model a delayed cache writer.
                storage.engine.close_session(id.clone()).await.unwrap();
            }
            storage.delete_session(id.clone()).await.unwrap();
            assert_eq!(
                lock_ptr!(retained).unwrap().status.state,
                SessionState::Closed
            );
            assert!(matches!(
                storage
                    .record_event(EventOwner::session(id.clone()), event("late"))
                    .await,
                Err(FlameError::NotFound(_))
            ));
            assert!(events
                .find_events(EventOwner::session(id))
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn recovery_cleans_closed_events_and_keeps_open_events() {
        let engine_dir = tempfile::tempdir().unwrap();
        for url in test_urls(engine_dir.path()).into_iter().skip(1) {
            let dir = tempfile::tempdir().unwrap();
            let events = Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap());
            let storage = test_storage(&url, events).await;
            for id in ["closed", "open"] {
                storage
                    .create_session(SessionAttributes {
                        id: id.to_string(),
                        application: "test-app".to_string(),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                storage
                    .record_event(EventOwner::session(id.to_string()), event(id))
                    .await
                    .unwrap();
            }
            // Model an older server or a crash after persisting Closed but before
            // releasing its event files.
            storage
                .engine
                .close_session("closed".to_string())
                .await
                .unwrap();
            let recovered = Storage {
                sessions: stdng::new_ptr(HashMap::new()),
                event_manager: Arc::new(FsEventManager::new(dir.path().to_str().unwrap()).unwrap()),
                ..storage.clone()
            };
            recovered.load_data().await.unwrap();
            assert!(recovered
                .get_session("closed".to_string())
                .unwrap()
                .events
                .is_empty());
            assert!(!dir.path().join("closed").exists());
            assert_eq!(
                recovered.get_session("open".to_string()).unwrap().events[0]
                    .message
                    .as_deref(),
                Some("open")
            );
            assert!(dir.path().join("open").exists());
        }
    }

    #[tokio::test]
    async fn recovery_preserves_running_task_history_in_closed_sessions() {
        let engine_dir = tempfile::tempdir().unwrap();
        let url = test_urls(engine_dir.path())[1].clone();
        let storage = test_storage(&url, Arc::new(MemoryEventManager::new())).await;
        let id = "closed-running-history".to_string();
        storage
            .create_session(SessionAttributes {
                id: id.clone(),
                application: "test-app".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let task = storage.create_task(id.clone(), None, None).await.unwrap();
        storage.engine.close_session(id.clone()).await.unwrap();
        // Older engines allowed late task writers to publish Running after
        // Closed. Recovery preserves that history without retrying its task.
        let persisted = storage
            .engine
            .update_task_state(task.gid(), TaskState::Running, None)
            .await
            .unwrap();
        let recovered = Storage {
            sessions: stdng::new_ptr(HashMap::new()),
            ..storage.clone()
        };
        recovered.load_data().await.unwrap();
        assert_eq!(
            recovered.get_session(id.clone()).unwrap().status.state,
            SessionState::Closed
        );
        let history = recovered.get_task_metadata(id, task.id).unwrap();
        assert_eq!(history.state, TaskState::Running);
        assert_eq!(history.version, persisted.version);
        let on_disk = storage.engine.get_task(task.gid()).await.unwrap();
        assert_eq!(on_disk.state, TaskState::Running);
        assert_eq!(on_disk.version, persisted.version);
    }

    struct CleanupFailure(MemoryEventManager);

    impl EventManager for CleanupFailure {
        fn setup_events(&self, id: SessionID) -> Result<(), FlameError> {
            self.0.setup_events(id)
        }
        fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
            self.0.record_event(owner, event)
        }
        fn find_events(&self, owner: EventOwner) -> Result<Vec<Event>, FlameError> {
            self.0.find_events(owner)
        }
        fn remove_events(&self, _id: SessionID) -> Result<(), FlameError> {
            Err(FlameError::Storage("cleanup failed".to_string()))
        }
        fn clear(&self) -> Result<(), FlameError> {
            self.0.clear()
        }
    }

    #[tokio::test]
    async fn cleanup_failure_keeps_cached_session_closed() {
        let storage =
            test_storage("none", Arc::new(CleanupFailure(MemoryEventManager::new()))).await;
        storage
            .create_session(SessionAttributes {
                id: "failure".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        // Event cleanup failure is logged, not returned.
        storage.close_session("failure".to_string()).await.unwrap();
        assert_eq!(
            storage
                .get_session("failure".to_string())
                .unwrap()
                .status
                .state,
            SessionState::Closed
        );
        storage
            .record_event(EventOwner::session("failure".to_string()), event("late"))
            .await
            .unwrap();
        assert!(storage
            .get_session("failure".to_string())
            .unwrap()
            .events
            .is_empty());
    }
}

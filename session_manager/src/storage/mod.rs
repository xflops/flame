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

use chrono::Utc;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use uuid::Uuid;

use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use common::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationPtr, CommonData, Event,
    EventOwner, ExecutorID, ExecutorState, Node, NodePtr, ResourceRequirement, Session,
    SessionAttributes, SessionID, SessionPtr, SessionState, Shim, Task, TaskGID, TaskID, TaskInput,
    TaskOutput, TaskPtr, TaskResult, TaskState,
};
use common::ctx::FlameClusterContext;
use common::FlameError;

use crate::model::{
    AppInfo, Executor, ExecutorFilter, ExecutorInfo, ExecutorPtr, NodeInfo, NodeInfoPtr,
    SessionInfo, SessionInfoPtr, SnapShot, SnapShotPtr,
};

use crate::events::{EventManagerPtr, FsEventManager, MemoryEventManager};
use crate::storage::engine::EnginePtr;

mod engine;

pub type StoragePtr = Arc<Storage>;

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

fn derive_events_path(storage_url: &str) -> String {
    if let Ok(test_dir) = std::env::var("FLAME_TEST_DIR") {
        return std::path::Path::new(&test_dir)
            .join("events")
            .to_string_lossy()
            .to_string();
    }

    "events".to_string()
}

impl Storage {
    pub fn snapshot(&self) -> Result<SnapShotPtr, FlameError> {
        let res = SnapShot::new(self.context.cluster.slot.clone());

        {
            let node_map = lock_ptr!(self.nodes)?;
            tracing::debug!("There are {} nodes in snapshot.", node_map.len());
            for node in node_map.deref().values() {
                let node = lock_ptr!(node)?;
                let info = NodeInfo::from(&(*node));
                res.add_node(Arc::new(info))?;
            }
        }

        {
            let ssn_map = lock_ptr!(self.sessions)?;
            tracing::debug!("There are {} sessions in snapshot.", ssn_map.len());
            for ssn in ssn_map.deref().values() {
                let ssn = lock_ptr!(ssn)?;
                let info = SessionInfo::from(&(*ssn));
                res.add_session(Arc::new(info))?;
            }
        }

        {
            let exe_map = lock_ptr!(self.executors)?;
            tracing::debug!("There are {} executors in snapshot.", exe_map.len());
            for exe in exe_map.deref().values() {
                let exe = lock_ptr!(exe)?;
                tracing::debug!(
                    "Executor <{}> state={:?}, ssn_id={:?}",
                    exe.id,
                    exe.state,
                    exe.ssn_id
                );
                let info = ExecutorInfo::from(&(*exe));
                res.add_executor(Arc::new(info))?;
            }
        }

        {
            let app_map = lock_ptr!(self.applications)?;
            tracing::debug!("There are {} applications in snapshot.", app_map.len());
            for app in app_map.deref().values() {
                let app = lock_ptr!(app)?;
                let info = AppInfo::from(&(*app));
                res.add_application(Arc::new(info))?;
            }
        }

        Ok(Arc::new(res))
    }

    pub async fn load_data(&self) -> Result<(), FlameError> {
        let ssn_list = self.engine.find_session().await?;
        for ssn in ssn_list {
            let task_list = self.engine.find_tasks(ssn.id.clone()).await?;
            let mut ssn = ssn.clone();
            for task in task_list {
                let task = match task.state {
                    TaskState::Running => self.engine.retry_task(task.gid()).await?,
                    _ => task,
                };

                ssn.update_task(&task);
            }

            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.into()));
        }

        let app_list = self.engine.find_application().await?;
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
                recovered.state = ExecutorState::Idle;
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
        state: common::apis::NodeState,
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
    pub fn list_node(&self) -> Result<Vec<Node>, FlameError> {
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
                    slots: exec.slots,
                    shim: exec.shim,
                    task_id: exec.task_id,
                    ssn_id: exec.ssn_id.clone(),
                    creation_time: exec.creation_time,
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

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.insert(ssn.id.clone(), SessionPtr::new(ssn.clone().into()));
        }

        self.evict_sessions()?;

        Ok(ssn)
    }

    pub async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        trace_fn!("Storage::close_session");

        let ssn_ptr = {
            let ssn_map = lock_ptr!(self.sessions)?;
            ssn_map
                .get(&id)
                .cloned()
                .ok_or(FlameError::NotFound(format!("session <{}>", id)))?
        };

        let result_ssn = {
            let mut ssn = lock_ptr!(ssn_ptr)?;
            ssn.status.state = SessionState::Closed;
            ssn.completion_time = Some(Utc::now());
            ssn.version += 1;
            ssn.clone()
        };

        if let Err(e) = self.engine.close_session(id.clone()).await {
            if !matches!(e, FlameError::NotFound(_)) {
                return Err(e);
            }
        }

        self.evict_sessions()?;

        Ok(result_ssn)
    }

    pub fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let ssn_ptr = self.get_session_ptr(id)?;
        let ssn = lock_ptr!(ssn_ptr)?;
        Ok(ssn.clone())
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
        let ssn = {
            let ssn_map = lock_ptr!(self.sessions)?;
            let ssn_ptr = ssn_map
                .get(&id)
                .ok_or_else(|| FlameError::NotFound(format!("session <{}>", id)))?;
            let ssn = lock_ptr!(ssn_ptr)?;
            ssn.clone()
        };

        if let Err(e) = self.engine.delete_session(id.clone()).await {
            if !matches!(e, FlameError::NotFound(_)) {
                return Err(e);
            }
        }

        {
            let mut ssn_map = lock_ptr!(self.sessions)?;
            ssn_map.remove(&id);
        }

        self.event_manager.remove_events(id)?;

        Ok(ssn)
    }

    pub fn list_session(&self) -> Result<Vec<Session>, FlameError> {
        let mut ssn_list = vec![];
        let ssn_map = lock_ptr!(self.sessions)?;

        for ssn in ssn_map.deref().values() {
            let ssn = lock_ptr!(ssn)?;
            ssn_list.push(ssn.clone());
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
    pub fn list_executor(
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
    ) -> Result<Task, FlameError> {
        trace_fn!("Storage::create_task");
        let task = self.engine.create_task(ssn_id.clone(), task_input).await?;

        let ssn = self.get_session_ptr(ssn_id.clone())?;
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

    pub fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        let ssn_map = lock_ptr!(self.sessions)?;

        let ssn = ssn_map
            .get(&ssn_id)
            .ok_or(FlameError::NotFound(ssn_id.to_string()))?;

        let ssn = lock_ptr!(ssn)?;
        let task = ssn
            .tasks
            .get(&id)
            .ok_or(FlameError::NotFound(id.to_string()))?;
        let mut task = lock_ptr!(task)?;
        let events = self
            .event_manager
            .find_events(EventOwner::from(task.gid()))?;
        task.events = events;

        Ok(task.clone())
    }

    pub fn list_task(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
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

    pub async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        self.engine.unregister_application(name.clone()).await?;

        {
            let mut app_map = lock_ptr!(self.applications)?;
            let mut ssn_map = lock_ptr!(self.sessions)?;

            app_map.remove(&name);

            ssn_map.retain(|_, ssn| {
                let ssn_ptr = lock_ptr!(ssn);
                match ssn_ptr {
                    Ok(ssn) => ssn.application != name,
                    Err(_) => true,
                }
            });
        }

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

    pub async fn list_application(&self) -> Result<Vec<Application>, FlameError> {
        self.engine.find_application().await
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

        let updated_task = match self
            .engine
            .update_task_state(gid.clone(), task_state, message)
            .await
        {
            Ok(task) => task,
            Err(FlameError::NotFound(_)) => {
                let mut task_ptr = lock_ptr!(task)?;
                task_ptr.state = task_state;
                task_ptr.version += 1;
                task_ptr.clone()
            }
            Err(e) => return Err(e),
        };

        let mut ssn_ptr = lock_ptr!(ssn)?;
        ssn_ptr.update_task(&updated_task)?;

        self.event_manager.record_event(
            EventOwner::from(updated_task.gid()),
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

        let updated_task = match self
            .engine
            .update_task_result(gid.clone(), task_result)
            .await
        {
            Ok(task) => task,
            Err(FlameError::NotFound(_)) => {
                let mut task_ptr = lock_ptr!(task)?;
                task_ptr.state = task_state;
                task_ptr.version += 1;
                task_ptr.completion_time = Some(Utc::now());
                task_ptr.output = task_output;
                task_ptr.clone()
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
            EventOwner::from(updated_task.gid()),
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
        ssn_id: SessionID,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Storage::create_executor");
        let ssn = self.get_session_ptr(ssn_id.clone())?;

        let (resreq, slots) = {
            let ssn = lock_ptr!(ssn)?;
            (
                ResourceRequirement::new(ssn.slots, &self.context.cluster.slot),
                ssn.slots,
            )
        };

        let e = Executor {
            id: Uuid::new_v4().to_string(),
            node: node_name.clone(),
            resreq,
            slots,
            shim: Shim::default(),
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
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
            exe.state = executor.state;
            exe.task_id = executor.task_id;
            exe.ssn_id = executor.ssn_id.clone();
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
        self.event_manager.record_event(owner, event)
    }
}

#[cfg(test)]
mod node_tests;

#[cfg(test)]
mod node_executor_tests;

#[cfg(test)]
mod session_limit_tests;

#[cfg(test)]
mod load_data_tests;

#[cfg(test)]
mod derive_events_path_tests;

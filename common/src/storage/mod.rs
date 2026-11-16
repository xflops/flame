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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use stdng::{ensure, lock_ptr, trace_fn, MutexPtr};

use crate::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationPtr, ApplicationState, Event,
    EventOwner, ExecutorID, ExecutorState, Node, NodePtr, NodeState, Session, SessionAttributes,
    SessionID, SessionPtr, SessionState, Shim, Task, TaskGID, TaskID, TaskInput, TaskOptions,
    TaskPtr, TaskResult, TaskState,
};
use crate::ctx::FlameClusterContext;
use crate::FlameError;

use crate::apis::{
    paginate, ApplicationFilter, Executor, ExecutorFilter, ExecutorPtr, NodeFilter, Page,
    SessionFilter, TaskFilter,
};

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

/// Select exact identities directly without scanning the entire cache.
fn selected_pointers<T>(
    entries: &HashMap<String, MutexPtr<T>>,
    ids: Option<&[String]>,
) -> Vec<MutexPtr<T>> {
    match ids {
        Some(ids) => ids
            .iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter_map(|id| entries.get(id).cloned())
            .collect(),
        None => entries.values().cloned().collect(),
    }
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
    session_admission: Arc<tokio::sync::Mutex<()>>,
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
        session_admission: Arc::new(tokio::sync::Mutex::new(())),
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
    pub fn context(&self) -> &FlameClusterContext {
        &self.context
    }

    pub fn session_retry_limits(&self) -> u32 {
        self.context.cluster.recovery.session.retry_limits
    }

    pub fn snapshot(&self) -> Result<StorageSnapshot, FlameError> {
        let applications = lock_ptr!(self.applications)?
            .values()
            .map(|ptr| Ok((*lock_ptr!(ptr)?).clone()))
            .collect::<Result<Vec<_>, FlameError>>()?;
        // Closed sessions remain visible to scheduler lifecycle decisions.
        // Suspended execution sessions have no demand and remain omitted.
        let live = lock_ptr!(self.sessions)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut sessions = Vec::new();
        for shared in live {
            let session = lock_ptr!(shared)?;
            if !session.is_suspended() {
                sessions.push(session.clone());
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
        for metadata in self.engine.find_session_metadata().await? {
            let mut session = self.engine.get_session(metadata.id.clone()).await?;
            if session.is_closed() {
                event_result(
                    self.event_manager.remove_events(session.id.clone()),
                    "remove",
                    &session.id,
                );
            } else {
                event_result(
                    self.event_manager.setup_events(session.id.clone()),
                    "create",
                    &session.id,
                );
            }
            session.task_counts = Default::default();
            for header in self
                .engine
                .find_task_metadata(&TaskFilter::by_session(session.id.clone()))
                .await?
            {
                if session.is_closed() || header.state.is_terminal() {
                    session.task_counts.increment(header.state);
                    continue;
                }
                let task = if session.is_open() && header.state == TaskState::Running {
                    self.engine.retry_task(header.gid()).await?
                } else {
                    self.engine.get_task(header.gid()).await?
                };
                session.task_counts.increment(task.state);
                session.cache_task(&task);
            }
            {
                let mut directory = lock_ptr!(self.sessions)?;
                directory.insert(session.id.clone(), stdng::new_ptr(session));
            }
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

    /// Reject Closed sessions for operations requiring active execution.
    fn require_live_session(&self, id: &str) -> Result<SessionPtr, FlameError> {
        let session = self.get_session_ptr(id.to_string())?;
        ensure!(
            !lock_ptr!(session)?.is_closed(),
            FlameError::InvalidState(format!("Session <{id}> is closed"))
        )?;
        Ok(session)
    }

    /// Call while holding the lifecycle admission gate; no directory lock crosses I/O.
    fn admit_open_session(&self) -> Result<(), FlameError> {
        let Some(max) = self.max_sessions else {
            return Ok(());
        };
        let directory = lock_ptr!(self.sessions)?;
        let mut open = 0;
        for session in directory.values() {
            if lock_ptr!(session)?.is_open() {
                open += 1;
            }
        }
        ensure!(
            open < max,
            FlameError::InvalidState(format!("Open session limit ({max}) reached"))
        )
    }

    pub async fn register_node(&self, node: &Node) -> Result<(), FlameError> {
        trace_fn!("Storage::register_node");

        let existing = lock_ptr!(self.nodes)?.get(&node.name).cloned();
        let mut registered = node.clone();
        let mut registered = if let Some(existing) = existing {
            registered.creation_time = lock_ptr!(existing)?.creation_time;
            self.engine.update_node(&registered).await?
        } else {
            registered.creation_time = Utc::now();
            self.engine.create_node(&registered).await?
        };
        {
            let mut nodes = lock_ptr!(self.nodes)?;
            if let Some(existing) = nodes.get(&registered.name) {
                registered.creation_time = lock_ptr!(existing)?.creation_time;
            }
            nodes.insert(registered.name.clone(), stdng::new_ptr(registered));
        }
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

    /// Lists nodes held by the controller cache.
    pub fn list_nodes(&self, filter: Option<&NodeFilter>) -> Result<Page<Node>, FlameError> {
        let shared = {
            let entries = lock_ptr!(self.nodes)?;
            selected_pointers(&entries, filter.and_then(|filter| filter.names.as_deref()))
        };
        let mut nodes = Vec::with_capacity(shared.len());
        for shared in shared {
            nodes.push(lock_ptr!(shared)?.clone());
        }
        nodes.retain(|node| {
            filter.is_none_or(|filter| filter.state.is_none_or(|state| state == node.state))
        });
        nodes.sort_by(|left, right| {
            (right.state == NodeState::Ready)
                .cmp(&(left.state == NodeState::Ready))
                .then_with(|| right.creation_time.cmp(&left.creation_time))
                .then_with(|| left.name.cmp(&right.name))
        });
        paginate(nodes, filter.and_then(|filter| filter.paging))
    }

    /// # Deprecated
    /// Use `WatchNode` streaming RPC instead for better efficiency.
    #[deprecated(since = "0.6.0", note = "Use WatchNode streaming RPC instead")]
    pub async fn sync_node(
        &self,
        node: &Node,
        _: &Vec<Executor>,
    ) -> Result<Vec<Executor>, FlameError> {
        let existing = lock_ptr!(self.nodes)?.get(&node.name).cloned();
        let mut registered = node.clone();
        let mut registered = if let Some(existing) = existing {
            registered.creation_time = lock_ptr!(existing)?.creation_time;
            self.engine.update_node(&registered).await?
        } else {
            registered.creation_time = Utc::now();
            self.engine.create_node(&registered).await?
        };
        {
            let mut nodes = lock_ptr!(self.nodes)?;
            if let Some(existing) = nodes.get(&registered.name) {
                registered.creation_time = lock_ptr!(existing)?.creation_time;
            }
            nodes.insert(registered.name.clone(), stdng::new_ptr(registered));
        }

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

    /// Remove an inactive node only after all of its executors have been removed.
    pub async fn release_node(&self, node_name: &str) -> Result<(), FlameError> {
        ensure!(
            !node_name.trim().is_empty(),
            FlameError::InvalidConfig("node name is required".into())
        )?;
        let node = self
            .get_node(node_name)?
            .ok_or_else(|| FlameError::NotFound(format!("node <{node_name}> not found")))?;
        ensure!(
            node.state != NodeState::Ready,
            FlameError::InvalidState(format!("Cannot delete Ready node <{node_name}>"))
        )?;
        ensure!(
            self.list_executors(Some(&ExecutorFilter::by_node(node_name)))
                .await?
                .is_empty(),
            FlameError::InvalidState(format!("Cannot delete node <{node_name}> with executors"))
        )?;
        self.engine.delete_node(node_name).await?;
        {
            let mut nodes = lock_ptr!(self.nodes)?;
            nodes.remove(node_name);
        }
        tracing::info!("Deleted stale node <{node_name}>");
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
                let active = self.get_session_ptr(ssn_id.clone()).and_then(|session| {
                    let ssn = lock_ptr!(session)?;
                    ssn.active_task(task_id)
                });
                if let Ok(Some(_)) = active {
                    match self.engine.retry_task(gid.clone()).await {
                        Ok(task) => {
                            // Update the in-memory session with the retried task
                            if let Ok(ssn_ptr) = self.get_session_ptr(ssn_id.clone()) {
                                if let Ok(mut ssn) = lock_ptr!(ssn_ptr) {
                                    if ssn.tasks.contains_key(&task.id) {
                                        let _ = ssn.update_task(&task);
                                    }
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
        let _admission = self.session_admission.lock().await;
        {
            let directory = lock_ptr!(self.sessions)?;
            ensure!(
                !directory.contains_key(&attr.id),
                FlameError::AlreadyExist(format!("session <{}>", attr.id))
            )?;
        }
        self.admit_open_session()?;
        let session = self.engine.create_session(attr).await?;
        event_result(
            self.event_manager.setup_events(session.id.clone()),
            "create",
            &session.id,
        );
        lock_ptr!(self.sessions)?.insert(session.id.clone(), stdng::new_ptr(session.clone()));
        Ok(session)
    }

    /// Close a session in any non-closed state.
    pub async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let _admission = self.session_admission.lock().await;
        let shared = {
            let directory = lock_ptr!(self.sessions)?;
            directory
                .get(&id)
                .cloned()
                .ok_or_else(|| FlameError::NotFound(format!("session <{id}>")))?
        };
        {
            let session = lock_ptr!(shared)?;
            if session.is_closed() {
                return Ok(session.clone());
            }
        }
        let persisted = match self.engine.close_session(id.clone()).await {
            Ok(session) => session,
            Err(error) => {
                let mut committed = self.engine.get_session(id.clone()).await?;
                if committed.is_closed() {
                    committed.task_counts = Default::default();
                    for task in self
                        .engine
                        .find_task_metadata(&TaskFilter::by_session(&id))
                        .await?
                    {
                        committed.task_counts.increment(task.state);
                    }
                    self.publish_closed_session(&id, &shared, &committed)?;
                } else {
                    self.reconcile_session_tasks(&shared).await?;
                }
                return Err(error);
            }
        };
        self.publish_closed_session(&id, &shared, &persisted)
    }

    /// Publish committed closure on the existing cached session.
    fn publish_closed_session(
        &self,
        id: &str,
        shared: &SessionPtr,
        persisted: &Session,
    ) -> Result<Session, FlameError> {
        let mut session = lock_ptr!(shared)?;
        session.status.state = SessionState::Closed;
        session.version = persisted.version;
        session.completion_time = persisted.completion_time;
        session.task_counts = persisted.task_counts;
        session.tasks.clear();
        session.tasks_index.clear();
        session.events.clear();
        event_result(
            self.event_manager.remove_events(id.to_string()),
            "remove",
            id,
        );
        Ok(session.clone())
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
        ensure!(
            state != SessionState::Closed,
            FlameError::InvalidState("use close_session to close a session".into())
        )?;
        let _admission = self.session_admission.lock().await;

        let ssn_ptr = self.require_live_session(&id)?;

        let was_open = lock_ptr!(ssn_ptr)?.is_open();
        if state == SessionState::Open && !was_open {
            self.admit_open_session()?;
        }
        // Pause launches immediately when leaving Open, but never admit work
        // into a resumed session before its underlying state commits.
        let previous = {
            let mut session = lock_ptr!(ssn_ptr)?;
            ensure!(
                !session.is_closed(),
                FlameError::InvalidState(format!("Session <{id}> is closed"))
            )?;
            let previous = session.status.state;
            if state != SessionState::Open {
                session.status.state = state;
            }
            previous
        };
        match self.engine.update_session_state(id.clone(), state).await {
            Ok(committed) => {
                let mut session = lock_ptr!(ssn_ptr)?;
                session.status.state = committed.status.state;
                session.version = committed.version;
            }
            Err(error) => {
                let committed = self.engine.get_session(id).await;
                let mut session = lock_ptr!(ssn_ptr)?;
                match committed {
                    Ok(committed) => {
                        session.status.state = committed.status.state;
                        session.version = committed.version;
                    }
                    Err(_) => session.status.state = previous,
                }
                return Err(error);
            }
        }
        let session = lock_ptr!(ssn_ptr)?;
        Ok(session.clone())
    }

    pub fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let shared = {
            let directory = lock_ptr!(self.sessions)?;
            directory
                .get(&id)
                .cloned()
                .ok_or_else(|| FlameError::NotFound(format!("Session <{id}> not found")))?
        };
        let mut session = lock_ptr!(shared)?.clone();
        session.events = event_result(
            self.event_manager
                .find_events(EventOwner::session(id.clone())),
            "load",
            &id,
        );
        Ok(session)
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
        let _admission = self.session_admission.lock().await;
        let cached = {
            let directory = lock_ptr!(self.sessions)?;
            directory.get(&id).cloned()
        };
        if let Some(shared) = cached {
            let session = lock_ptr!(shared)?;
            return self.setup_session(&session, spec.as_ref());
        }
        self.admit_open_session()?;
        let session = self.engine.open_session(id, spec.clone()).await?;
        self.cache_session(session, spec.as_ref())
    }

    /// Publishes an engine snapshot without replacing an existing session pointer.
    fn cache_session(
        &self,
        ssn: Session,
        spec: Option<&SessionAttributes>,
    ) -> Result<Session, FlameError> {
        let ssn_ptr = {
            let fresh = SessionPtr::new(ssn.into());
            // Lock the unpublished pointer first: readers cannot observe it
            // before event setup, and no existing map entry can wait on it yet.
            let ssn = lock_ptr!(fresh)?;
            let published = {
                let mut ssn_map = lock_ptr!(self.sessions)?;
                ssn_map
                    .entry(ssn.id.clone())
                    .or_insert_with(|| fresh.clone())
                    .clone()
            };
            if Arc::ptr_eq(&fresh, &published) {
                return self.setup_session(&ssn, spec);
            }
            published
        };
        // A competing opener owns this pointer. Release the unpublished
        // candidate's guard before acquiring the existing session's lock.
        let ssn = lock_ptr!(ssn_ptr)?;
        self.setup_session(&ssn, spec)
    }

    /// Validates a locked cached session and initializes events, serializing
    /// setup with close without holding the global session map.
    fn setup_session(
        &self,
        ssn: &Session,
        spec: Option<&SessionAttributes>,
    ) -> Result<Session, FlameError> {
        if ssn.is_suspending() || ssn.is_suspended() {
            return Err(FlameError::InvalidState(format!(
                "Session <{}> is suspended",
                ssn.id
            )));
        }
        ensure!(
            ssn.is_open(),
            FlameError::InvalidState(format!("Session <{}> is not open", ssn.id))
        )?;
        if let Some(attr) = spec {
            ssn.validate_spec(attr)?;
        }
        event_result(
            self.event_manager.setup_events(ssn.id.clone()),
            "create",
            &ssn.id,
        );
        Ok(ssn.clone())
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

    pub async fn delete_session(&self, shared: &SessionPtr) -> Result<Session, FlameError> {
        let _admission = self.session_admission.lock().await;
        let previous = self.session_for_removal(shared)?;
        self.event_manager.remove_events(previous.id.clone())?;
        let removed = match self.engine.delete_session(previous.id.clone()).await {
            Ok(session) => session,
            Err(FlameError::NotFound(_)) => previous,
            Err(error) => return Err(error),
        };
        self.remove_cached_session(shared, &removed.id)?;
        Ok(removed.metadata())
    }

    pub async fn stale_session(&self, shared: &SessionPtr) -> Result<(), FlameError> {
        let _admission = self.session_admission.lock().await;
        let previous = self.session_for_removal(shared)?;
        self.event_manager.remove_events(previous.id.clone())?;
        match self.engine.stale_session(previous.id.clone()).await {
            Ok(()) | Err(FlameError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        self.remove_cached_session(shared, &previous.id)
    }

    /// Call while holding session_admission, which excludes removal and same-ID creation.
    fn session_for_removal(&self, shared: &SessionPtr) -> Result<Session, FlameError> {
        let session = { lock_ptr!(shared)?.metadata() };
        let current = self.get_session_ptr(session.id.clone())?;
        ensure!(
            Arc::ptr_eq(&current, shared),
            FlameError::NotFound(format!("Session <{}> was removed or replaced", session.id))
        )?;
        ensure!(
            session.is_closed(),
            FlameError::InvalidState(format!("session <{}> is not closed", session.id))
        )?;
        Ok(session)
    }

    fn remove_cached_session(&self, shared: &SessionPtr, id: &SessionID) -> Result<(), FlameError> {
        {
            let mut session = lock_ptr!(shared)?;
            session.tasks.clear();
            session.tasks_index.clear();
            session.tokens.clear();
            session.common_data = None;
            session.events.clear();
        }
        let mut sessions = lock_ptr!(self.sessions)?;
        if sessions
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, shared))
        {
            sessions.remove(id);
        }
        Ok(())
    }

    /// Project metadata from all cached sessions without copying task maps or payloads.
    pub async fn list_sessions(
        &self,
        filter: Option<&SessionFilter>,
    ) -> Result<Page<Session>, FlameError> {
        let shared = {
            let sessions = lock_ptr!(self.sessions)?;
            selected_pointers(&sessions, filter.and_then(|filter| filter.ids.as_deref()))
        };
        let mut sessions = Vec::with_capacity(shared.len());
        for session in shared {
            sessions.push(lock_ptr!(session)?.metadata());
        }
        let retry_limits = self.session_retry_limits();
        sessions.retain(|session| {
            filter.is_none_or(|filter| {
                filter
                    .application
                    .as_ref()
                    .is_none_or(|app| app == &session.application)
                    && filter
                        .state
                        .is_none_or(|state| state == session.status.state)
                    && filter.predicate.is_none_or(|predicate| {
                        predicate.matches(session.retry_count, retry_limits)
                    })
            })
        });
        sessions.sort_by(|left, right| {
            right
                .is_open()
                .cmp(&left.is_open())
                .then_with(|| right.creation_time.cmp(&left.creation_time))
                .then_with(|| left.id.cmp(&right.id))
        });
        paginate(sessions, filter.and_then(|filter| filter.paging))
    }

    /// List complete in-memory executors for lifecycle callers and explicit pages.
    ///
    /// # Arguments
    /// * `filter` - Filter criteria (state, node, and/or executor IDs).
    ///   - `None` filter means return all executors
    ///   - For each field in filter:
    ///     - `None` = ignore this field (match all)
    ///     - `Some(value)` = match exactly (empty vec/string matches nothing)
    ///   - All specified filters use AND logic.
    pub async fn list_executors(
        &self,
        filter: Option<&ExecutorFilter>,
    ) -> Result<Page<Executor>, FlameError> {
        let shared = {
            let entries = lock_ptr!(self.executors)?;
            selected_pointers(&entries, filter.and_then(|filter| filter.ids.as_deref()))
        };
        let mut executors = Vec::with_capacity(shared.len());
        for shared in shared {
            executors.push(lock_ptr!(shared)?.clone());
        }
        executors.retain(|executor| {
            filter.is_none_or(|filter| {
                filter.state.is_none_or(|state| state == executor.state)
                    && filter
                        .node
                        .as_ref()
                        .is_none_or(|node| node == &executor.node)
                    && filter
                        .application
                        .as_ref()
                        .is_none_or(|app| app == &executor.application)
                    && filter
                        .session_id
                        .as_ref()
                        .is_none_or(|session| Some(session) == executor.ssn_id.as_ref())
            })
        });
        executors.sort_by(|left, right| {
            left.node
                .cmp(&right.node)
                .then_with(|| (left.state as i32).cmp(&(right.state as i32)))
                .then_with(|| left.id.cmp(&right.id))
        });
        paginate(executors, filter.and_then(|filter| filter.paging))
    }

    pub async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        trace_fn!("Storage::create_task");
        let ssn = self.require_live_session(&ssn_id)?;
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

    pub async fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        let mut task = self.get_task_metadata(ssn_id, id).await?;
        task.events = event_result(
            self.event_manager.find_events(EventOwner::from(task.gid())),
            "load",
            &task.ssn_id,
        );
        Ok(task)
    }

    /// Current full task without historical events; terminal reads do not repopulate the cache.
    pub async fn get_task_metadata(
        &self,
        ssn_id: SessionID,
        id: TaskID,
    ) -> Result<Task, FlameError> {
        let gid = TaskGID {
            ssn_id,
            task_id: id,
        };
        let mut task = match self.get_task_ptr(gid.clone()) {
            Ok(ptr) => lock_ptr!(ptr)?.clone(),
            Err(FlameError::NotFound(_)) => self.engine.get_task(gid).await?,
            Err(error) => return Err(error),
        };
        task.events.clear();
        Ok(task)
    }

    /// List authoritative task headers, ordered independently of task state.
    pub async fn list_tasks(&self, filter: &TaskFilter) -> Result<Page<Task>, FlameError> {
        self.engine.find_task_metadata(filter).await
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
    ) -> Result<Page<Application>, FlameError> {
        let shared = {
            let entries = lock_ptr!(self.applications)?;
            selected_pointers(&entries, filter.and_then(|filter| filter.names.as_deref()))
        };
        let mut applications = Vec::with_capacity(shared.len());
        for shared in shared {
            let application = lock_ptr!(shared)?;
            if filter
                .is_some_and(|filter| filter.state.is_some_and(|state| state != application.state))
            {
                continue;
            }
            applications.push(Application {
                name: application.name.clone(),
                version: application.version,
                state: application.state,
                creation_time: application.creation_time,
                shim: application.shim,
                image: application.image.clone(),
                description: application.description.clone(),
                labels: application.labels.clone(),
                command: application.command.clone(),
                arguments: application.arguments.clone(),
                working_directory: application.working_directory.clone(),
                max_instances: application.max_instances,
                delay_release: application.delay_release,
                schema: application.schema.clone(),
                url: application.url.clone(),
                installer: application.installer.clone(),
                environments: HashMap::new(),
            });
        }
        applications.sort_by(|left, right| {
            (right.state == ApplicationState::Enabled)
                .cmp(&(left.state == ApplicationState::Enabled))
                .then_with(|| right.creation_time.cmp(&left.creation_time))
                .then_with(|| left.name.cmp(&right.name))
        });
        paginate(applications, filter.and_then(|filter| filter.paging))
    }

    pub async fn session_application(&self, id: SessionID) -> Result<ApplicationID, FlameError> {
        Ok(self.get_session(id)?.application)
    }

    /// Refresh cached task states after a mutation partially committed before returning an error.
    async fn reconcile_session_tasks(&self, shared: &SessionPtr) -> Result<(), FlameError> {
        let id = lock_ptr!(shared)?.id.clone();
        let tasks = self
            .engine
            .find_task_metadata(&TaskFilter::by_session(id))
            .await?;
        let mut session = lock_ptr!(shared)?;
        if session.is_closed() {
            return Ok(());
        }
        for metadata in tasks {
            if let Some(mut current) = session.active_task(metadata.id)? {
                current.state = metadata.state;
                current.version = metadata.version;
                current.completion_time = metadata.completion_time;
                session.update_task(&current)?;
            }
        }
        Ok(())
    }

    pub async fn count_session(&self, filter: &SessionFilter) -> Result<usize, FlameError> {
        Ok(self.list_sessions(Some(filter)).await?.len())
    }

    pub async fn update_task_state(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_state");
        let task_id = lock_ptr!(task)?.id;

        // A completed task has left the active cache and must not be updated.
        let current = {
            let ssn_ptr = lock_ptr!(ssn)?;
            ssn_ptr.active_task(task_id)?
        };
        let Some(current) = current else {
            return Ok(());
        };

        let updated_task = match self
            .engine
            .update_task_state(current, task_state, message)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                self.reconcile_session_tasks(&ssn).await?;
                return Err(error);
            }
        };

        self.record_event(
            EventOwner::from(updated_task.gid()),
            Event {
                code: task_state.into(),
                message: Some(format!("Task state was updated to <{:?}>", task_state)),
                creation_time: Utc::now(),
            },
        )
        .await?;

        {
            let mut ssn_ptr = lock_ptr!(ssn)?;
            // A late engine response must not reinsert an evicted task.
            if ssn_ptr.tasks.contains_key(&updated_task.id) {
                ssn_ptr.update_task(&updated_task)?;
            }
        }

        Ok(())
    }

    pub async fn update_task_result(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("Storage::update_task_result");
        let task_id = lock_ptr!(task)?.id;

        let task_state = task_result.state;
        let task_message = task_result.message.clone();

        // A completed task has left the active cache and must not be updated.
        let current = {
            let ssn_ptr = lock_ptr!(ssn)?;
            ssn_ptr.active_task(task_id)?
        };
        let Some(current) = current else {
            return Ok(());
        };

        let updated_task = match self.engine.update_task_result(current, task_result).await {
            Ok(task) => task,
            Err(error) => {
                self.reconcile_session_tasks(&ssn).await?;
                return Err(error);
            }
        };

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

        {
            let mut ssn_ptr = lock_ptr!(ssn)?;
            // A late engine response must not reinsert an evicted task.
            if ssn_ptr.tasks.contains_key(&updated_task.id) {
                ssn_ptr.update_task(&updated_task)?;
            }
        }

        Ok(())
    }

    pub async fn create_executor(
        &self,
        node_name: String,
        ssn_id: SessionID,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Storage::create_executor");
        ensure!(
            self.get_node(&node_name)?.is_some(),
            FlameError::NotFound(format!("node <{node_name}> not found"))
        )?;
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
mod tests;

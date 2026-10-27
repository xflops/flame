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

pub mod snapshot;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;

use common::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationState, CommonData, Event,
    EventOwner, ExecutorID, ExecutorState, FlameResult, Node, NodeState, Session,
    SessionAttributes, SessionID, SessionPtr, SessionState, Signal, Task, TaskGID, TaskID,
    TaskInput, TaskOptions, TaskPtr, TaskResult, TaskState,
};

use common::FlameError;
use stdng::{ensure, lock_ptr, logs::TraceFn, trace_fn};

use ::rpc::flame::v1 as rpc;

use crate::model::{
    AppInfo, ConnectionCallbacks, ConnectionState, Executor, ExecutorFilter, ExecutorInfo,
    ExecutorPtr, NodeConnectionPtr, NodeConnectionReceiver, NodeConnectionSender, NodeInfo,
    NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot, SnapShotPtr,
};
use crate::notify::{NotifyManager, NotifyManagerPtr, TaskSubscription};
use crate::storage::StoragePtr;

mod connections;
mod executors;
mod nodes;

pub use connections::ConnectionManager;

struct NodeCallbacks {
    storage: StoragePtr,
}

#[async_trait::async_trait]
impl ConnectionCallbacks for NodeCallbacks {
    async fn on_connected(&self, node_name: &str) -> Result<(), FlameError> {
        trace_fn!("NodeCallbacks::on_connected");

        // Transition node to Ready state via state machine.
        // Note: This is called for both new connections and reconnections from Draining.
        // The node should already be registered in storage by controller.register_node().
        if let Ok(node_ptr) = self.storage.get_node_ptr(node_name) {
            let state = nodes::from(self.storage.clone(), node_ptr)?;
            state.register_node().await?;
        }

        tracing::info!("Node <{}> connected", node_name);
        Ok(())
    }

    async fn on_draining(&self, node_name: &str) -> Result<(), FlameError> {
        trace_fn!("NodeCallbacks::on_draining");

        // Transition node to Unknown (draining) state via state machine
        if let Ok(node_ptr) = self.storage.get_node_ptr(node_name) {
            let state = nodes::from(self.storage.clone(), node_ptr)?;
            state.drain().await?;
        }

        tracing::info!("Node <{}> draining", node_name);
        Ok(())
    }

    async fn on_closed(&self, node_name: &str) -> Result<(), FlameError> {
        trace_fn!("NodeCallbacks::on_closed");

        // Shutdown node via state machine (Unknown -> NotReady)
        // The shutdown() method handles executor cleanup
        if let Ok(node_ptr) = self.storage.get_node_ptr(node_name) {
            let state = nodes::from(self.storage.clone(), node_ptr)?;
            state.shutdown().await?;
        }

        tracing::info!("Node <{}> connection closed", node_name);
        Ok(())
    }
}

/// A validated executor attribute update: keys to add and keys to remove.
#[derive(Default)]
struct ExecutorAttributeUpdate {
    publish: HashSet<bytes::Bytes>,
    retract: HashSet<bytes::Bytes>,
}

pub struct Controller {
    storage: StoragePtr,
    connection_manager: ConnectionManager<NodeCallbacks>,
    notifier: NotifyManagerPtr,
}

pub type ControllerPtr = Arc<Controller>;

pub fn new_ptr(storage: StoragePtr) -> ControllerPtr {
    let callbacks = NodeCallbacks {
        storage: storage.clone(),
    };
    Arc::new(Controller {
        storage,
        connection_manager: ConnectionManager::new(callbacks),
        notifier: NotifyManager::new_ptr(),
    })
}

impl Controller {
    const MAX_EXECUTOR_ATTRIBUTES: usize = 1_024;
    const MAX_EXECUTOR_ATTRIBUTE_BYTES: usize = 256;
    const MAX_EXECUTOR_ATTRIBUTES_BYTES: usize = 64 * 1_024;

    // ========================================================================
    // Accessors
    // ========================================================================

    /// Returns a reference to the storage.
    pub fn storage(&self) -> &StoragePtr {
        &self.storage
    }

    pub(crate) async fn wait_for_scheduler_event(&self, interval: tokio::time::Duration) {
        self.notifier.scheduler.wait(interval).await;
    }

    fn notify_scheduler(&self) {
        self.notifier.scheduler.notify();
    }

    fn validate_attribute_keys(keys: Vec<Vec<u8>>) -> Result<HashSet<bytes::Bytes>, FlameError> {
        let mut set = HashSet::with_capacity(keys.len());
        let mut total_bytes = 0usize;
        for value in keys {
            let value_len = value.len();
            if value.is_empty() || value_len > Self::MAX_EXECUTOR_ATTRIBUTE_BYTES {
                return Err(FlameError::InvalidConfig(
                    "executor attributes must be nonempty and at most 256 bytes".to_string(),
                ));
            }
            if set.insert(bytes::Bytes::from(value)) {
                total_bytes += value_len;
            }
        }
        if set.len() > Self::MAX_EXECUTOR_ATTRIBUTES
            || total_bytes > Self::MAX_EXECUTOR_ATTRIBUTES_BYTES
        {
            return Err(FlameError::InvalidConfig(
                "executor attribute update exceeds configured limits".to_string(),
            ));
        }
        Ok(set)
    }

    /// Validates the whole update before any of it is applied.
    fn validate_executor_attributes(
        attributes: rpc::ExecutorAttributes,
    ) -> Result<ExecutorAttributeUpdate, FlameError> {
        let publish = Self::validate_attribute_keys(attributes.publish)?;
        let retract = Self::validate_attribute_keys(attributes.retract)?;
        if !publish.is_disjoint(&retract) {
            return Err(FlameError::InvalidConfig(
                "executor attributes must not be published and retracted in one update".to_string(),
            ));
        }
        Ok(ExecutorAttributeUpdate { publish, retract })
    }

    fn update_executor_attributes(
        &self,
        id: &ExecutorID,
        update: ExecutorAttributeUpdate,
    ) -> Result<(), FlameError> {
        let executor = self.storage.get_executor_ptr(id.clone())?;
        let mut executor = lock_ptr!(executor)?;
        if executor.state != ExecutorState::Bound {
            return Err(FlameError::InvalidState(format!(
                "executor <{id}> must be bound before publishing attributes"
            )));
        }
        executor.ssn_id.as_ref().ok_or_else(|| {
            FlameError::InvalidState(format!(
                "executor <{id}> must have a session before publishing attributes"
            ))
        })?;
        executor
            .attributes
            .retain(|key| !update.retract.contains(key));
        executor.attributes.extend(update.publish);
        Ok(())
    }

    fn validate_task_options(options: &TaskOptions) -> Result<(), FlameError> {
        let total_bytes: usize = options.affinity.iter().map(|key| key.len()).sum();
        if options
            .affinity
            .iter()
            .any(|key| key.is_empty() || key.len() > Self::MAX_EXECUTOR_ATTRIBUTE_BYTES)
        {
            return Err(FlameError::InvalidConfig(
                "task affinity keys must be nonempty and at most 256 bytes".to_string(),
            ));
        }
        if options.affinity.len() > Self::MAX_EXECUTOR_ATTRIBUTES
            || total_bytes > Self::MAX_EXECUTOR_ATTRIBUTES_BYTES
        {
            return Err(FlameError::InvalidConfig(
                "task affinity exceeds configured limits".to_string(),
            ));
        }
        Ok(())
    }

    // ========================================================================
    // Node Management
    // ========================================================================

    /// Registers a node and aligns executor state.
    ///
    /// This is the main entry point for node registration. It:
    /// 1. Registers/updates the node in storage (preserving existing state)
    /// 2. Connects the node (creates in Connected state, calls on_connected)
    /// 3. Compares reported executors with DB executors
    /// 4. Releases orphaned executors (in DB but not reported)
    /// 5. Sends all executors to the node for initial sync
    pub async fn register_node(
        &self,
        node: &Node,
        reported_executors: &[Executor],
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::register_node");

        // Check if node exists to preserve its state
        let existing_state = self.storage.get_node(&node.name)?.map(|n| n.state);

        // Get executors from DB for this node
        let db_executors = self
            .storage
            .list_executors(Some(&ExecutorFilter::by_node(&node.name)))?;

        // The node may only report executors this Session Manager knows. An unknown
        // one is capacity nothing counts (a new node, or executors deleted while the
        // node was away), so the site admin clears the node (for example by
        // restarting it) before it joins. Nothing is stored for a rejected node.
        let unknown = reported_executors
            .iter()
            .filter(|reported| !db_executors.iter().any(|db_exec| db_exec.id == reported.id))
            .count();
        if unknown > 0 {
            return Err(FlameError::InvalidState(format!(
                "node <{}> reports {} executors unknown to Session Manager; clear them before it joins",
                node.name, unknown
            )));
        }

        // Create node with preserved state (or use input state for new nodes)
        let node_to_store = if let Some(state) = existing_state {
            Node {
                state,
                ..node.clone()
            }
        } else {
            node.clone()
        };

        // Store/update node info (with preserved state)
        self.storage.register_node(&node_to_store).await?;

        // Connect node and get sender (creates connection if not exists, calls on_connected)
        let (sender, _receiver) = self.connection_manager.connect(&node.name).await?;

        // Build sets for comparison
        let reported_ids: HashSet<&str> = reported_executors
            .iter()
            .map(|executor| executor.id.as_str())
            .collect();

        // DB executors not reported by the node have lost their in-memory
        // shim. Retain the record and ask the executor-manager run loop to
        // perform targeted cleanup by persisted executor ID. The interrupted
        // task is failed only after cleanup is acknowledged by unregister_executor.
        for db_exec in &db_executors {
            if !reported_ids.contains(db_exec.id.as_str()) {
                tracing::info!(
                    "Executor <{}> in DB but not reported by node <{}>. Releasing orphaned executor.",
                    db_exec.id,
                    node.name
                );
                let mut cleanup_executor = db_exec.clone();
                cleanup_executor.state = ExecutorState::Releasing;
                self.storage.update_executor(&cleanup_executor).await?;
                sender.send(cleanup_executor).await.map_err(|error| {
                    FlameError::Network(format!(
                        "failed to send cleanup for stale executor <{}>: {error}",
                        db_exec.id
                    ))
                })?;
                continue;
            }

            // Only replay executors whose runtime was actually reported.
            if let Err(e) = sender.send(db_exec.clone()).await {
                tracing::warn!("Failed to send executor <{}> to node: {:?}", db_exec.id, e);
            }
        }

        Ok(())
    }

    /// Gets the sender and receiver handles for a node's connection.
    ///
    /// Used by watch_node to receive executor updates.
    /// The node must have been registered via register_node first.
    pub fn get_node_channel(
        &self,
        node_name: &str,
    ) -> Result<Option<(NodeConnectionSender, NodeConnectionReceiver)>, FlameError> {
        trace_fn!("Controller::get_node_channel");
        self.connection_manager.get_channel(node_name)
    }

    /// Drains a node when its watch stream disconnects.
    ///
    /// This handles the full drain lifecycle:
    /// 1. Drains via ConnectionManager (triggers on_draining callback)
    /// 2. Starts drain timer
    /// 3. When timer expires, on_closed callback shuts down the node
    pub async fn drain_node(&self, node_name: &str) -> Result<(), FlameError> {
        trace_fn!("Controller::drain_node");

        self.connection_manager.drain(node_name).await?;
        Ok(())
    }

    /// Gets a node by name. Returns None if the node doesn't exist.
    pub fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        self.storage.get_node(name)
    }

    /// Lists all registered nodes.
    pub fn list_nodes(&self) -> Result<Vec<Node>, FlameError> {
        self.storage.list_nodes()
    }

    /// Updates node status from a heartbeat.
    ///
    /// This is a lightweight update that only modifies node status fields
    /// (capacity, allocatable, info) without affecting connection state.
    /// Used for periodic heartbeat updates from connected nodes.
    ///
    /// If the node doesn't exist or is not in Ready state, the update is
    /// silently ignored (heartbeats are best-effort).
    pub async fn update_node(&self, node: &Node) -> Result<(), FlameError> {
        trace_fn!("Controller::update_node");

        // Only update if node exists
        if let Ok(node_ptr) = self.storage.get_node_ptr(&node.name) {
            let state = nodes::from(self.storage.clone(), node_ptr)?;
            // Ignore errors from state machine (e.g., node not in Ready state)
            // Heartbeats are best-effort and shouldn't fail the connection
            let _ = state.update_node(node).await;
        }
        // If node doesn't exist, ignore the heartbeat silently
        // (node will be registered via WatchNode registration message)

        Ok(())
    }

    /// Syncs node state and returns executors for the node.
    ///
    /// # Deprecated
    /// Use `WatchNode` streaming RPC instead for better efficiency.
    /// `sync_node` uses polling which is less efficient than server-push.
    #[deprecated(since = "0.6.0", note = "Use WatchNode streaming RPC instead")]
    #[allow(deprecated)]
    pub async fn sync_node(
        &self,
        node: &Node,
        executors: &Vec<Executor>,
    ) -> Result<Vec<Executor>, FlameError> {
        trace_fn!("Controller::sync_node");

        // If node exists, update it via state machine
        if let Ok(node_ptr) = self.storage.get_node_ptr(&node.name) {
            let state = nodes::from(self.storage.clone(), node_ptr)?;
            state.update_node(node).await?;
        }

        self.storage.sync_node(node, executors).await
    }

    /// Releases a node and removes it from storage.
    pub async fn release_node(&self, node_name: &str) -> Result<(), FlameError> {
        trace_fn!("Controller::release_node");

        // If node exists, use state machine to validate release
        if let Ok(node_ptr) = self.storage.get_node_ptr(node_name) {
            let state = nodes::from(self.storage.clone(), node_ptr)?;
            state.release_node().await?;
        }

        self.storage.release_node(node_name).await
    }

    pub async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        trace_fn!("Controller::create_session");
        self.application_enabled(&attr.application).await?;
        self.storage.create_session(attr).await
    }

    pub async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        trace_fn!("Controller::open_session");
        let application = match spec.as_ref() {
            Some(attr) => attr.application.clone(),
            None => self.storage.session_application(id.clone()).await?,
        };
        self.application_enabled(&application).await?;
        self.storage.open_session(id, spec).await
    }

    async fn application_enabled(&self, name: &str) -> Result<(), FlameError> {
        let application = self.storage.get_application(name.to_string()).await?;
        if application.state != ApplicationState::Enabled {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> is disabled"
            )));
        }
        Ok(())
    }

    pub async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        trace_fn!("Controller::close_session");
        let session = self.storage.close_session(id.clone()).await?;
        // Dropping the session's broadcast sender lets subscribers drain queued
        // task updates, then receive RecvError::Closed.
        self.notifier.tasks.remove(&id)?;
        Ok(session)
    }

    /// Signals a session. `Signal::Suspend` stops launching tasks for it: running tasks finish and its
    /// executors unbind like those of a closed session (a binding one finishes or fails its bind
    /// first). The signal only starts the suspend; the session stays `Suspending` until the
    /// scheduler finds no executor serving it ([`Controller::suspend_session`]). There is no
    /// deadline: a suspend that cannot finish stays `Suspending`.
    pub async fn signal_session(
        &self,
        id: SessionID,
        signal: Signal,
    ) -> Result<Session, FlameError> {
        trace_fn!("Controller::signal_session");

        ensure!(
            signal == Signal::Suspend,
            FlameError::InvalidConfig(format!(
                "Session <{id}> cannot be signalled with {signal:?}"
            ))
        )?;
        let current = self.storage.get_session(id.clone())?;
        ensure!(
            current.is_open(),
            FlameError::InvalidState(format!("Session <{id}> is not open"))
        )?;
        let session = self
            .storage
            .update_session_state(id.clone(), SessionState::Suspending)
            .await?;

        // Wake the executors waiting for tasks so they notice the session left `Open`.
        self.notifier.tasks.remove(&id)?;
        Ok(session)
    }

    /// Finishes the suspend of `id`. The caller (the scheduler's shuffle action) has already verified
    /// that no executor serves the session, so this does not look at the executors. It only moves a
    /// `Suspending` session: one resumed or closed since that check is left alone.
    pub async fn suspend_session(&self, id: SessionID) -> Result<(), FlameError> {
        trace_fn!("Controller::suspend_session");

        let current = self.storage.get_session(id.clone())?;
        if !current.is_suspending() {
            return Ok(());
        }

        match self
            .storage
            .update_session_state(id.clone(), SessionState::Suspended)
            .await
        {
            Ok(_) => {
                tracing::info!("Session <{}> is suspended", id);
                Ok(())
            }
            // Closed by someone else in the meantime.
            Err(FlameError::InvalidState(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Reopens a suspended session; the scheduler assigns executors again.
    pub async fn resume_session(&self, id: SessionID) -> Result<Session, FlameError> {
        trace_fn!("Controller::resume_session");
        let current = self.storage.get_session(id.clone())?;
        let session = match current.status.state {
            // Already open: nothing to resume.
            SessionState::Open => return Ok(current),
            _ => {
                ensure!(
                    current.is_suspended(),
                    FlameError::InvalidState(format!("Session <{id}> is not suspended"))
                )?;
                self.storage
                    .update_session_state(id, SessionState::Open)
                    .await?
            }
        };
        self.notify_scheduler();
        Ok(session)
    }

    pub fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        self.storage.get_session(id)
    }

    pub fn is_session_open(&self, id: &SessionID) -> Result<bool, FlameError> {
        let session = self.storage.get_session_ptr(id.clone())?;
        let is_open = !lock_ptr!(session)?.is_closed();
        Ok(is_open)
    }

    pub async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let session = self.storage.delete_session(id.clone()).await?;
        self.notifier.tasks.remove(&id)?;
        Ok(session)
    }

    pub fn list_sessions(
        &self,
        filter: Option<&crate::model::SessionFilter>,
    ) -> Result<Vec<Session>, FlameError> {
        self.storage.list_sessions(filter)
    }

    pub async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        if let Some(options) = options.as_ref() {
            Self::validate_task_options(options)?;
        }
        let task = self
            .storage
            .create_task(ssn_id.clone(), task_input, options)
            .await?;
        let _ = self.notifier.tasks.notify(&ssn_id, task.id);
        self.notify_scheduler();
        Ok(task)
    }

    pub fn subscribe(&self, ssn_id: &SessionID) -> Result<TaskSubscription, FlameError> {
        let subscription = self.notifier.tasks.subscribe(ssn_id)?;
        // Subscribe first: a concurrent close then either fails this check or
        // closes the channel observed by the subscriber.
        if !self.is_session_open(ssn_id)? {
            return Err(FlameError::NotFound("session is closed".to_string()));
        }
        Ok(subscription)
    }

    pub fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        self.storage.get_task(ssn_id, id)
    }

    pub fn get_task_metadata(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        self.storage.get_task_metadata(ssn_id, id)
    }

    pub fn list_tasks(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        self.storage.list_tasks(ssn_id)
    }

    pub async fn create_executor(
        &self,
        node_name: String,
        ssn_id: SessionID,
    ) -> Result<Executor, FlameError> {
        trace_fn!("Controller::create_executor");
        let executor = self
            .storage
            .create_executor(node_name.clone(), ssn_id)
            .await?;

        // Notify the node about the new executor
        if let Err(e) = self
            .connection_manager
            .notify_executor(&node_name, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about executor creation: {}",
                node_name,
                e
            );
        }

        Ok(executor)
    }

    pub fn get_executor(&self, id: ExecutorID) -> Result<Executor, FlameError> {
        trace_fn!("Controller::get_executor");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let exe = lock_ptr!(exe_ptr)?;
        Ok((*exe).clone())
    }

    pub fn list_executors(&self) -> Result<Vec<Executor>, FlameError> {
        trace_fn!("Controller::list_executors");
        self.storage.list_executors(None)
    }

    /// Prepare lifecycle changes privately; storage publishes them after saving.
    fn clone_executor_ptr(&self, id: ExecutorID) -> Result<ExecutorPtr, FlameError> {
        let executor = self.storage.get_executor_ptr(id)?;
        let snapshot = lock_ptr!(executor)?.clone();
        Ok(stdng::new_ptr(snapshot))
    }

    pub async fn register_executor(&self, e: &Executor) -> Result<(), FlameError> {
        trace_fn!("Controller::register_executor");

        let exe_ptr = self.clone_executor_ptr(e.id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.register_executor().await?;
        let executor = lock_ptr!(exe_ptr)?.clone();
        self.storage.update_executor(&executor).await?;

        // Notify the node about the executor registration
        if let Err(err) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about executor registration: {}",
                e.node,
                err
            );
        }

        self.notify_scheduler();
        Ok(())
    }

    pub fn snapshot(&self) -> Result<SnapShotPtr, FlameError> {
        let data = self.storage.snapshot()?;
        let snapshot = Arc::new(SnapShot::new_with_session_retry_limits(
            self.storage.session_retry_limits(),
        ));

        for application in &data.applications {
            snapshot.add_application(Arc::new(AppInfo::from(application)))?;
        }
        for session in &data.sessions {
            snapshot.add_session(Arc::new(SessionInfo::try_from(session)?))?;
        }
        for executor in &data.executors {
            snapshot.add_executor(Arc::new(ExecutorInfo::from(executor)))?;
        }
        for node in &data.nodes {
            snapshot.add_node(Arc::new(NodeInfo::from(node)))?;
        }

        Ok(snapshot)
    }

    pub async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        self.storage.get_application(id).await
    }

    pub async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::register_application");
        common::apis::validate_application_name(&name)?;
        self.storage.register_application(name, attr).await
    }

    pub async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        trace_fn!("Controller::unregister_application");
        self.storage
            .update_application_state(name, ApplicationState::Disabled)
            .await?;
        Ok(())
    }

    pub async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::update_application");
        common::apis::validate_application_name(&name)?;
        self.storage.update_application(name, attr).await
    }

    pub async fn list_applications(
        &self,
        filter: Option<&crate::model::ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        trace_fn!("Controller::list_applications");
        self.storage.list_applications(filter).await
    }

    pub async fn wait_for_session(&self, id: ExecutorID) -> Result<Option<Session>, FlameError> {
        trace_fn!("Controller::wait_for_session");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let mut rx = self.notifier.executors.subscribe(&id)?;

        let ssn_id = loop {
            {
                let exe = lock_ptr!(exe_ptr)?;
                if exe.state == ExecutorState::Releasing || exe.state == ExecutorState::Released {
                    return Ok(None);
                }
                if let Some(ssn_id) = exe.ssn_id.clone() {
                    break ssn_id;
                }
            }

            // A lagged wakeup still requires reconciling the current executor state.
            if matches!(rx.recv().await, Err(RecvError::Closed)) {
                return Ok(None);
            }
        };

        let ssn_ptr = match self.storage.get_session_ptr(ssn_id.clone()) {
            Ok(ssn_ptr) => ssn_ptr,
            Err(FlameError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };

        let ssn = lock_ptr!(ssn_ptr)?;
        if !ssn.is_open() {
            return Ok(None);
        }

        Ok(Some((*ssn).clone()))
    }

    pub async fn bind_session(&self, id: ExecutorID, ssn_id: SessionID) -> Result<(), FlameError> {
        trace_fn!("Controller::bind_session");

        let exe_ptr = self.clone_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;

        let ssn_ptr = self.storage.get_session_ptr(ssn_id)?;
        let open = {
            let ssn = lock_ptr!(ssn_ptr)?;
            ssn.is_open()
        };
        if !open {
            return Err(FlameError::InvalidState(
                "session is not open for binding".to_string(),
            ));
        }
        state.bind_session(ssn_ptr).await?;

        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };
        self.storage.update_executor(&executor).await?;
        self.notifier.executors.notify(&id, id.clone())?;

        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about executor binding: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn bind_session_completed(
        &self,
        id: ExecutorID,
        result: Option<FlameResult>,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::bind_session_completed");

        let exe_ptr = self.clone_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;

        state.bind_session_completed(result).await?;

        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };
        self.storage.update_executor(&executor).await?;

        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about bind completion: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn bind_executor_completed(
        &self,
        id: ExecutorID,
        result: Option<FlameResult>,
        attributes: Option<rpc::ExecutorAttributes>,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::bind_executor_completed");
        let attributes = attributes
            .map(Self::validate_executor_attributes)
            .transpose()?;
        self.bind_session_completed(id.clone(), result).await?;
        if let Some(attributes) = attributes {
            self.update_executor_attributes(&id, attributes)?;
        }
        Ok(())
    }

    pub async fn launch_task(&self, id: ExecutorID) -> Result<Option<Task>, FlameError> {
        trace_fn!("Controller::launch_task");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        let (ssn_id, task_id) = {
            let exec = lock_ptr!(exe_ptr)?;
            (exec.ssn_id.clone(), exec.task_id)
        };

        tracing::debug!("Try to launch task for session <{:?}>", ssn_id);
        let Some(ssn_id) = ssn_id else {
            tracing::debug!("No session to launch task for, return.");
            return Ok(None);
        };

        if let Some(task_id) = task_id {
            tracing::warn!("Re-launch the task <{}/{}>", ssn_id, task_id);
            let task_ptr = self.storage.get_task_ptr(TaskGID { ssn_id, task_id })?;
            let task = lock_ptr!(task_ptr)?;
            return Ok(Some((*task).clone()));
        }

        tracing::debug!("Launching task for session <{:?}>", ssn_id);
        let ssn_ptr = match self.storage.get_session_ptr(ssn_id.clone()) {
            Ok(ssn_ptr) => ssn_ptr,
            Err(FlameError::NotFound(msg)) => {
                tracing::warn!(
                    "Session <{:?}> not found when launching task: {}",
                    ssn_id,
                    msg
                );
                return Ok(None);
            }
            Err(e) => {
                tracing::error!(
                    "Failed to get session <{:?}> when launching task: {:?}",
                    ssn_id,
                    e
                );
                return Err(e);
            }
        };

        let task_ptr = self.wait_for_task(&ssn_ptr, &exe_ptr).await?;
        let Some(task_ptr) = task_ptr else {
            return Ok(None);
        };

        let task_id = {
            let task = lock_ptr!(task_ptr)?;
            task.id
        };

        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        let result = state.launch_task(ssn_ptr, task_ptr).await;

        if result.is_ok() {
            let _ = self.notifier.tasks.notify(&ssn_id, task_id);

            let executor = {
                let exe = lock_ptr!(exe_ptr)?;
                (*exe).clone()
            };
            self.storage.update_executor(&executor).await?;
        }

        result
    }

    async fn wait_for_task(
        &self,
        session: &SessionPtr,
        executor: &ExecutorPtr,
    ) -> Result<Option<TaskPtr>, FlameError> {
        let session_id = lock_ptr!(session)?.id.clone();
        let executor_id = lock_ptr!(executor)?.id.clone();
        let mut tasks_rx = self.notifier.tasks.subscribe(&session_id)?;
        let mut executors_rx = self.notifier.executors.subscribe(&executor_id)?;

        loop {
            {
                let mut session = lock_ptr!(session)?;
                let executor = lock_ptr!(executor)?;
                if !session.is_open()
                    || executor.is_unbinding(&session_id)
                    || executor.is_interrupted()
                {
                    return Ok(None);
                }
                if let Some(task) = session.pop_pending_task() {
                    return Ok(Some(task));
                }
            }

            tokio::select! {
                task = tasks_rx.recv() => {
                    if matches!(task, Err(RecvError::Closed)) {
                        return Ok(None);
                    }
                }
                changed = executors_rx.recv() => {
                    if matches!(changed, Err(RecvError::Closed)) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    pub async fn complete_task(
        &self,
        id: ExecutorID,
        task_result: TaskResult,
        attributes: Option<rpc::ExecutorAttributes>,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::complete_task");
        let attributes = attributes
            .map(Self::validate_executor_attributes)
            .transpose()?;
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let (ssn_id, task_id, host) = {
            let exe = lock_ptr!(exe_ptr)?;
            (
                exe.ssn_id.clone().ok_or(FlameError::InvalidState(
                    "no session in executor".to_string(),
                ))?,
                exe.task_id
                    .ok_or(FlameError::InvalidState("no task in executor".to_string()))?,
                exe.node.clone(),
            )
        };

        let gid = TaskGID {
            ssn_id: ssn_id.clone(),
            task_id,
        };

        let task_ptr = self.storage.get_task_ptr(gid)?;
        let ssn_ptr = self.storage.get_session_ptr(ssn_id.clone())?;

        let msg = match task_result.state {
            TaskState::Failed => task_result.message,
            TaskState::Succeed => Some(format!("Task completed successfully on host <{host}>.")),
            _ => {
                tracing::warn!(
                    "Invalid task state <{:?}> for task <{}/{}> on host <{}> when completing task",
                    task_result.state,
                    ssn_id,
                    task_id,
                    host
                );
                None
            }
        };

        let task_result = TaskResult {
            message: msg,
            ..task_result
        };

        if let Some(attributes) = attributes {
            self.update_executor_attributes(&id, attributes)?;
        }
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.complete_task(ssn_ptr, task_ptr, task_result).await?;
        let _ = self.notifier.tasks.notify(&ssn_id, task_id);

        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };
        self.storage.update_executor(&executor).await?;

        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about task completion: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    /// Request unbinding at the next task-poll round, preserving in-flight work.
    pub async fn interrupt_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        let executor_ptr = self.storage.get_executor_ptr(id.clone())?;
        {
            let mut executor = lock_ptr!(executor_ptr)?;
            executor.signal = Some(Signal::Interrupt);
        }
        self.notifier.executors.notify(&id, id.clone())?;
        Ok(())
    }

    /// Acknowledge the worker beginning its normal unbinding handshake.
    pub async fn unbind_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::unbind_executor");
        let exe_ptr = self.clone_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.unbind_executor().await?;

        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };
        self.storage.update_executor(&executor).await?;
        self.notifier.executors.notify(&id, id.clone())?;

        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about executor unbinding: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn unbind_executor_completed(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::unbind_executor_completed");
        let exe_ptr = self.clone_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;

        state.unbind_executor_completed().await?;

        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };
        self.storage.update_executor(&executor).await?;

        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about unbind completion: {}",
                executor.node,
                e
            );
        }

        self.notify_scheduler();
        Ok(())
    }

    pub async fn release_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::release_executor");
        let exe_ptr = self.clone_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.release_executor().await?;

        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };
        self.storage.update_executor(&executor).await?;
        self.notifier.executors.notify(&id, id.clone())?;

        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about executor release: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn unregister_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::unregister_executor");
        let exe_ptr = match self.storage.get_executor_ptr(id.clone()) {
            Ok(executor) => executor,
            // Removal is idempotent: FEM can acknowledge an executor that SM
            // already deleted without changing its normal cleanup workflow.
            Err(FlameError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };

        // Get executor info before unregistering for notification
        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };

        // Crash recovery is terminal in v1: after the node confirms targeted
        // service destruction, fail the interrupted task instead of retrying it.
        if let (Some(task_id), Some(ssn_id)) = (executor.task_id, executor.ssn_id.as_ref()) {
            let gid = TaskGID {
                ssn_id: ssn_id.clone(),
                task_id,
            };
            let ssn_ptr = self.storage.get_session_ptr(ssn_id.clone())?;
            let task_ptr = self.storage.get_task_ptr(gid)?;
            self.storage
                .update_task_state(
                    ssn_ptr,
                    task_ptr,
                    TaskState::Failed,
                    Some(format!(
                        "Executor <{}> was lost; its service was destroyed without retry",
                        executor.id
                    )),
                )
                .await?;
            let _ = self.notifier.tasks.notify(ssn_id, task_id);
        }

        let next = stdng::new_ptr(executor.clone());
        let state = executors::from(self.storage.clone(), next.clone())?;
        state.unregister_executor().await?;

        self.storage.delete_executor(id.clone()).await?;
        // Existing waiters can retain the pointer after it leaves the map.
        *lock_ptr!(exe_ptr)? = lock_ptr!(next)?.clone();
        self.notifier.executors.remove(&id)?;

        // Notify the node about the executor deletion.
        let mut released_executor = executor.clone();
        released_executor.state = ExecutorState::Released;
        released_executor.task_id = None;
        released_executor.ssn_id = None;
        if let Err(e) = self
            .connection_manager
            .notify_executor(&released_executor.node, &released_executor)
            .await
        {
            tracing::warn!(
                "Failed to notify node <{}> about executor unregistration: {}",
                executor.node,
                e
            );
        }
        Ok(())
    }

    pub async fn record_event(&self, owner: EventOwner, event: Event) -> Result<(), FlameError> {
        trace_fn!("Controller::record_event");
        self.storage.record_event(owner, event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{
        Node, NodeInfo, NodeState, ResourceRequirement, Shim, BIND_RESULT_OK, SESSION_BIND_FAILED,
        SESSION_RETRY_LIMIT_REACHED,
    };
    use common::ctx::{
        FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits, FlameRecovery,
        FlameSessionRecovery,
    };
    use tokio::sync::mpsc;

    #[test]
    fn task_options_reject_invalid_affinity_keys() {
        assert!(Controller::validate_task_options(&TaskOptions {
            affinity: std::collections::HashSet::from([bytes::Bytes::new()]),
        })
        .is_err());
        assert!(Controller::validate_task_options(&TaskOptions {
            affinity: std::collections::HashSet::from([bytes::Bytes::from(vec![0; 257])]),
        })
        .is_err());
    }

    #[test]
    fn executor_attributes_reject_invalid_keys_before_indexing() {
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                publish: vec![vec![]],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                publish: vec![vec![0; 257]],
                ..Default::default()
            })
            .is_err()
        );

        let attributes = Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: vec![b"kv-cache-key".to_vec(), b"kv-cache-key".to_vec()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            attributes.publish,
            HashSet::from([bytes::Bytes::from_static(b"kv-cache-key")])
        );
        assert!(attributes.retract.is_empty());

        let byte_boundary = (0_u16..256)
            .map(|index| {
                let mut value = index.to_be_bytes().to_vec();
                value.resize(Controller::MAX_EXECUTOR_ATTRIBUTE_BYTES, 0);
                value
            })
            .collect::<Vec<_>>();
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                publish: byte_boundary.clone(),
                ..Default::default()
            })
            .is_ok()
        );

        let mut over_byte_limit = byte_boundary;
        over_byte_limit.push(b"overflow".to_vec());
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                publish: over_byte_limit,
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn executor_attribute_retractions_are_validated_with_the_whole_update() {
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                retract: vec![vec![]],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                retract: vec![vec![0; 257]],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                publish: vec![b"x".to_vec()],
                retract: vec![b"x".to_vec()],
            })
            .is_err()
        );
        let too_many = (0..=Controller::MAX_EXECUTOR_ATTRIBUTES)
            .map(|index| format!("key-{index}").into_bytes())
            .collect::<Vec<_>>();
        assert!(
            Controller::validate_executor_attributes(rpc::ExecutorAttributes {
                publish: vec![b"ok".to_vec()],
                retract: too_many,
            })
            .is_err()
        );

        let update = Controller::validate_executor_attributes(rpc::ExecutorAttributes {
            publish: vec![b"x".to_vec()],
            retract: vec![b"y".to_vec(), b"y".to_vec()],
        })
        .unwrap();
        assert_eq!(update.publish.len(), 1);
        assert_eq!(update.retract.len(), 1);
    }

    /// Creates a test storage with a unique SQLite database.
    async fn create_test_storage() -> StoragePtr {
        create_test_storage_with_retry_limits(common::ctx::DEFAULT_SESSION_RETRY_LIMITS).await
    }

    async fn create_test_storage_with_retry_limits(retry_limits: u32) -> StoragePtr {
        let unique_id = format!(
            "{}_{:?}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            std::thread::current().id()
        );
        let test_dir = std::env::temp_dir().join(format!("flame_test_{}", unique_id));
        std::fs::create_dir_all(&test_dir).unwrap();
        let db_path = test_dir.join("flame.db");
        let url = format!("sqlite://{}", db_path.display());

        let ctx = FlameClusterContext {
            cluster: FlameCluster {
                name: "test".to_string(),
                endpoint: "http://localhost:8080".to_string(),
                storage: url,
                resreq: None,
                policies: vec!["priority".to_string(), "drf".to_string()],
                schedule_interval: 1000,
                executors: FlameExecutors {
                    shim: Shim::default(),
                    ..Default::default()
                },
                tls: None,
                limits: FlameLimits {
                    max_sessions: None,
                    node: common::ctx::FlameNodeLimits { max_executors: 10 },
                    ..Default::default()
                },
                recovery: FlameRecovery {
                    session: FlameSessionRecovery { retry_limits },
                },
                pprof: None,
            },
            cache: None,
        };

        crate::storage::new_ptr(&ctx).await.unwrap()
    }

    /// Creates a test node.
    fn create_test_node(name: &str) -> Node {
        Node {
            name: name.to_string(),
            state: NodeState::Unknown,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
                gpu: 0,
            },
            allocatable: ResourceRequirement {
                cpu: 8,
                memory: 16384,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        }
    }

    fn create_test_application() -> ApplicationAttributes {
        ApplicationAttributes {
            shim: Shim::default(),
            ..Default::default()
        }
    }

    fn create_test_session_attr(id: &str) -> SessionAttributes {
        SessionAttributes {
            batch_size: 1,
            id: id.to_string(),
            application: "test-app".to_string(),
            common_data: None,
            tokens: Default::default(),
            min_instances: 0,
            max_instances: None,
            priority: 0,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
        }
    }

    #[tokio::test]
    async fn new_tasks_and_idle_executors_wake_scheduler() {
        use tokio::time::Duration;

        let controller = new_ptr(create_test_storage().await);
        controller
            .register_application("test-app".to_string(), create_test_application())
            .await
            .unwrap();

        controller
            .create_session(create_test_session_attr("wake-session"))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                controller.wait_for_scheduler_event(Duration::from_secs(60)),
            )
            .await
            .is_err(),
            "a session without tasks must not wake the scheduler"
        );

        controller
            .create_task("wake-session".to_string(), None, None)
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_millis(100),
            controller.wait_for_scheduler_event(Duration::from_secs(60)),
        )
        .await
        .expect("new task should wake scheduler");

        controller
            .storage()
            .register_node(&create_test_node("wake-node"))
            .await
            .unwrap();
        let executor = controller
            .create_executor("wake-node".to_string(), "wake-session".to_string())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                controller.wait_for_scheduler_event(Duration::from_secs(60)),
            )
            .await
            .is_err(),
            "scheduler-owned executor creation must not wake the scheduler"
        );

        controller.register_executor(&executor).await.unwrap();
        tokio::time::timeout(
            Duration::from_millis(100),
            controller.wait_for_scheduler_event(Duration::from_secs(60)),
        )
        .await
        .expect("newly Idle executor should wake scheduler");

        controller
            .bind_session(executor.id.clone(), "wake-session".to_string())
            .await
            .unwrap();
        controller
            .bind_executor_completed(executor.id.clone(), None, None)
            .await
            .unwrap();
        controller
            .unbind_executor(executor.id.clone())
            .await
            .unwrap();
        controller
            .unbind_executor_completed(executor.id.clone())
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_millis(100),
            controller.wait_for_scheduler_event(Duration::from_secs(60)),
        )
        .await
        .expect("executor becoming Idle after unbind should wake scheduler");
    }

    async fn create_binding_executor(controller: &ControllerPtr, ssn_id: &str) -> String {
        controller
            .register_application("test-app".to_string(), create_test_application())
            .await
            .unwrap();
        controller
            .storage()
            .register_node(&create_test_node("bind-node"))
            .await
            .unwrap();
        controller
            .create_session(create_test_session_attr(ssn_id))
            .await
            .unwrap();
        let executor = controller
            .create_executor("bind-node".to_string(), ssn_id.to_string())
            .await
            .unwrap();
        controller.register_executor(&executor).await.unwrap();
        controller
            .bind_session(executor.id.clone(), ssn_id.to_string())
            .await
            .unwrap();
        executor.id
    }

    #[tokio::test]
    async fn unregister_missing_executor_succeeds() {
        let controller = new_ptr(create_test_storage().await);
        controller
            .unregister_executor("missing-executor".into())
            .await
            .unwrap();
        assert!(matches!(
            controller.get_executor("missing-executor".into()),
            Err(FlameError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn unregister_executor_is_idempotent() {
        let controller = new_ptr(create_test_storage().await);
        let id = create_binding_executor(&controller, "unregister-idempotent").await;
        controller.unregister_executor(id.clone()).await.unwrap();
        controller.unregister_executor(id.clone()).await.unwrap();
        assert!(matches!(
            controller.get_executor(id),
            Err(FlameError::NotFound(_))
        ));
    }

    async fn bound_task_waiter(minimum: u32) -> (ControllerPtr, String) {
        let config = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let controller = new_ptr(crate::storage::new_ptr(&config).await.unwrap());
        let executor_id = create_binding_executor(&controller, "task-wait-session").await;
        {
            let session = controller
                .storage
                .get_session_ptr("task-wait-session".into())
                .unwrap();
            lock_ptr!(session).unwrap().min_instances = minimum;
        }
        controller
            .bind_executor_completed(executor_id.clone(), None, None)
            .await
            .unwrap();
        (controller, executor_id)
    }

    #[tokio::test]
    async fn minimum_binding_waits_for_tasks_and_session_close() {
        use tokio::time::{timeout, Duration};
        let (controller, executor_id) = bound_task_waiter(1).await;
        let waiting = controller.launch_task(executor_id.clone());
        tokio::pin!(waiting);
        assert!(timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err());
        assert_eq!(
            controller.get_executor(executor_id.clone()).unwrap().state,
            ExecutorState::Bound
        );
        controller
            .create_task("task-wait-session".into(), None, None)
            .await
            .unwrap();
        let task = timeout(Duration::from_millis(100), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            controller
                .get_task("task-wait-session".into(), task.id)
                .unwrap()
                .state,
            TaskState::Running
        );
        controller
            .complete_task(
                executor_id.clone(),
                TaskResult {
                    state: TaskState::Succeed,
                    output: None,
                    message: None,
                },
                None,
            )
            .await
            .unwrap();

        let waiting = controller.launch_task(executor_id.clone());
        tokio::pin!(waiting);
        assert!(timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err());
        controller
            .close_session("task-wait-session".into())
            .await
            .unwrap();
        assert!(timeout(Duration::from_millis(100), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        assert_eq!(
            controller.get_executor(executor_id.clone()).unwrap().state,
            ExecutorState::Unbinding
        );
    }

    #[tokio::test]
    async fn closed_task_subscription_releases_minimum_waiter() {
        use tokio::time::{timeout, Duration};
        let (controller, executor_id) = bound_task_waiter(1).await;
        let waiting = controller.launch_task(executor_id.clone());
        tokio::pin!(waiting);
        assert!(timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err());
        controller
            .notifier
            .tasks
            .remove(&"task-wait-session".into())
            .unwrap();
        assert!(timeout(Duration::from_millis(100), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        assert_eq!(
            controller.get_executor(executor_id.clone()).unwrap().state,
            ExecutorState::Unbinding
        );
    }

    #[tokio::test]
    async fn lagged_executor_wakeups_keep_task_waiter_until_interrupted() {
        use tokio::time::{timeout, Duration};
        let (controller, executor_id) = bound_task_waiter(0).await;
        let waiting = controller.launch_task(executor_id.clone());
        tokio::pin!(waiting);
        assert!(timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err());

        // Overflow the bounded wakeup queue without polling the waiter.
        for _ in 0..4096 {
            controller
                .notifier
                .executors
                .notify(&executor_id, executor_id.clone())
                .unwrap();
        }
        assert!(timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err());
        controller
            .interrupt_executor(executor_id.clone())
            .await
            .unwrap();
        assert!(timeout(Duration::from_millis(100), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn scheduler_unbind_wakes_task_wait_and_preserves_pending_work() {
        use tokio::time::{timeout, Duration};
        let (controller, executor_id) = bound_task_waiter(0).await;
        let waiting = controller.launch_task(executor_id.clone());
        tokio::pin!(waiting);
        assert!(timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err());
        controller
            .interrupt_executor(executor_id.clone())
            .await
            .unwrap();
        let task = controller
            .create_task("task-wait-session".into(), None, None)
            .await
            .unwrap();
        assert!(timeout(Duration::from_millis(100), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(
            controller
                .get_task("task-wait-session".into(), task.id)
                .unwrap()
                .state,
            TaskState::Pending
        );
        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        controller
            .unbind_executor_completed(executor_id.clone())
            .await
            .unwrap();
        assert_eq!(
            controller.get_executor(executor_id.clone()).unwrap().state,
            ExecutorState::Idle
        );
    }

    #[tokio::test]
    async fn interrupt_during_task_launch_preserves_completion_and_stops_next_poll() {
        let (controller, executor_id) = bound_task_waiter(0).await;
        let task = controller
            .create_task("task-wait-session".into(), None, None)
            .await
            .unwrap();
        let executor = controller
            .storage
            .get_executor_ptr(executor_id.clone())
            .unwrap();
        let session = controller
            .storage
            .get_session_ptr("task-wait-session".into())
            .unwrap();
        let task_ptr = controller
            .wait_for_task(&session, &executor)
            .await
            .unwrap()
            .unwrap();
        let state = executors::from(controller.storage.clone(), executor.clone()).unwrap();
        // Reproduce Shuffle interrupting after Pending is popped but before
        // launch_task persists Running and publishes executor task ownership.
        controller
            .interrupt_executor(executor_id.clone())
            .await
            .unwrap();
        controller
            .interrupt_executor(executor_id.clone())
            .await
            .unwrap();
        assert_eq!(lock_ptr!(executor).unwrap().state, ExecutorState::Bound);
        state.launch_task(session, task_ptr).await.unwrap().unwrap();
        // Completion retry must still recover this task before handling the signal.
        assert_eq!(
            controller
                .launch_task(executor_id.clone())
                .await
                .unwrap()
                .unwrap()
                .id,
            task.id
        );
        controller
            .complete_task(
                executor_id.clone(),
                TaskResult {
                    state: TaskState::Succeed,
                    output: None,
                    message: None,
                },
                Some(rpc::ExecutorAttributes {
                    publish: vec![b"completed-key".to_vec()],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            controller
                .get_task("task-wait-session".into(), task.id)
                .unwrap()
                .state,
            TaskState::Succeed
        );
        assert!(controller
            .launch_task(executor_id.clone())
            .await
            .unwrap()
            .is_none());
        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        let leaving = controller.get_executor(executor_id.clone()).unwrap();
        assert_eq!(leaving.state, ExecutorState::Unbinding);
        assert!(leaving.is_interrupted());
        controller
            .unbind_executor_completed(executor_id.clone())
            .await
            .unwrap();
        let idle = controller.get_executor(executor_id).unwrap();
        assert_eq!(idle.state, ExecutorState::Idle);
        assert!(!idle.is_interrupted());
    }

    #[tokio::test]
    async fn executor_accumulates_attributes_and_retains_them_on_unbind() {
        let storage = create_test_storage().await;
        let controller = new_ptr(storage);
        let ssn_id = "executor-attributes-unbind".to_string();
        let executor_id = create_binding_executor(&controller, &ssn_id).await;

        controller
            .bind_executor_completed(
                executor_id.clone(),
                Some(FlameResult {
                    return_code: BIND_RESULT_OK,
                    message: None,
                }),
                Some(rpc::ExecutorAttributes {
                    publish: vec![b"kv-cache-key".to_vec()],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let executor = controller.get_executor(executor_id.clone()).unwrap();
        assert_eq!(executor.application, "test-app");
        assert_eq!(
            executor.attributes,
            HashSet::from([bytes::Bytes::from_static(b"kv-cache-key")])
        );

        controller
            .update_executor_attributes(&executor_id, ExecutorAttributeUpdate::default())
            .unwrap();
        assert_eq!(
            controller
                .get_executor(executor_id.clone())
                .unwrap()
                .attributes,
            HashSet::from([bytes::Bytes::from_static(b"kv-cache-key")])
        );
        controller
            .create_task(ssn_id.clone(), None, None)
            .await
            .unwrap();
        controller
            .launch_task(executor_id.clone())
            .await
            .unwrap()
            .unwrap();
        controller
            .complete_task(
                executor_id.clone(),
                TaskResult {
                    state: TaskState::Succeed,
                    message: None,
                    output: None,
                },
                Some(rpc::ExecutorAttributes {
                    publish: vec![b"second-key".to_vec()],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        let expected_attributes = HashSet::from([
            bytes::Bytes::from_static(b"kv-cache-key"),
            bytes::Bytes::from_static(b"second-key"),
        ]);
        assert_eq!(
            controller
                .get_executor(executor_id.clone())
                .unwrap()
                .attributes,
            expected_attributes
        );

        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        controller
            .unbind_executor_completed(executor_id.clone())
            .await
            .unwrap();

        let executor = controller.get_executor(executor_id.clone()).unwrap();
        assert_eq!(executor.application, "test-app");
        assert_eq!(executor.attributes, expected_attributes);

        let snapshot = controller.snapshot().unwrap();
        let executor = snapshot
            .find_executors(None)
            .unwrap()
            .remove(&executor_id)
            .unwrap();
        assert_eq!(executor.application, "test-app");
        assert_eq!(executor.attributes, expected_attributes);

        controller
            .release_executor(executor_id.clone())
            .await
            .unwrap();
        controller
            .unregister_executor(executor_id.clone())
            .await
            .unwrap();
        assert!(controller.get_executor(executor_id).is_err());
    }

    #[tokio::test]
    async fn executor_retracts_attributes_and_keeps_the_rest_on_unbind() {
        let storage = create_test_storage().await;
        let controller = new_ptr(storage);
        let ssn_id = "executor-attributes-retract".to_string();
        let executor_id = create_binding_executor(&controller, &ssn_id).await;
        let key = |value: &'static [u8]| bytes::Bytes::from_static(value);

        controller
            .bind_executor_completed(
                executor_id.clone(),
                Some(FlameResult {
                    return_code: BIND_RESULT_OK,
                    message: None,
                }),
                Some(rpc::ExecutorAttributes {
                    publish: vec![b"x".to_vec(), b"y".to_vec()],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        // An invalid update leaves the previous state unchanged.
        controller
            .create_task(ssn_id.clone(), None, None)
            .await
            .unwrap();
        controller
            .launch_task(executor_id.clone())
            .await
            .unwrap()
            .unwrap();
        let succeed = || TaskResult {
            state: TaskState::Succeed,
            message: None,
            output: None,
        };
        assert!(controller
            .complete_task(
                executor_id.clone(),
                succeed(),
                Some(rpc::ExecutorAttributes {
                    publish: vec![b"z".to_vec()],
                    retract: vec![b"x".to_vec(), vec![]],
                }),
            )
            .await
            .is_err());
        assert_eq!(
            controller
                .get_executor(executor_id.clone())
                .unwrap()
                .attributes,
            HashSet::from([key(b"x"), key(b"y")])
        );

        // Retracting X (and an absent key) keeps Y; an empty update is a no-op.
        controller
            .complete_task(
                executor_id.clone(),
                succeed(),
                Some(rpc::ExecutorAttributes {
                    retract: vec![b"x".to_vec(), b"absent".to_vec()],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            controller
                .get_executor(executor_id.clone())
                .unwrap()
                .attributes,
            HashSet::from([key(b"y")])
        );
        controller
            .update_executor_attributes(&executor_id, ExecutorAttributeUpdate::default())
            .unwrap();

        // The retraction survives an ordinary unbind.
        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        controller
            .unbind_executor_completed(executor_id.clone())
            .await
            .unwrap();
        assert_eq!(
            controller
                .get_executor(executor_id.clone())
                .unwrap()
                .attributes,
            HashSet::from([key(b"y")])
        );

        // A replacement executor starts without the retired executor's keys.
        let replacement_ssn = "executor-attributes-replacement";
        controller
            .create_session(create_test_session_attr(replacement_ssn))
            .await
            .unwrap();
        let replacement = controller
            .create_executor("bind-node".to_string(), replacement_ssn.to_string())
            .await
            .unwrap();
        controller.register_executor(&replacement).await.unwrap();
        assert_ne!(replacement.id, executor_id);
        assert!(controller
            .get_executor(replacement.id)
            .unwrap()
            .attributes
            .is_empty());
    }

    #[tokio::test]
    async fn unregister_application_only_disables_application() {
        let storage = create_test_storage().await;
        let controller = new_ptr(storage);
        let ssn_id = "unregister-releases-idle";
        let executor_id = create_binding_executor(&controller, ssn_id).await;

        controller
            .bind_executor_completed(
                executor_id.clone(),
                Some(FlameResult {
                    return_code: BIND_RESULT_OK,
                    message: None,
                }),
                None,
            )
            .await
            .unwrap();
        controller.close_session(ssn_id.to_string()).await.unwrap();
        controller
            .unbind_executor(executor_id.clone())
            .await
            .unwrap();
        controller
            .unbind_executor_completed(executor_id.clone())
            .await
            .unwrap();

        assert_eq!(
            controller.get_executor(executor_id.clone()).unwrap().state,
            ExecutorState::Idle
        );

        controller
            .unregister_application("test-app".to_string())
            .await
            .unwrap();

        assert_eq!(
            controller.get_executor(executor_id).unwrap().state,
            ExecutorState::Idle
        );
        assert_eq!(
            controller
                .get_application("test-app".to_string())
                .await
                .unwrap()
                .state,
            ApplicationState::Disabled
        );
    }

    #[tokio::test]
    async fn disabled_application_rejects_create_and_open() {
        let storage = create_test_storage().await;
        let controller = new_ptr(storage);
        controller
            .register_application("disabled-app".to_string(), ApplicationAttributes::default())
            .await
            .unwrap();

        let attributes = SessionAttributes {
            id: "disabled-session".to_string(),
            application: "disabled-app".to_string(),
            ..SessionAttributes::default()
        };
        controller.create_session(attributes.clone()).await.unwrap();
        controller
            .close_session(attributes.id.clone())
            .await
            .unwrap();
        controller
            .unregister_application("disabled-app".to_string())
            .await
            .unwrap();

        assert!(matches!(
            controller
                .create_session(SessionAttributes {
                    id: "new-disabled-session".to_string(),
                    ..attributes.clone()
                })
                .await,
            Err(FlameError::InvalidState(_))
        ));
        assert!(matches!(
            controller.open_session(attributes.id.clone(), None).await,
            Err(FlameError::InvalidState(_))
        ));
        assert!(matches!(
            controller
                .open_session(
                    "missing-disabled-session".to_string(),
                    Some(SessionAttributes {
                        id: "missing-disabled-session".to_string(),
                        ..attributes
                    })
                )
                .await,
            Err(FlameError::InvalidState(_))
        ));
    }

    // ========================================================================
    // Controller::register_node Tests
    // ========================================================================

    mod bind_session_failed_tests {
        use super::*;
        use uuid::Uuid;

        async fn fail_binding(
            controller: &ControllerPtr,
            executor_id: &str,
        ) -> Result<(), FlameError> {
            controller
                .bind_executor_completed(
                    executor_id.to_string(),
                    Some(FlameResult {
                        return_code: common::apis::BIND_RESULT_ON_SESSION_ENTER_FAILED,
                        message: Some("on_session_enter failed: boom".to_string()),
                    }),
                    None,
                )
                .await
        }

        #[tokio::test]
        async fn records_failure_event_and_moves_executor_to_unbinding() {
            let storage = create_test_storage_with_retry_limits(2).await;
            let controller = new_ptr(storage.clone());
            let ssn_id = format!("bind-failed-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;

            fail_binding(&controller, &executor_id).await.unwrap();

            let executor = controller.get_executor(executor_id).unwrap();
            assert_eq!(executor.state, ExecutorState::Unbinding);
            assert_eq!(executor.ssn_id, None);
            assert_eq!(executor.task_id, None);

            let session = controller.get_session(ssn_id).unwrap();
            assert_eq!(session.retry_count, 1);
            assert_eq!(
                session
                    .events
                    .iter()
                    .filter(|event| event.code == SESSION_BIND_FAILED)
                    .count(),
                1
            );
            assert!(session.events.iter().any(|event| event
                .message
                .as_deref()
                .is_some_and(|message| message.contains("on_session_enter failed: boom"))));
        }

        #[tokio::test]
        async fn failed_bind_after_session_close_moves_executor_to_unbinding_without_events() {
            let storage = create_test_storage_with_retry_limits(1).await;
            let controller = new_ptr(storage);
            let ssn_id = format!("closed-bind-failed-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;
            controller
                .record_event(
                    EventOwner::session(ssn_id.clone()),
                    Event {
                        code: SESSION_BIND_FAILED,
                        message: Some("earlier binding failure".to_string()),
                        creation_time: chrono::Utc::now(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                controller.get_session(ssn_id.clone()).unwrap().events.len(),
                1
            );

            controller.close_session(ssn_id.clone()).await.unwrap();
            assert_eq!(
                controller.get_executor(executor_id.clone()).unwrap().state,
                ExecutorState::Binding
            );

            // The worker can finish its failed bind after the session has closed.
            // Both failure and retry-limit events must be ignored without
            // preventing the executor from completing its unbinding handshake.
            fail_binding(&controller, &executor_id).await.unwrap();

            let executor = controller.get_executor(executor_id.clone()).unwrap();
            assert_eq!(executor.state, ExecutorState::Unbinding);
            assert_eq!(executor.ssn_id, None);
            assert_eq!(executor.task_id, None);
            let session = controller.get_session(ssn_id.clone()).unwrap();
            assert_eq!(session.status.state, SessionState::Closed);
            assert!(session.events.is_empty());

            controller
                .unbind_executor_completed(executor_id.clone())
                .await
                .unwrap();
            assert_eq!(
                controller.get_executor(executor_id).unwrap().state,
                ExecutorState::Idle
            );
            assert!(controller.get_session(ssn_id).unwrap().events.is_empty());
        }

        #[tokio::test]
        async fn records_retry_limit_once_and_allows_count_to_exceed_limit() {
            let storage = create_test_storage_with_retry_limits(2).await;
            let controller = new_ptr(storage.clone());
            let ssn_id = format!("retry-limit-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;

            fail_binding(&controller, &executor_id).await.unwrap();
            controller
                .unbind_executor_completed(executor_id.clone())
                .await
                .unwrap();
            controller
                .bind_session(executor_id.clone(), ssn_id.clone())
                .await
                .unwrap();

            fail_binding(&controller, &executor_id).await.unwrap();
            controller
                .unbind_executor_completed(executor_id.clone())
                .await
                .unwrap();
            controller
                .bind_session(executor_id.clone(), ssn_id.clone())
                .await
                .unwrap();

            fail_binding(&controller, &executor_id).await.unwrap();

            let session = controller.get_session(ssn_id).unwrap();
            assert_eq!(session.retry_count, 3);
            assert_eq!(
                session
                    .events
                    .iter()
                    .filter(|event| event.code == SESSION_BIND_FAILED)
                    .count(),
                3
            );
            assert_eq!(
                session
                    .events
                    .iter()
                    .filter(|event| event.code == SESSION_RETRY_LIMIT_REACHED)
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn bind_success_keeps_retry_count() {
            let storage = create_test_storage_with_retry_limits(2).await;
            let controller = new_ptr(storage.clone());
            let ssn_id = format!("retry-keep-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;

            fail_binding(&controller, &executor_id).await.unwrap();
            controller
                .unbind_executor_completed(executor_id.clone())
                .await
                .unwrap();
            controller
                .bind_session(executor_id.clone(), ssn_id.clone())
                .await
                .unwrap();

            controller
                .bind_executor_completed(
                    executor_id.clone(),
                    Some(FlameResult {
                        return_code: BIND_RESULT_OK,
                        message: None,
                    }),
                    None,
                )
                .await
                .unwrap();

            let session = controller.get_session(ssn_id).unwrap();
            assert_eq!(session.retry_count, 1);
            assert_eq!(
                controller.get_executor(executor_id).unwrap().state,
                ExecutorState::Bound
            );
        }

        #[tokio::test]
        async fn bind_success_requires_attached_session() {
            let storage = create_test_storage_with_retry_limits(2).await;
            let controller = new_ptr(storage.clone());
            let ssn_id = format!("missing-session-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;

            {
                let executor = storage.get_executor_ptr(executor_id.clone()).unwrap();
                let mut executor = lock_ptr!(executor).unwrap();
                executor.ssn_id = None;
            }

            let err = controller
                .bind_executor_completed(
                    executor_id.clone(),
                    Some(FlameResult {
                        return_code: BIND_RESULT_OK,
                        message: None,
                    }),
                    None,
                )
                .await
                .unwrap_err();

            assert!(matches!(err, FlameError::InvalidState(_)));
            assert_eq!(
                controller.get_executor(executor_id).unwrap().state,
                ExecutorState::Binding
            );
        }
    }

    mod wait_for_session_tests {
        use super::*;
        use uuid::Uuid;

        async fn persistent_lifecycle_executor(
        ) -> (tempfile::TempDir, ControllerPtr, Executor, String) {
            let directory = tempfile::TempDir::new().unwrap();
            let url = format!("sqlite://{}", directory.path().join("binding.db").display());
            let ctx = FlameClusterContext {
                cluster: FlameCluster {
                    storage: url.clone(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let controller = new_ptr(crate::storage::new_ptr(&ctx).await.unwrap());
            let session_id = "persist-before-assignment".to_string();
            controller
                .register_application("test-app".to_string(), create_test_application())
                .await
                .unwrap();
            controller
                .storage()
                .register_node(&create_test_node("persist-binding-node"))
                .await
                .unwrap();
            controller
                .create_session(create_test_session_attr(&session_id))
                .await
                .unwrap();
            let executor = controller
                .create_executor("persist-binding-node".to_string(), session_id.clone())
                .await
                .unwrap();
            (directory, controller, executor, url)
        }

        async fn assert_transition_is_staged<F>(
            pool: &sqlx::SqlitePool,
            executor: &ExecutorPtr,
            transition: F,
            expected: ExecutorState,
        ) where
            F: std::future::Future<Output = Result<(), FlameError>>,
        {
            use tokio::time::{timeout, Duration};

            let before = lock_ptr!(executor).unwrap().clone();
            let mut transaction = pool.begin().await.unwrap();
            sqlx::query("UPDATE executors SET state = state WHERE id = ?")
                .bind(&before.id)
                .execute(&mut *transaction)
                .await
                .unwrap();
            tokio::pin!(transition);
            assert!(timeout(Duration::from_millis(50), &mut transition)
                .await
                .is_err());
            {
                let current = lock_ptr!(executor).unwrap();
                assert_eq!(current.state, before.state);
                assert_eq!(current.ssn_id, before.ssn_id);
                assert_eq!(current.task_id, before.task_id);
                assert_eq!(current.signal, before.signal);
            }
            transaction.commit().await.unwrap();
            timeout(Duration::from_secs(1), transition)
                .await
                .expect("transition should finish after persistence is unblocked")
                .unwrap();
            assert_eq!(lock_ptr!(executor).unwrap().state, expected);
            let persisted: Option<i32> =
                sqlx::query_scalar("SELECT state FROM executors WHERE id = ?")
                    .bind(&before.id)
                    .fetch_optional(pool)
                    .await
                    .unwrap();
            assert_eq!(
                persisted,
                (expected != ExecutorState::Released).then(|| i32::from(expected))
            );
        }

        #[tokio::test]
        async fn lifecycle_transitions_are_visible_only_after_persistence() {
            let (_directory, controller, executor, url) = persistent_lifecycle_executor().await;
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            let shared = controller
                .storage()
                .get_executor_ptr(executor.id.clone())
                .unwrap();
            assert_eq!(lock_ptr!(shared).unwrap().state, ExecutorState::Void);
            assert_transition_is_staged(
                &pool,
                &shared,
                controller.register_executor(&executor),
                ExecutorState::Idle,
            )
            .await;
            assert_transition_is_staged(
                &pool,
                &shared,
                controller
                    .bind_session(executor.id.clone(), "persist-before-assignment".to_string()),
                ExecutorState::Binding,
            )
            .await;
            assert_transition_is_staged(
                &pool,
                &shared,
                controller.bind_executor_completed(executor.id.clone(), None, None),
                ExecutorState::Bound,
            )
            .await;
            assert_transition_is_staged(
                &pool,
                &shared,
                controller.unbind_executor(executor.id.clone()),
                ExecutorState::Unbinding,
            )
            .await;
            controller
                .interrupt_executor(executor.id.clone())
                .await
                .unwrap();
            assert_eq!(lock_ptr!(shared).unwrap().signal, Some(Signal::Interrupt));
            assert_transition_is_staged(
                &pool,
                &shared,
                controller.unbind_executor_completed(executor.id.clone()),
                ExecutorState::Idle,
            )
            .await;
            assert_eq!(lock_ptr!(shared).unwrap().signal, None);
            assert_transition_is_staged(
                &pool,
                &shared,
                controller.release_executor(executor.id.clone()),
                ExecutorState::Releasing,
            )
            .await;
            assert_transition_is_staged(
                &pool,
                &shared,
                controller.unregister_executor(executor.id.clone()),
                ExecutorState::Released,
            )
            .await;
            assert!(matches!(
                controller.get_executor(executor.id),
                Err(FlameError::NotFound(_))
            ));
        }

        #[tokio::test]
        async fn assignment_is_visible_only_after_binding_is_persisted() {
            use tokio::time::{timeout, Duration};

            let (_directory, controller, executor, url) = persistent_lifecycle_executor().await;
            let session_id = "persist-before-assignment".to_string();
            controller.register_executor(&executor).await.unwrap();

            // An external transaction holds SQLite's write lock while the
            // scheduler attempts to persist the new executor assignment.
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            let mut transaction = pool.begin().await.unwrap();
            sqlx::query("UPDATE executors SET state = state WHERE id = ?")
                .bind(&executor.id)
                .execute(&mut *transaction)
                .await
                .unwrap();
            let binding = controller.bind_session(executor.id.clone(), session_id.clone());
            tokio::pin!(binding);
            assert!(timeout(Duration::from_millis(50), &mut binding)
                .await
                .is_err());

            let current = controller.get_executor(executor.id.clone()).unwrap();
            assert_eq!(current.state, ExecutorState::Idle);
            assert_eq!(current.ssn_id, None);
            let waiting = controller.wait_for_session(executor.id.clone());
            tokio::pin!(waiting);
            assert!(timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err());

            transaction.commit().await.unwrap();
            timeout(Duration::from_secs(1), binding)
                .await
                .expect("binding should finish after persistence is unblocked")
                .unwrap();
            let assigned = timeout(Duration::from_secs(1), waiting)
                .await
                .expect("persisted assignment should wake the executor")
                .unwrap()
                .unwrap();
            assert_eq!(assigned.id, session_id);
            timeout(
                Duration::from_secs(1),
                controller.bind_executor_completed(executor.id.clone(), None, None),
            )
            .await
            .expect("bind acknowledgement should finish")
            .unwrap();
            assert_eq!(
                controller.get_executor(executor.id).unwrap().state,
                ExecutorState::Bound
            );
        }

        #[tokio::test]
        async fn returns_none_when_assigned_session_is_closed() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage);
            let ssn_id = format!("closed-session-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;

            controller.close_session(ssn_id.clone()).await.unwrap();

            let session = controller
                .wait_for_session(executor_id.clone())
                .await
                .unwrap();
            assert!(session.is_none());

            let executor = controller.get_executor(executor_id.clone()).unwrap();
            assert_eq!(executor.state, ExecutorState::Binding);
            assert_eq!(executor.ssn_id, Some(ssn_id));

            controller
                .unregister_executor(executor_id.clone())
                .await
                .unwrap();

            assert!(matches!(
                controller.get_executor(executor_id).unwrap_err(),
                FlameError::NotFound(_)
            ));
        }

        #[tokio::test]
        async fn returns_none_when_assigned_session_is_missing() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage);
            let ssn_id = format!("missing-session-{}", Uuid::new_v4());
            let executor_id = create_binding_executor(&controller, &ssn_id).await;

            controller.close_session(ssn_id.clone()).await.unwrap();
            controller.delete_session(ssn_id.clone()).await.unwrap();

            let session = controller
                .wait_for_session(executor_id.clone())
                .await
                .unwrap();
            assert!(session.is_none());

            let executor = controller.get_executor(executor_id.clone()).unwrap();
            assert_eq!(executor.state, ExecutorState::Binding);
            assert_eq!(executor.ssn_id, Some(ssn_id));

            controller
                .unregister_executor(executor_id.clone())
                .await
                .unwrap();

            assert!(matches!(
                controller.get_executor(executor_id).unwrap_err(),
                FlameError::NotFound(_)
            ));
        }
    }

    mod register_node_tests {
        use super::*;
        use std::collections::HashSet;
        use std::time::Duration;

        #[tokio::test]
        async fn test_update_node_does_not_transition_state() {
            // This tests that update_node (used for heartbeat) does NOT transition state
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let node = create_test_node("heartbeat-node");

            // First register the node (sets it to Unknown state in storage)
            storage.register_node(&node).await.unwrap();

            // Now call update_node (heartbeat case)
            let result = controller.update_node(&node).await;

            assert!(result.is_ok());

            // Verify node state was NOT changed to Ready
            // The node should remain in whatever state it was in storage
            let stored_node = storage.get_node("heartbeat-node").unwrap();
            assert!(stored_node.is_some());
            // Note: The initial state is Unknown, and without a stream connection,
            // it should NOT transition to Ready
        }

        #[tokio::test]
        async fn test_register_node_with_stream_transitions_to_ready() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let node = create_test_node("stream-node");

            // Register node
            let result = controller.register_node(&node, &[]).await;

            assert!(result.is_ok());

            // Verify node state WAS changed to Ready (via on_connected callback)
            let stored_node = storage.get_node("stream-node").unwrap();
            assert!(stored_node.is_some());
            assert_eq!(stored_node.unwrap().state, NodeState::Ready);
        }

        #[tokio::test]
        async fn test_register_node_fresh_connection_succeeds() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let node = create_test_node("fresh-node");

            // Fresh connection with no reported executors
            let result = controller.register_node(&node, &[]).await;

            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn missing_persisted_executor_is_sent_for_targeted_release() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());
            let node = create_test_node("recovery-node");
            storage.register_node(&node).await.unwrap();
            controller
                .register_application("test-app".to_string(), create_test_application())
                .await
                .unwrap();
            let session = controller
                .create_session(create_test_session_attr("recovery-session"))
                .await
                .unwrap();
            let executor = storage
                .create_executor(node.name.clone(), session.id)
                .await
                .unwrap();

            controller.register_node(&node, &[]).await.unwrap();

            let (_, receiver) = controller.get_node_channel(&node.name).unwrap().unwrap();
            let cleanup = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cleanup.id, executor.id);
            assert_eq!(cleanup.state, ExecutorState::Releasing);
            assert_eq!(
                controller.get_executor(executor.id).unwrap().state,
                ExecutorState::Releasing
            );
        }

        #[tokio::test]
        async fn cleanup_ack_fails_interrupted_task_without_retry() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());
            let node = create_test_node("terminal-recovery-node");
            storage.register_node(&node).await.unwrap();
            controller
                .register_application("test-app".to_string(), create_test_application())
                .await
                .unwrap();
            let session = controller
                .create_session(create_test_session_attr("terminal-recovery-session"))
                .await
                .unwrap();
            let task = controller
                .create_task(session.id.clone(), None, None)
                .await
                .unwrap();
            let session_ptr = storage.get_session_ptr(session.id.clone()).unwrap();
            let task_ptr = storage
                .get_task_ptr(TaskGID {
                    ssn_id: session.id.clone(),
                    task_id: task.id,
                })
                .unwrap();
            storage
                .update_task_state(session_ptr, task_ptr, TaskState::Running, None)
                .await
                .unwrap();
            let mut executor = storage
                .create_executor(node.name, session.id.clone())
                .await
                .unwrap();
            executor.state = ExecutorState::Releasing;
            executor.ssn_id = Some(session.id.clone());
            executor.task_id = Some(task.id);
            storage.update_executor(&executor).await.unwrap();

            controller
                .unregister_executor(executor.id.clone())
                .await
                .unwrap();

            assert_eq!(
                controller.get_task(session.id, task.id).unwrap().state,
                TaskState::Failed
            );
            assert!(matches!(
                controller.get_executor(executor.id).unwrap_err(),
                FlameError::NotFound(_)
            ));
        }

        #[tokio::test]
        async fn test_register_node_replaces_existing_connection() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let node = create_test_node("replace-node");

            // First connection
            controller.register_node(&node, &[]).await.unwrap();

            // Second connection (replaces first)
            let result = controller.register_node(&node, &[]).await;

            assert!(result.is_ok());
            // Node should still be Ready
            let stored_node = storage.get_node("replace-node").unwrap().unwrap();
            assert_eq!(stored_node.state, NodeState::Ready);
        }

        #[tokio::test]
        async fn test_register_node_preserves_persisted_executor_application() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage);
            let executor_id = create_binding_executor(&controller, "reconnect-application").await;
            let mut reported = controller.get_executor(executor_id.clone()).unwrap();
            assert_eq!(reported.application, "test-app");
            reported.application = "other-app".to_string();

            controller
                .register_node(&create_test_node("bind-node"), &[reported])
                .await
                .unwrap();

            assert_eq!(
                controller.get_executor(executor_id).unwrap().application,
                "test-app"
            );
        }

        #[tokio::test]
        async fn test_register_node_rejects_a_new_node_that_reports_executors() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());
            let node = create_test_node("new-busy-node");
            let reported = Executor {
                id: "running-executor".to_string(),
                node: "new-busy-node".to_string(),
                application: "test-app".to_string(),
                state: ExecutorState::Bound,
                ..Default::default()
            };

            let result = controller.register_node(&node, &[reported]).await;

            assert!(matches!(result, Err(FlameError::InvalidState(_))));
            // Nothing was recorded for the rejected node.
            assert!(storage.get_node("new-busy-node").unwrap().is_none());
            assert!(matches!(
                controller.get_executor("running-executor".to_string()),
                Err(FlameError::NotFound(_))
            ));

            // After the admin clears it, it joins with no executors.
            controller.register_node(&node, &[]).await.unwrap();
            assert!(storage.get_node("new-busy-node").unwrap().is_some());
        }

        #[tokio::test]
        async fn test_register_node_rejects_executors_unknown_to_a_known_node() {
            let controller = new_ptr(create_test_storage().await);
            let known_id =
                create_binding_executor(&controller, "known-node-unknown-executor").await;
            let known = controller.get_executor(known_id.clone()).unwrap();
            let node = create_test_node("bind-node");
            let unknown = Executor {
                id: "unknown-executor".to_string(),
                node: "bind-node".to_string(),
                application: "test-app".to_string(),
                state: ExecutorState::Bound,
                ..Default::default()
            };

            let result = controller.register_node(&node, &[known, unknown]).await;

            assert!(matches!(result, Err(FlameError::InvalidState(_))));
            // The rejection changes nothing: the known executor is untouched.
            assert_eq!(
                controller.get_executor(known_id).unwrap().state,
                ExecutorState::Binding
            );
            assert!(matches!(
                controller.get_executor("unknown-executor".to_string()),
                Err(FlameError::NotFound(_))
            ));
        }

        #[tokio::test]
        async fn test_register_node_reconnect_during_drain() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let node = create_test_node("drain-reconnect-node");

            // Initial connection
            controller.register_node(&node, &[]).await.unwrap();

            // Start draining
            controller.drain_node("drain-reconnect-node").await.unwrap();

            // Verify node is in Unknown state (draining)
            let stored_node = storage.get_node("drain-reconnect-node").unwrap().unwrap();
            assert_eq!(stored_node.state, NodeState::Unknown);

            // Reconnect before timeout
            let result = controller.register_node(&node, &[]).await;

            assert!(result.is_ok());
            // Node should be back to Ready
            let stored_node = storage.get_node("drain-reconnect-node").unwrap().unwrap();
            assert_eq!(stored_node.state, NodeState::Ready);
        }

        #[tokio::test]
        async fn test_register_node_updates_node_info() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let mut node = create_test_node("update-node");

            // Initial registration
            controller.register_node(&node, &[]).await.unwrap();

            // Update node info
            node.capacity = ResourceRequirement {
                cpu: 16,
                memory: 32768,
                gpu: 0,
            };
            node.allocatable = ResourceRequirement {
                cpu: 14,
                memory: 28672,
                gpu: 0,
            };

            controller.register_node(&node, &[]).await.unwrap();

            // Verify info was updated
            let stored_node = storage.get_node("update-node").unwrap().unwrap();
            assert_eq!(stored_node.capacity.cpu, 16);
            assert_eq!(stored_node.allocatable.memory, 28672);
        }
    }

    // ========================================================================
    // Controller::drain_node Tests
    // ========================================================================

    mod drain_node_tests {
        use super::*;

        #[tokio::test]
        async fn test_drain_node_transitions_to_unknown() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage.clone());

            let node = create_test_node("drain-test-node");

            // Connect node
            controller.register_node(&node, &[]).await.unwrap();

            // Drain node
            let result = controller.drain_node("drain-test-node").await;

            assert!(result.is_ok());

            // Verify node state is Unknown (draining)
            let stored_node = storage.get_node("drain-test-node").unwrap().unwrap();
            assert_eq!(stored_node.state, NodeState::Unknown);
        }

        #[tokio::test]
        async fn test_drain_nonexistent_node_succeeds() {
            let storage = create_test_storage().await;
            let controller = new_ptr(storage);

            // Draining a node that doesn't exist should not error
            let result = controller.drain_node("nonexistent-node").await;

            assert!(result.is_ok());
        }
    }

    mod suspend_tests {
        use super::*;
        use tokio::time::sleep;

        const NODE: &str = "suspend-node";

        async fn memory_controller() -> ControllerPtr {
            let config = FlameClusterContext {
                cluster: FlameCluster {
                    storage: "none".into(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let storage = crate::storage::new_ptr(&config).await.unwrap();
            let controller = new_ptr(storage);
            controller
                .register_application("test-app".to_string(), create_test_application())
                .await
                .unwrap();
            controller
                .storage()
                .register_node(&create_test_node(NODE))
                .await
                .unwrap();
            controller
        }

        async fn open(controller: &ControllerPtr, ssn_id: &str) {
            controller
                .create_session(create_test_session_attr(ssn_id))
                .await
                .unwrap();
        }

        /// Assigns a new executor to the session; `bound` also completes the bind.
        async fn assign(controller: &ControllerPtr, ssn_id: &str, bound: bool) -> String {
            let executor = controller
                .create_executor(NODE.to_string(), ssn_id.to_string())
                .await
                .unwrap();
            controller.register_executor(&executor).await.unwrap();
            controller
                .bind_session(executor.id.clone(), ssn_id.to_string())
                .await
                .unwrap();
            if bound {
                controller
                    .bind_executor_completed(executor.id.clone(), None, None)
                    .await
                    .unwrap();
            }
            executor.id
        }

        fn state_of(controller: &ControllerPtr, ssn_id: &str) -> SessionState {
            controller
                .get_session(ssn_id.to_string())
                .unwrap()
                .status
                .state
        }

        /// The scheduler must never see an executor naming a session it cannot find.
        fn assert_snapshot_consistent(controller: &ControllerPtr) {
            let snapshot = controller.storage().snapshot().unwrap();
            for executor in &snapshot.executors {
                if let Some(ssn_id) = executor.ssn_id.as_ref() {
                    assert!(
                        snapshot.sessions.iter().any(|ssn| &ssn.id == ssn_id),
                        "executor <{}> references session <{}> missing from the snapshot",
                        executor.id,
                        ssn_id
                    );
                }
            }
        }

        fn succeed() -> TaskResult {
            TaskResult {
                state: TaskState::Succeed,
                output: None,
                message: None,
            }
        }

        async fn unbind(controller: &ControllerPtr, executor_id: &str) {
            controller
                .unbind_executor(executor_id.to_string())
                .await
                .unwrap();
            controller
                .unbind_executor_completed(executor_id.to_string())
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn suspend_without_executor_completes_in_suspend_session() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;

            let session = controller
                .signal_session("ssn".into(), Signal::Suspend)
                .await
                .unwrap();
            assert!(session.is_suspending());
            assert_eq!(controller.storage().snapshot().unwrap().sessions.len(), 1);

            controller.suspend_session("ssn".into()).await.unwrap();
            assert_eq!(state_of(&controller, "ssn"), SessionState::Suspended);
            assert!(controller.storage().snapshot().unwrap().sessions.is_empty());
            // Completing again, or an unknown/open session, changes nothing.
            controller.suspend_session("ssn".into()).await.unwrap();
            assert_eq!(state_of(&controller, "ssn"), SessionState::Suspended);

            let session = controller.resume_session("ssn".into()).await.unwrap();
            assert!(session.is_open());
            controller.suspend_session("ssn".into()).await.unwrap();
            assert_eq!(state_of(&controller, "ssn"), SessionState::Open);
        }

        #[tokio::test]
        async fn suspend_unbinds_bound_executor_then_suspends() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;
            let executor_id = assign(&controller, "ssn", true).await;

            let waiting = {
                let controller = controller.clone();
                let executor_id = executor_id.clone();
                tokio::spawn(async move { controller.launch_task(executor_id).await })
            };
            sleep(Duration::from_millis(20)).await;

            let session = controller
                .signal_session("ssn".into(), Signal::Suspend)
                .await
                .unwrap();
            assert!(session.is_suspending());
            assert!(waiting.await.unwrap().unwrap().is_none());
            assert_snapshot_consistent(&controller);

            controller
                .unbind_executor(executor_id.clone())
                .await
                .unwrap();
            assert_snapshot_consistent(&controller);

            controller
                .unbind_executor_completed(executor_id.clone())
                .await
                .unwrap();
            let executor = controller.get_executor(executor_id).unwrap();
            assert_eq!(executor.state, ExecutorState::Idle);
            assert_eq!(executor.ssn_id, None);
            assert_snapshot_consistent(&controller);
        }

        #[tokio::test]
        async fn suspend_lets_running_task_finish() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;
            let executor_id = assign(&controller, "ssn", true).await;
            let task = controller
                .create_task("ssn".into(), None, None)
                .await
                .unwrap();
            let launched = controller.launch_task(executor_id.clone()).await.unwrap();
            assert_eq!(launched.unwrap().id, task.id);

            controller
                .signal_session("ssn".into(), Signal::Suspend)
                .await
                .unwrap();
            controller
                .complete_task(executor_id.clone(), succeed(), None)
                .await
                .unwrap();
            assert_eq!(
                controller.get_task("ssn".into(), task.id).unwrap().state,
                TaskState::Succeed
            );
            // The executor still serves the session until it unbinds.

            assert!(controller
                .launch_task(executor_id.clone())
                .await
                .unwrap()
                .is_none());
            unbind(&controller, &executor_id).await;
            assert_snapshot_consistent(&controller);
        }

        #[tokio::test]
        async fn tasks_submitted_while_suspended_run_after_resume() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;
            let executor_id = assign(&controller, "ssn", true).await;
            controller
                .signal_session("ssn".into(), Signal::Suspend)
                .await
                .unwrap();

            // Suspending: accepted, never handed out.
            let first = controller
                .create_task("ssn".into(), None, None)
                .await
                .unwrap();
            assert!(controller
                .launch_task(executor_id.clone())
                .await
                .unwrap()
                .is_none());
            unbind(&controller, &executor_id).await;
            controller.suspend_session("ssn".into()).await.unwrap();
            assert_eq!(state_of(&controller, "ssn"), SessionState::Suspended);

            // Suspended: accepted, and the session takes no executor.
            let second = controller
                .create_task("ssn".into(), None, None)
                .await
                .unwrap();
            assert!(matches!(
                controller
                    .bind_session(executor_id.clone(), "ssn".into())
                    .await,
                Err(FlameError::InvalidState(_))
            ));
            for id in [first.id, second.id] {
                assert_eq!(
                    controller.get_task("ssn".into(), id).unwrap().state,
                    TaskState::Pending
                );
            }

            controller.resume_session("ssn".into()).await.unwrap();
            controller
                .bind_session(executor_id.clone(), "ssn".into())
                .await
                .unwrap();
            controller
                .bind_executor_completed(executor_id.clone(), None, None)
                .await
                .unwrap();
            let task = controller
                .launch_task(executor_id)
                .await
                .unwrap()
                .expect("pending task runs after resume");
            assert_eq!(task.id, first.id);
        }

        #[tokio::test]
        async fn suspend_requires_an_open_session() {
            let controller = memory_controller().await;
            open(&controller, "closed").await;
            controller.close_session("closed".into()).await.unwrap();
            assert!(matches!(
                controller
                    .signal_session("closed".into(), Signal::Suspend)
                    .await,
                Err(FlameError::InvalidState(_))
            ));

            open(&controller, "suspended").await;
            controller
                .signal_session("suspended".into(), Signal::Suspend)
                .await
                .unwrap();
            assert!(matches!(
                controller
                    .signal_session("suspended".into(), Signal::Suspend)
                    .await,
                Err(FlameError::InvalidState(_))
            ));

            open(&controller, "suspending").await;
            assign(&controller, "suspending", true).await;
            controller
                .signal_session("suspending".into(), Signal::Suspend)
                .await
                .unwrap();
            assert!(matches!(
                controller
                    .signal_session("suspending".into(), Signal::Suspend)
                    .await,
                Err(FlameError::InvalidState(_))
            ));
            assert!(matches!(
                controller.resume_session("suspending".into()).await,
                Err(FlameError::InvalidState(_))
            ));
            assert!(matches!(
                controller.resume_session("closed".into()).await,
                Err(FlameError::InvalidState(_))
            ));
        }

        #[tokio::test]
        async fn resume_of_open_session_is_a_no_op() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;
            let session = controller.resume_session("ssn".into()).await.unwrap();
            assert!(session.is_open());
        }

        #[tokio::test]
        async fn binding_executor_delays_suspend_until_it_unbinds() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;
            let executor_id = assign(&controller, "ssn", false).await;

            let session = controller
                .signal_session("ssn".into(), Signal::Suspend)
                .await
                .unwrap();
            assert!(session.is_suspending());
            // A refused bind hands the executor back with no session.
            assert!(controller
                .wait_for_session(executor_id.clone())
                .await
                .unwrap()
                .is_none());
            assert_snapshot_consistent(&controller);

            controller
                .bind_executor_completed(executor_id.clone(), None, None)
                .await
                .unwrap();
            assert!(controller
                .launch_task(executor_id.clone())
                .await
                .unwrap()
                .is_none());
            unbind(&controller, &executor_id).await;
            assert_snapshot_consistent(&controller);
        }

        #[tokio::test]
        async fn suspend_session_ignores_a_session_that_is_not_suspending() {
            let controller = memory_controller().await;

            // Resumed or closed after the scheduler's check: nothing to complete.
            open(&controller, "open").await;
            controller.suspend_session("open".into()).await.unwrap();
            assert_eq!(state_of(&controller, "open"), SessionState::Open);

            open(&controller, "suspended").await;
            controller
                .signal_session("suspended".into(), Signal::Suspend)
                .await
                .unwrap();
            controller
                .suspend_session("suspended".into())
                .await
                .unwrap();
            controller
                .suspend_session("suspended".into())
                .await
                .unwrap();
            assert_eq!(state_of(&controller, "suspended"), SessionState::Suspended);

            open(&controller, "closed").await;
            controller.close_session("closed".into()).await.unwrap();
            controller.suspend_session("closed".into()).await.unwrap();
            assert_eq!(state_of(&controller, "closed"), SessionState::Closed);
        }

        #[tokio::test]
        async fn close_wins_over_suspending() {
            let controller = memory_controller().await;
            open(&controller, "ssn").await;
            assign(&controller, "ssn", true).await;
            controller
                .signal_session("ssn".into(), Signal::Suspend)
                .await
                .unwrap();

            controller.close_session("ssn".into()).await.unwrap();
            controller.suspend_session("ssn".into()).await.unwrap();
            assert_eq!(state_of(&controller, "ssn"), SessionState::Closed);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn snapshot_never_pairs_a_suspended_session_with_its_executor() {
            let controller = memory_controller().await;
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // Sessions and executors exist before the reader starts, so only the
            // suspend changes what the snapshot shows.
            let mut pairs = Vec::new();
            for index in 0..20 {
                let ssn_id = format!("ssn-{index}");
                open(&controller, &ssn_id).await;
                pairs.push((ssn_id.clone(), assign(&controller, &ssn_id, true).await));
            }
            let reader = {
                let controller = controller.clone();
                let done = done.clone();
                tokio::spawn(async move {
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        assert_snapshot_consistent(&controller);
                        tokio::task::yield_now().await;
                    }
                })
            };

            for (ssn_id, executor_id) in pairs {
                controller
                    .signal_session(ssn_id.clone(), Signal::Suspend)
                    .await
                    .unwrap();
                unbind(&controller, &executor_id).await;
                controller.suspend_session(ssn_id.clone()).await.unwrap();
                assert_eq!(state_of(&controller, &ssn_id), SessionState::Suspended);
            }
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            reader.await.unwrap();
        }
    }
}

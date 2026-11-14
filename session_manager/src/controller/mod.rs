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

/// A gang (`batch_size` above 1) is bound all at once and is the whole session: `min_instances`
/// keeps it warm and `max_instances` limits it, so each is the gang's size or not set. Any other
/// value would have the scheduler bind a part of the gang, or more executors than it. The
/// application's `max_instances` must hold the gang too. A session without a gang only needs a
/// maximum that holds its minimum.
fn validate_instance_limits(
    attr: &SessionAttributes,
    application: &Application,
) -> Result<(), FlameError> {
    let batch_size = attr.batch_size.max(1);
    if batch_size > 1 {
        ensure!(
            attr.min_instances == 0 || attr.min_instances == batch_size,
            FlameError::InvalidConfig(format!(
                "session <{}> min_instances ({}) must be 0 or batch_size ({batch_size})",
                attr.id, attr.min_instances
            ))
        )?;
        ensure!(
            attr.max_instances
                .is_none_or(|maximum| maximum == batch_size),
            FlameError::InvalidConfig(format!(
                "session <{}> max_instances ({:?}) must be unset or batch_size ({batch_size})",
                attr.id, attr.max_instances
            ))
        )?;
        ensure!(
            application.max_instances >= batch_size,
            FlameError::InvalidConfig(format!(
                "session <{}> batch_size ({batch_size}) is more than max_instances ({}) of application <{}>",
                attr.id, application.max_instances, application.name
            ))
        )?;
    }
    if let Some(maximum) = attr.max_instances {
        ensure!(
            maximum >= attr.min_instances,
            FlameError::InvalidConfig(format!(
                "session <{}> max_instances ({maximum}) is less than min_instances ({})",
                attr.id, attr.min_instances
            ))
        )?;
    }
    Ok(())
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

    /// Releases an inactive, executor-free node and removes it from storage.
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
        let application = self.application_enabled(&attr.application).await?;
        validate_instance_limits(&attr, &application)?;
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
        let application = self.application_enabled(&application).await?;
        if let Some(attr) = spec.as_ref() {
            validate_instance_limits(attr, &application)?;
        }
        self.storage.open_session(id, spec).await
    }

    async fn application_enabled(&self, name: &str) -> Result<Application, FlameError> {
        let application = self.storage.get_application(name.to_string()).await?;
        if application.state != ApplicationState::Enabled {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> is disabled"
            )));
        }
        Ok(application)
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

    pub async fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        self.storage.get_task(ssn_id, id).await
    }

    pub async fn get_task_metadata(
        &self,
        ssn_id: SessionID,
        id: TaskID,
    ) -> Result<Task, FlameError> {
        self.storage.get_task_metadata(ssn_id, id).await
    }

    pub async fn list_tasks(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        self.storage.list_tasks(ssn_id).await
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

    fn unassign_task(executor: &ExecutorPtr, expected: &TaskGID) -> Result<(), FlameError> {
        let mut executor = lock_ptr!(executor)?;
        ensure!(
            executor.ssn_id.as_ref() == Some(&expected.ssn_id)
                && executor.task_id == Some(expected.task_id),
            FlameError::InvalidState(format!(
                "Executor <{}> no longer owns task <{}>",
                executor.id, expected
            ))
        )?;
        executor.task_id = None;
        if executor.state == ExecutorState::Unbinding {
            executor.ssn_id = None;
        }
        Ok(())
    }

    pub async fn launch_task(&self, id: ExecutorID) -> Result<Option<Task>, FlameError> {
        trace_fn!("Controller::launch_task");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
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
            let gid = TaskGID { ssn_id, task_id };
            return match self.storage.get_task_ptr(gid.clone()) {
                Ok(task) => {
                    let task = { lock_ptr!(task)?.clone() };
                    Ok(Some(task))
                }
                Err(FlameError::NotFound(_)) => {
                    Self::unassign_task(&exe_ptr, &gid)?;
                    let executor = { lock_ptr!(exe_ptr)?.clone() };
                    self.storage.update_executor(&executor).await?;
                    Ok(None)
                }
                Err(error) => Err(error),
            };
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
        let task = match self.storage.get_task_ptr(gid.clone()) {
            Ok(task) => Some(task),
            Err(FlameError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };

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

        if task.is_none() {
            Self::unassign_task(&exe_ptr, &gid)?;
        }
        if let Some(attributes) = attributes {
            self.update_executor_attributes(&id, attributes)?;
        }
        if let Some(task) = task {
            let ssn_ptr = self.storage.get_session_ptr(ssn_id.clone())?;
            let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
            state.complete_task(ssn_ptr, task, task_result).await?;
        }
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
            let task = match self.storage.get_task_ptr(TaskGID {
                ssn_id: ssn_id.clone(),
                task_id,
            }) {
                Ok(task) => Some(task),
                Err(FlameError::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            if let Some(task) = task {
                let ssn_ptr = self.storage.get_session_ptr(ssn_id.clone())?;
                self.storage
                    .update_task_state(
                        ssn_ptr,
                        task,
                        TaskState::Failed,
                        Some(format!(
                            "Executor <{}> was lost; its service was destroyed without retry",
                            executor.id
                        )),
                    )
                    .await?;
                let _ = self.notifier.tasks.notify(ssn_id, task_id);
            }
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
mod tests;

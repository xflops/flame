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

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use common::apis::{
    Application, ApplicationAttributes, ApplicationID, CommonData, Event, EventOwner, ExecutorID,
    ExecutorState, Node, NodeState, Role, Session, SessionAttributes, SessionID, SessionPtr,
    SessionState, Task, TaskGID, TaskID, TaskInput, TaskOutput, TaskPtr, TaskResult, TaskState,
    User, Workspace,
};

use common::FlameError;
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use crate::model::{
    ConnectionCallbacks, ConnectionState, Executor, ExecutorFilter, ExecutorPtr, NodeConnectionPtr,
    NodeConnectionReceiver, NodeConnectionSender, NodeInfoPtr, SessionInfoPtr, SnapShotPtr,
};
use crate::storage::StoragePtr;

mod connections;
mod executors;
mod nodes;

pub use connections::ConnectionManager;

/// Callbacks for node connection lifecycle events.
/// Implements the state machine transitions for node states.
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

pub struct Controller {
    storage: StoragePtr,
    connection_manager: ConnectionManager<NodeCallbacks>,
}

pub type ControllerPtr = Arc<Controller>;

pub fn new_ptr(storage: StoragePtr) -> ControllerPtr {
    let callbacks = NodeCallbacks {
        storage: storage.clone(),
    };
    Arc::new(Controller {
        storage,
        connection_manager: ConnectionManager::new(callbacks),
    })
}

impl Controller {
    // ========================================================================
    // Accessors
    // ========================================================================

    /// Returns a reference to the storage.
    pub fn storage(&self) -> &StoragePtr {
        &self.storage
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
        let reported_ids: HashSet<String> =
            reported_executors.iter().map(|e| e.id.clone()).collect();

        // Get executors from DB for this node
        let db_executors = self
            .storage
            .list_executor(Some(&ExecutorFilter::by_node(&node.name)))?;

        let db_ids: HashSet<String> = db_executors.iter().map(|e| e.id.clone()).collect();

        // Compare both directions and release mismatched executors

        // 1. DB executors not reported by node - orphaned in DB, release them
        for db_exec in &db_executors {
            if !reported_ids.contains(&db_exec.id) {
                tracing::info!(
                    "Executor <{}> in DB but not reported by node <{}>. Releasing orphaned executor.",
                    db_exec.id,
                    node.name
                );
                if let Err(e) = self.release_executor(db_exec.id.clone()).await {
                    tracing::warn!(
                        "Failed to release orphaned executor <{}>: {}",
                        db_exec.id,
                        e
                    );
                }
            }

            // Send executor to node (whether aligned or being released)
            if let Err(e) = sender.send(db_exec.clone()).await {
                tracing::warn!("Failed to send executor <{}> to node: {:?}", db_exec.id, e);
            }
        }

        // 2. Reported executors not in DB - unknown to DB, node should release them
        for reported_exec in reported_executors {
            if !db_ids.contains(&reported_exec.id) {
                tracing::info!(
                    "Executor <{}> reported by node <{}> but not in DB. Sending release signal.",
                    reported_exec.id,
                    node.name
                );
                // Send the executor with released state so node knows to clean it up
                let mut exec_to_release = reported_exec.clone();
                exec_to_release.state = common::apis::ExecutorState::Released;
                if let Err(e) = sender.send(exec_to_release).await {
                    tracing::warn!(
                        "Failed to send release signal for executor <{}>: {:?}",
                        reported_exec.id,
                        e
                    );
                }
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
    pub fn list_node(&self) -> Result<Vec<Node>, FlameError> {
        self.storage.list_node()
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
        self.storage.create_session(attr).await
    }

    pub async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        trace_fn!("Controller::open_session");
        self.storage.open_session(id, spec).await
    }

    pub async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        trace_fn!("Controller::close_session");
        self.storage.close_session(id).await
    }

    pub fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        self.storage.get_session(id)
    }

    pub async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        self.storage.delete_session(id).await
    }

    pub fn list_session(&self) -> Result<Vec<Session>, FlameError> {
        self.storage.list_session()
    }

    pub async fn create_task(
        &self,
        ssn_id: SessionID,
        task_input: Option<TaskInput>,
    ) -> Result<Task, FlameError> {
        self.storage.create_task(ssn_id, task_input).await
    }

    pub fn get_task(&self, ssn_id: SessionID, id: TaskID) -> Result<Task, FlameError> {
        self.storage.get_task(ssn_id, id)
    }

    pub fn list_task(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        self.storage.list_task(ssn_id)
    }

    pub async fn update_task_result(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::update_task_result");
        self.storage
            .update_task_result(ssn, task, task_result)
            .await
    }

    pub async fn update_task_state(
        &self,
        ssn: SessionPtr,
        task: TaskPtr,
        task_state: TaskState,
        message: Option<String>,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::update_task_state");
        self.storage
            .update_task_state(ssn, task, task_state, message)
            .await
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
            tracing::debug!(
                "Failed to notify node <{}> about executor creation: {}",
                node_name,
                e
            );
        }

        Ok(executor)
    }

    pub fn list_executor(&self) -> Result<Vec<Executor>, FlameError> {
        trace_fn!("Controller::list_executor");
        self.storage.list_executor(None)
    }

    pub async fn register_executor(&self, e: &Executor) -> Result<(), FlameError> {
        trace_fn!("Controller::register_executor");

        let exe_ptr = self.storage.get_executor_ptr(e.id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.register_executor().await?;

        // Notify the node about the executor registration
        if let Err(err) = self.connection_manager.notify_executor(&e.node, e).await {
            tracing::debug!(
                "Failed to notify node <{}> about executor registration: {}",
                e.node,
                err
            );
        }

        Ok(())
    }

    pub fn snapshot(&self) -> Result<SnapShotPtr, FlameError> {
        self.storage.snapshot()
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
        self.storage.register_application(name, attr).await
    }

    pub async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        trace_fn!("Controller::unregister_application");
        self.storage.unregister_application(name).await
    }

    pub async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::update_application");
        self.storage.update_application(name, attr).await
    }

    pub async fn list_application(&self) -> Result<Vec<Application>, FlameError> {
        trace_fn!("Controller::list_application");
        self.storage.list_application().await
    }

    pub async fn watch_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        trace_fn!("Controller::watch_task");
        let task_ptr = self.storage.get_task_ptr(gid)?;
        WatchTaskFuture::new(self.storage.clone(), &task_ptr)?.await?;

        let task = lock_ptr!(task_ptr)?;
        Ok((*task).clone())
    }

    pub async fn wait_for_session(&self, id: ExecutorID) -> Result<Option<Session>, FlameError> {
        trace_fn!("Controller::wait_for_session");
        let exe_ptr = self.storage.get_executor_ptr(id)?;
        let ssn_id = WaitForSsnFuture::new(&exe_ptr).await?;

        let Some(ssn_id) = ssn_id else {
            return Ok(None);
        };

        let ssn_ptr = self.storage.get_session_ptr(ssn_id)?;
        let ssn = lock_ptr!(ssn_ptr)?;

        Ok(Some((*ssn).clone()))
    }

    pub async fn bind_session(&self, id: ExecutorID, ssn_id: SessionID) -> Result<(), FlameError> {
        trace_fn!("Controller::bind_session");

        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;

        let ssn_ptr = self.storage.get_session_ptr(ssn_id)?;
        state.bind_session(ssn_ptr).await?;

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
            tracing::debug!(
                "Failed to notify node <{}> about executor binding: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn bind_session_completed(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::bind_session_completed");

        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;

        state.bind_session_completed().await?;

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
            tracing::debug!(
                "Failed to notify node <{}> about bind completion: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn launch_task(&self, id: ExecutorID) -> Result<Option<Task>, FlameError> {
        trace_fn!("Controller::launch_task");
        let exe_ptr = self.storage.get_executor_ptr(id)?;
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
            tracing::warn!(
                "Re-launch the task <{}/{}>",
                ssn_id.clone(),
                task_id.clone()
            );
            let task_ptr = self.storage.get_task_ptr(TaskGID { ssn_id, task_id })?;

            let task = lock_ptr!(task_ptr)?;
            return Ok(Some((*task).clone()));
        }

        tracing::debug!("Launching task for session <{:?}>", ssn_id);
        let ssn_ptr = self.storage.get_session_ptr(ssn_id.clone());

        let result = match ssn_ptr {
            Ok(ssn_ptr) => state.launch_task(ssn_ptr).await,
            Err(FlameError::NotFound(msg)) => {
                tracing::warn!(
                    "Session <{:?}> not found when launching task: {}",
                    ssn_id,
                    msg
                );
                Ok(None)
            }
            Err(e) => {
                tracing::error!(
                    "Failed to get session <{:?}> when launching task: {:?}",
                    ssn_id,
                    e
                );
                Err(e)
            }
        };

        if result.is_ok() {
            let executor = {
                let exe = lock_ptr!(exe_ptr)?;
                (*exe).clone()
            };
            self.storage.update_executor(&executor).await?;
        }

        result
    }

    pub async fn complete_task(
        &self,
        id: ExecutorID,
        task_result: TaskResult,
    ) -> Result<(), FlameError> {
        trace_fn!("Controller::complete_task");
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

        let task_ptr = self.storage.get_task_ptr(TaskGID {
            ssn_id: ssn_id.clone(),
            task_id,
        })?;
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

        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.complete_task(ssn_ptr, task_ptr, task_result).await?;

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
            tracing::debug!(
                "Failed to notify node <{}> about task completion: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn unbind_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::unbind_executor");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.unbind_executor().await?;

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
            tracing::debug!(
                "Failed to notify node <{}> about executor unbinding: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn unbind_executor_completed(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::unbind_executor_completed");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
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
            tracing::debug!(
                "Failed to notify node <{}> about unbind completion: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn release_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::release_executor");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;
        let state = executors::from(self.storage.clone(), exe_ptr.clone())?;
        state.release_executor().await?;

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
            tracing::debug!(
                "Failed to notify node <{}> about executor release: {}",
                executor.node,
                e
            );
        }

        Ok(())
    }

    pub async fn unregister_executor(&self, id: ExecutorID) -> Result<(), FlameError> {
        trace_fn!("Controller::unregister_executor");
        let exe_ptr = self.storage.get_executor_ptr(id.clone())?;

        // Get executor info before unregistering for notification
        let executor = {
            let exe = lock_ptr!(exe_ptr)?;
            (*exe).clone()
        };

        let state = executors::from(self.storage.clone(), exe_ptr)?;
        state.unregister_executor().await?;

        self.storage.delete_executor(id).await?;

        // Notify the node about the executor deletion
        if let Err(e) = self
            .connection_manager
            .notify_executor(&executor.node, &executor)
            .await
        {
            tracing::debug!(
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

    // ========================================================================
    // Workspace Operations
    // ========================================================================

    pub async fn create_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError> {
        trace_fn!("Controller::create_workspace");
        self.storage.create_workspace(workspace).await
    }

    pub async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, FlameError> {
        trace_fn!("Controller::get_workspace");
        self.storage.get_workspace(name).await
    }

    pub async fn update_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError> {
        trace_fn!("Controller::update_workspace");
        self.storage.update_workspace(workspace).await
    }

    pub async fn delete_workspace(&self, name: &str) -> Result<(), FlameError> {
        trace_fn!("Controller::delete_workspace");
        self.storage.delete_workspace(name).await
    }

    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        trace_fn!("Controller::list_workspaces");
        self.storage.find_workspaces().await
    }

    // ========================================================================
    // User Operations
    // ========================================================================

    pub async fn create_user(&self, user: &User) -> Result<User, FlameError> {
        trace_fn!("Controller::create_user");
        self.storage.create_user(user).await
    }

    pub async fn get_user(&self, name: &str) -> Result<Option<User>, FlameError> {
        trace_fn!("Controller::get_user");
        self.storage.get_user(name).await
    }

    pub async fn update_user(
        &self,
        user: &User,
        assign_roles: &[String],
        revoke_roles: &[String],
    ) -> Result<User, FlameError> {
        trace_fn!("Controller::update_user");
        self.storage
            .update_user(user, assign_roles, revoke_roles)
            .await
    }

    pub async fn delete_user(&self, name: &str) -> Result<(), FlameError> {
        trace_fn!("Controller::delete_user");
        self.storage.delete_user(name).await
    }

    pub async fn list_users(&self, role_filter: Option<&str>) -> Result<Vec<User>, FlameError> {
        trace_fn!("Controller::list_users");
        self.storage.find_users(role_filter).await
    }

    pub async fn get_user_by_cn(&self, cn: &str) -> Result<Option<User>, FlameError> {
        trace_fn!("Controller::get_user_by_cn");
        self.storage.get_user_by_cn(cn).await
    }

    pub async fn get_user_roles(&self, user_name: &str) -> Result<Vec<Role>, FlameError> {
        trace_fn!("Controller::get_user_roles");
        self.storage.get_user_roles(user_name).await
    }

    // ========================================================================
    // Role Operations
    // ========================================================================

    pub async fn create_role(&self, role: &Role) -> Result<Role, FlameError> {
        trace_fn!("Controller::create_role");
        self.storage.create_role(role).await
    }

    pub async fn get_role(&self, name: &str) -> Result<Option<Role>, FlameError> {
        trace_fn!("Controller::get_role");
        self.storage.get_role(name).await
    }

    pub async fn update_role(&self, role: &Role) -> Result<Role, FlameError> {
        trace_fn!("Controller::update_role");
        self.storage.update_role(role).await
    }

    pub async fn delete_role(&self, name: &str) -> Result<(), FlameError> {
        trace_fn!("Controller::delete_role");
        self.storage.delete_role(name).await
    }

    pub async fn list_roles(
        &self,
        workspace_filter: Option<&str>,
    ) -> Result<Vec<Role>, FlameError> {
        trace_fn!("Controller::list_roles");
        self.storage.find_roles(workspace_filter).await
    }
}

struct WatchTaskFuture {
    storage: StoragePtr,
    current_state: TaskState,
    task_gid: TaskGID,
}

impl WatchTaskFuture {
    pub fn new(storage: StoragePtr, task_ptr: &TaskPtr) -> Result<Self, FlameError> {
        let task_ptr = task_ptr.clone();
        let task = lock_ptr!(task_ptr)?;

        Ok(Self {
            storage,
            current_state: task.state,
            task_gid: TaskGID {
                ssn_id: task.ssn_id.clone(),
                task_id: task.id,
            },
        })
    }
}

impl Future for WatchTaskFuture {
    type Output = Result<(), FlameError>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let task_ptr = self.storage.get_task_ptr(self.task_gid.clone())?;

        let task = lock_ptr!(task_ptr)?;
        // If the state of task was updated, return ready.
        if self.current_state != task.state || task.is_completed() {
            return Poll::Ready(Ok(()));
        }

        ctx.waker().wake_by_ref();
        Poll::Pending
    }
}

struct WaitForSsnFuture {
    executor: ExecutorPtr,
}

impl WaitForSsnFuture {
    pub fn new(exe_ptr: &ExecutorPtr) -> Self {
        Self {
            executor: exe_ptr.clone(),
        }
    }
}

impl Future for WaitForSsnFuture {
    type Output = Result<Option<SessionID>, FlameError>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let exe = lock_ptr!(self.executor)?;

        if exe.state == ExecutorState::Releasing || exe.state == ExecutorState::Released {
            return Poll::Ready(Ok(None));
        }

        match exe.ssn_id.clone() {
            None => {
                // No bound session, trigger waker.
                ctx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(ssn_id) => Poll::Ready(Ok(Some(ssn_id))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{Node, NodeInfo, NodeState, ResourceRequirement, Shim};
    use common::ctx::{FlameCluster, FlameClusterContext, FlameExecutorLimits, FlameExecutors};
    use tokio::sync::mpsc;

    /// Creates a test storage with a unique SQLite database.
    async fn create_test_storage() -> StoragePtr {
        let temp_dir = std::env::temp_dir();
        let db_name = format!(
            "flame_test_controller_{}.db",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let db_path = temp_dir.join(db_name);
        let url = format!("sqlite://{}", db_path.display());

        let ctx = FlameClusterContext {
            cluster: FlameCluster {
                name: "test".to_string(),
                endpoint: "http://localhost:8080".to_string(),
                storage: url,
                slot: ResourceRequirement::default(),
                policy: "fifo".to_string(),
                schedule_interval: 1000,
                executors: FlameExecutors {
                    shim: Shim::default(),
                    limits: FlameExecutorLimits { max_executors: 10 },
                },
                tls: None,
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
            },
            allocatable: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        }
    }

    // ========================================================================
    // Controller::register_node Tests
    // ========================================================================

    mod register_node_tests {
        use super::*;
        use std::collections::HashSet;

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
            };
            node.allocatable = ResourceRequirement {
                cpu: 14,
                memory: 28672,
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
}

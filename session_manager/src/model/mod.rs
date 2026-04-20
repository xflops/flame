pub mod connection;

pub use connection::{
    ConnectionCallbacks, ConnectionState, NodeConnection, NodeConnectionPtr,
    NodeConnectionReceiver, NodeConnectionSender, DEFAULT_DRAIN_TIMEOUT_SECS,
};

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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use stdng::{lock_ptr, MutexPtr};

use common::apis::{
    Application, ExecutorID, ExecutorState, Node, NodeState, ResourceRequirement, Session,
    SessionID, SessionState, Shim, Task, TaskID, TaskState,
};
use common::FlameError;
use rpc::flame::v1 as rpc;

pub type SessionInfoPtr = Arc<SessionInfo>;
pub type ExecutorInfoPtr = Arc<ExecutorInfo>;
pub type NodeInfoPtr = Arc<NodeInfo>;
pub type AppInfoPtr = Arc<AppInfo>;

#[derive(Clone)]
pub struct SnapShot {
    pub unit: ResourceRequirement,
    pub applications: MutexPtr<HashMap<String, AppInfoPtr>>,

    pub sessions: MutexPtr<HashMap<SessionID, SessionInfoPtr>>,
    pub ssn_index: MutexPtr<HashMap<SessionState, HashMap<SessionID, SessionInfoPtr>>>,

    pub executors: MutexPtr<HashMap<ExecutorID, ExecutorInfoPtr>>,
    pub exec_index: MutexPtr<HashMap<ExecutorState, HashMap<ExecutorID, ExecutorInfoPtr>>>,

    pub nodes: MutexPtr<HashMap<String, NodeInfoPtr>>,
}

pub type SnapShotPtr = Arc<SnapShot>;

impl SnapShot {
    pub fn new(unit: ResourceRequirement) -> Self {
        SnapShot {
            unit,
            applications: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            ssn_index: Arc::new(Mutex::new(HashMap::new())),
            executors: Arc::new(Mutex::new(HashMap::new())),
            exec_index: Arc::new(Mutex::new(HashMap::new())),
            nodes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn debug(&self) -> Result<(), FlameError> {
        if tracing::enabled!(tracing::Level::DEBUG) {
            let ssn_num = {
                let ssns = lock_ptr!(self.sessions)?;
                ssns.len()
            };
            let exe_num = {
                let exes = lock_ptr!(self.executors)?;
                exes.len()
            };

            let state_counts = {
                let exec_index = lock_ptr!(self.exec_index)?;
                let mut counts = std::collections::HashMap::new();
                for (state, execs) in exec_index.iter() {
                    counts.insert(*state, execs.len());
                }
                counts
            };

            tracing::debug!(
                "Session: <{ssn_num}>, Executor: <{exe_num}>, States: {:?}",
                state_counts
            );
        }

        Ok(())
    }

    /// Get the application info for a session by looking up the application name.
    pub fn get_application(&self, app_name: &str) -> Result<Option<AppInfoPtr>, FlameError> {
        let apps = lock_ptr!(self.applications)?;
        Ok(apps.get(app_name).cloned())
    }
}

#[derive(Debug, Default, Clone)]
pub struct TaskInfo {
    pub id: TaskID,
    pub ssn_id: SessionID,

    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,

    pub state: TaskState,
}

#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,

    pub tasks_status: HashMap<TaskState, i32>,

    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,

    pub state: SessionState,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ExecutorInfo {
    pub id: ExecutorID,
    pub node: String,
    pub resreq: ResourceRequirement,
    pub slots: u32,
    pub shim: Shim,
    pub task_id: Option<TaskID>,
    pub ssn_id: Option<SessionID>,

    pub creation_time: DateTime<Utc>,
    pub state: ExecutorState,
}

#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    pub name: String,
    pub allocatable: ResourceRequirement,
    pub state: NodeState,
}

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub name: String,
    pub shim: Shim, // Required shim type for the application
    pub max_instances: u32,
    pub delay_release: Duration,
}

impl From<Application> for AppInfo {
    fn from(app: Application) -> Self {
        AppInfo::from(&app)
    }
}

impl From<&Node> for NodeInfo {
    fn from(node: &Node) -> Self {
        NodeInfo {
            name: node.name.clone(),
            allocatable: node.allocatable.clone(),
            state: node.state,
        }
    }
}

impl From<&Application> for AppInfo {
    fn from(app: &Application) -> Self {
        AppInfo {
            name: app.name.to_string(),
            shim: app.shim, // Get shim from application
            max_instances: app.max_instances,
            delay_release: app.delay_release,
        }
    }
}

impl From<&Executor> for ExecutorInfo {
    fn from(exec: &Executor) -> Self {
        ExecutorInfo {
            id: exec.id.clone(),
            node: exec.node.clone(),
            resreq: exec.resreq.clone(),
            slots: exec.slots,
            shim: exec.shim,
            task_id: exec.task_id,
            ssn_id: exec.ssn_id.clone(),
            creation_time: exec.creation_time,
            state: exec.state,
        }
    }
}

impl From<&Task> for TaskInfo {
    fn from(task: &Task) -> Self {
        TaskInfo {
            id: task.id,
            ssn_id: task.ssn_id.clone(),
            creation_time: task.creation_time,
            completion_time: task.completion_time,
            state: task.state,
        }
    }
}

impl From<&Session> for SessionInfo {
    fn from(ssn: &Session) -> Self {
        let mut tasks_status = HashMap::new();
        for (k, v) in &ssn.tasks_index {
            tasks_status.insert(*k, v.len() as i32);
        }

        SessionInfo {
            id: ssn.id.clone(),
            application: ssn.application.clone(),
            slots: ssn.slots,
            tasks_status,
            creation_time: ssn.creation_time,
            completion_time: ssn.completion_time,
            state: ssn.status.state,
            min_instances: ssn.min_instances,
            max_instances: ssn.max_instances,
            batch_size: ssn.batch_size.max(1),
        }
    }
}

/// Filter for listing sessions.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec matches nothing)
pub struct SessionFilter {
    /// Filter by session state
    pub state: Option<SessionState>,
    /// Filter by session IDs
    pub ids: Option<Vec<SessionID>>,
}

impl SessionFilter {
    /// Creates a new empty filter (matches all sessions).
    pub const fn new() -> Self {
        Self {
            state: None,
            ids: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: SessionState) -> Self {
        Self {
            state: Some(state),
            ids: None,
        }
    }

    /// Creates a filter for specific session IDs.
    pub fn by_ids(ids: Vec<SessionID>) -> Self {
        Self {
            state: None,
            ids: Some(ids),
        }
    }
}

impl Default for SessionFilter {
    fn default() -> Self {
        Self::new()
    }
}

pub const OPEN_SESSION: Option<SessionFilter> = Some(SessionFilter::by_state(SessionState::Open));

/// Filter for listing executors.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec/string matches nothing)
pub struct ExecutorFilter {
    /// Filter by executor state
    pub state: Option<ExecutorState>,
    /// Filter by executor IDs
    pub ids: Option<Vec<ExecutorID>>,
    /// Filter by node name
    pub node: Option<String>,
}

impl ExecutorFilter {
    /// Creates a new empty filter (matches all executors).
    pub const fn new() -> Self {
        Self {
            state: None,
            ids: None,
            node: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: ExecutorState) -> Self {
        Self {
            state: Some(state),
            ids: None,
            node: None,
        }
    }

    /// Creates a filter for a specific node.
    pub fn by_node(node: impl Into<String>) -> Self {
        Self {
            state: None,
            ids: None,
            node: Some(node.into()),
        }
    }

    /// Creates a filter for specific executor IDs.
    pub fn by_ids(ids: Vec<ExecutorID>) -> Self {
        Self {
            state: None,
            ids: Some(ids),
            node: None,
        }
    }
}

impl Default for ExecutorFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Filter for listing nodes.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec matches nothing)
pub struct NodeFilter {
    /// Filter by node state
    pub state: Option<NodeState>,
    /// Filter by node names
    pub names: Option<Vec<String>>,
}

impl NodeFilter {
    /// Creates a new empty filter (matches all nodes).
    pub const fn new() -> Self {
        Self {
            state: None,
            names: None,
        }
    }

    /// Creates a filter for a specific state.
    pub const fn by_state(state: NodeState) -> Self {
        Self {
            state: Some(state),
            names: None,
        }
    }

    /// Creates a filter for specific node names.
    pub fn by_names(names: Vec<String>) -> Self {
        Self {
            state: None,
            names: Some(names),
        }
    }
}

impl Default for NodeFilter {
    fn default() -> Self {
        Self::new()
    }
}

pub const ALL_NODE: Option<NodeFilter> = None;

pub const IDLE_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Idle));
pub const VOID_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Void));
pub const UNBINDING_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Unbinding));
pub const BOUND_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Bound));
pub const BINDING_EXECUTOR: Option<ExecutorFilter> =
    Some(ExecutorFilter::by_state(ExecutorState::Binding));

pub const ALL_EXECUTOR: Option<ExecutorFilter> = None;

/// Filter for listing applications.
/// All fields are Option:
/// - `None` = ignore this filter (match all)
/// - `Some(value)` = match exactly (empty vec matches nothing)
pub struct AppFilter {
    /// Filter by application names
    pub names: Option<Vec<String>>,
}

impl AppFilter {
    /// Creates a new empty filter (matches all applications).
    pub const fn new() -> Self {
        Self { names: None }
    }

    /// Creates a filter for specific application names.
    pub fn by_names(names: Vec<String>) -> Self {
        Self { names: Some(names) }
    }
}

impl Default for AppFilter {
    fn default() -> Self {
        Self::new()
    }
}

pub const ALL_APPLICATION: Option<AppFilter> = None;

impl SnapShot {
    pub fn find_nodes(
        &self,
        filter: Option<NodeFilter>,
    ) -> Result<HashMap<String, NodeInfoPtr>, FlameError> {
        match filter {
            Some(filter) => self.find_nodes_by_filter(filter),
            None => self.find_all_nodes(),
        }
    }

    fn find_nodes_by_filter(
        &self,
        filter: NodeFilter,
    ) -> Result<HashMap<String, NodeInfoPtr>, FlameError> {
        let nodes_list = lock_ptr!(self.nodes)?;

        // Start with all nodes
        let candidates: Vec<NodeInfoPtr> = nodes_list.values().cloned().collect();

        // Apply state filter if specified
        let filtered: Vec<NodeInfoPtr> = match filter.state {
            None => candidates,
            Some(state) => candidates
                .into_iter()
                .filter(|node| node.state == state)
                .collect(),
        };

        // Apply names filter if specified
        let filtered: Vec<NodeInfoPtr> = match filter.names {
            None => filtered,
            Some(ref names) => filtered
                .into_iter()
                .filter(|node| names.contains(&node.name))
                .collect(),
        };

        Ok(filtered
            .into_iter()
            .map(|node| (node.name.clone(), node))
            .collect())
    }

    fn find_all_nodes(&self) -> Result<HashMap<String, NodeInfoPtr>, FlameError> {
        let mut nodes = HashMap::new();

        {
            let nodes_list = lock_ptr!(self.nodes)?;

            for node in nodes_list.values() {
                nodes.insert(node.name.clone(), node.clone());
            }
        }

        Ok(nodes)
    }

    pub fn find_applications(
        &self,
        filter: Option<AppFilter>,
    ) -> Result<HashMap<String, AppInfoPtr>, FlameError> {
        match filter {
            Some(filter) => self.find_applications_by_filter(filter),
            None => self.find_all_applications(),
        }
    }

    fn find_applications_by_filter(
        &self,
        filter: AppFilter,
    ) -> Result<HashMap<String, AppInfoPtr>, FlameError> {
        let apps = lock_ptr!(self.applications)?;

        // Apply names filter if specified
        let filtered: Vec<AppInfoPtr> = match filter.names {
            None => apps.values().cloned().collect(),
            Some(ref names) => apps
                .values()
                .filter(|app| names.contains(&app.name))
                .cloned()
                .collect(),
        };

        Ok(filtered
            .into_iter()
            .map(|app| (app.name.clone(), app))
            .collect())
    }

    fn find_all_applications(&self) -> Result<HashMap<String, AppInfoPtr>, FlameError> {
        let mut appinfos = HashMap::new();

        {
            let apps = lock_ptr!(self.applications)?;

            for app in apps.values() {
                appinfos.insert(app.name.clone(), app.clone());
            }
        }

        Ok(appinfos)
    }

    pub fn find_sessions(
        &self,
        filter: Option<SessionFilter>,
    ) -> Result<HashMap<SessionID, SessionInfoPtr>, FlameError> {
        match filter {
            Some(filter) => self.find_sessions_by_filter(filter),
            None => self.find_all_sessions(),
        }
    }

    fn find_sessions_by_filter(
        &self,
        filter: SessionFilter,
    ) -> Result<HashMap<SessionID, SessionInfoPtr>, FlameError> {
        let sessions = lock_ptr!(self.sessions)?;
        let ssn_index = lock_ptr!(self.ssn_index)?;

        // Start with all sessions or sessions matching state filter
        let candidates: Vec<SessionInfoPtr> = match filter.state {
            None => sessions.values().cloned().collect(),
            Some(state) => ssn_index
                .get(&state)
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default(),
        };

        // Apply ids filter if specified
        let filtered: Vec<SessionInfoPtr> = match filter.ids {
            None => candidates,
            Some(ref ids) => candidates
                .into_iter()
                .filter(|ssn| ids.contains(&ssn.id))
                .collect(),
        };

        Ok(filtered
            .into_iter()
            .map(|ssn| (ssn.id.clone(), ssn))
            .collect())
    }

    fn find_all_sessions(&self) -> Result<HashMap<SessionID, SessionInfoPtr>, FlameError> {
        let mut ssns = HashMap::new();

        {
            let sessions = lock_ptr!(self.sessions)?;

            for ssn in sessions.values() {
                ssns.insert(ssn.id.clone(), ssn.clone());
            }
        }

        Ok(ssns)
    }

    pub fn add_node(&self, node: NodeInfoPtr) -> Result<(), FlameError> {
        {
            let mut nodes = lock_ptr!(self.nodes)?;
            nodes.insert(node.name.clone(), node.clone());
        }

        Ok(())
    }

    pub fn add_session(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        {
            let mut sessions = lock_ptr!(self.sessions)?;
            sessions.insert(ssn.id.clone(), ssn.clone());
        }

        {
            let mut ssn_index = lock_ptr!(self.ssn_index)?;
            ssn_index.entry(ssn.state).or_default();

            if let Some(ssn_list) = ssn_index.get_mut(&ssn.state) {
                ssn_list.insert(ssn.id.clone(), ssn.clone());
            }
        }

        Ok(())
    }

    pub fn add_application(&self, app: AppInfoPtr) -> Result<(), FlameError> {
        {
            let mut apps = lock_ptr!(self.applications)?;
            apps.insert(app.name.clone(), app.clone());
        }

        Ok(())
    }

    pub fn get_session(&self, id: &SessionID) -> Result<SessionInfoPtr, FlameError> {
        let sessions = lock_ptr!(self.sessions)?;
        match sessions.get(id) {
            Some(ptr) => Ok(ptr.clone()),
            None => Err(FlameError::NotFound(format!("session <{id}> not found"))),
        }
    }

    pub fn delete_session(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        {
            let mut sessions = lock_ptr!(self.sessions)?;
            sessions.remove(&ssn.id.clone());
        }

        {
            let mut ssn_index = lock_ptr!(self.ssn_index)?;
            for ssn_list in &mut ssn_index.values_mut() {
                ssn_list.remove(&ssn.id.clone());
            }
        }

        Ok(())
    }

    pub fn update_session(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        self.delete_session(ssn.clone())?;
        self.add_session(ssn)?;

        Ok(())
    }

    pub fn find_executors(
        &self,
        filter: Option<ExecutorFilter>,
    ) -> Result<HashMap<ExecutorID, ExecutorInfoPtr>, FlameError> {
        match filter {
            Some(filter) => self.find_executors_by_filter(filter),
            None => self.find_all_executors(),
        }
    }

    fn find_executors_by_filter(
        &self,
        filter: ExecutorFilter,
    ) -> Result<HashMap<ExecutorID, ExecutorInfoPtr>, FlameError> {
        let executors = lock_ptr!(self.executors)?;
        let exec_index = lock_ptr!(self.exec_index)?;

        // Start with all executors or executors matching state filter
        let candidates: Vec<ExecutorInfoPtr> = match filter.state {
            None => executors.values().cloned().collect(),
            Some(state) => exec_index
                .get(&state)
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default(),
        };

        // Apply ids filter if specified
        let filtered: Vec<ExecutorInfoPtr> = match filter.ids {
            None => candidates,
            Some(ref ids) => candidates
                .into_iter()
                .filter(|exec| ids.contains(&exec.id))
                .collect(),
        };

        // Apply node filter if specified
        let filtered: Vec<ExecutorInfoPtr> = match filter.node {
            None => filtered,
            Some(ref node_name) => filtered
                .into_iter()
                .filter(|exec| &exec.node == node_name)
                .collect(),
        };

        Ok(filtered
            .into_iter()
            .map(|exec| (exec.id.clone(), exec))
            .collect())
    }

    fn find_all_executors(&self) -> Result<HashMap<ExecutorID, ExecutorInfoPtr>, FlameError> {
        let mut execs = HashMap::new();

        {
            let executors = lock_ptr!(self.executors)?;

            for e in executors.values() {
                execs.insert(e.id.clone(), e.clone());
            }
        }

        Ok(execs)
    }

    pub fn add_executor(&self, exec: ExecutorInfoPtr) -> Result<(), FlameError> {
        {
            let mut executors = lock_ptr!(self.executors)?;
            executors.insert(exec.id.clone(), exec.clone());
        }

        {
            let mut exec_index = lock_ptr!(self.exec_index)?;
            exec_index.entry(exec.state).or_default();

            if let Some(exec_list) = exec_index.get_mut(&exec.state.clone()) {
                exec_list.insert(exec.id.clone(), exec.clone());
            }
        }

        Ok(())
    }

    pub fn delete_executor(&self, exec: ExecutorInfoPtr) -> Result<(), FlameError> {
        {
            let mut executors = lock_ptr!(self.executors)?;
            executors.remove(&exec.id);
        }
        {
            let mut exec_index = lock_ptr!(self.exec_index)?;
            for exec_list in &mut exec_index.values_mut() {
                exec_list.remove(&exec.id);
            }
        }

        Ok(())
    }

    pub fn update_executor_state(
        &self,
        exec: ExecutorInfoPtr,
        state: ExecutorState,
    ) -> Result<(), FlameError> {
        let new_exec = Arc::new(ExecutorInfo {
            id: exec.id.clone(),
            node: exec.node.clone(),
            resreq: exec.resreq.clone(),
            task_id: exec.task_id,
            slots: exec.slots,
            shim: exec.shim,
            ssn_id: exec.ssn_id.clone(),
            creation_time: exec.creation_time,
            state,
        });

        self.delete_executor(new_exec.clone())?;
        self.add_executor(new_exec)?;

        Ok(())
    }

    ///
    /// Get the executors that maybe assigned to the session later, depending on the scheduler algorithm.
    /// The pipelined executors are the executors that are not bound to any other session and
    /// meet the session's resource requirements.
    ///
    /// # Arguments
    ///
    /// * `ssn`: The session to get the pipelined executors.
    ///
    /// # Returns
    ///
    /// The pipelined executors.
    ///
    pub fn pipelined_executors(
        &self,
        ssn: SessionInfoPtr,
    ) -> Result<Vec<ExecutorInfoPtr>, FlameError> {
        let void_execs = self.find_executors(VOID_EXECUTOR)?;
        let idle_execs = self.find_executors(IDLE_EXECUTOR)?;

        let executors = void_execs.values().chain(idle_execs.values());

        // Get the application's required shim
        let app_shim = self
            .get_application(&ssn.application)?
            .map(|app| app.shim)
            .unwrap_or(Shim::Host);

        Ok(executors
            .filter(|exec| ssn.slots == exec.slots && exec.shim == app_shim)
            .cloned()
            .collect())
    }
}

#[derive(Clone, Debug)]
pub struct Executor {
    pub id: ExecutorID,
    pub node: String,
    pub resreq: ResourceRequirement,
    pub slots: u32,
    pub shim: Shim,
    pub task_id: Option<TaskID>,
    pub ssn_id: Option<SessionID>,

    pub creation_time: DateTime<Utc>,
    pub state: ExecutorState,
}

impl Default for Executor {
    fn default() -> Self {
        Executor {
            id: String::new(),
            node: String::new(),
            resreq: ResourceRequirement::default(),
            slots: 0,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::default(),
        }
    }
}

pub type ExecutorPtr = MutexPtr<Executor>;

impl From<rpc::Executor> for Executor {
    fn from(e: rpc::Executor) -> Self {
        Executor::from(&e)
    }
}

impl From<&rpc::Executor> for Executor {
    fn from(e: &rpc::Executor) -> Self {
        let spec = e.spec.clone().unwrap();
        let status = e.status.clone().unwrap();
        let metadata = e.metadata.clone().unwrap();

        let state = rpc::ExecutorState::try_from(status.state).unwrap().into();

        Executor {
            id: metadata.id.clone(),
            node: spec.node.clone(),
            resreq: spec.resreq.unwrap().into(),
            slots: spec.slots,
            shim: Shim::from(spec.shim()),
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state,
        }
    }
}

impl From<Executor> for rpc::Executor {
    fn from(e: Executor) -> Self {
        rpc::Executor::from(&e)
    }
}

impl From<&Executor> for rpc::Executor {
    fn from(e: &Executor) -> Self {
        let metadata = Some(rpc::Metadata {
            id: e.id.clone(),
            name: e.id.clone(),
        });

        let spec = Some(rpc::ExecutorSpec {
            resreq: Some(e.resreq.clone().into()),
            node: e.node.clone(),
            slots: e.slots,
            shim: rpc::Shim::from(e.shim).into(), // Include shim in spec
        });

        let status = Some(rpc::ExecutorStatus {
            state: rpc::ExecutorState::from(e.state).into(),
            session_id: e.ssn_id.clone(),
        });

        rpc::Executor {
            metadata,
            spec,
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Helper to create a test executor with given parameters.
    fn create_test_executor(id: &str, slots: u32, state: ExecutorState) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
            },
            slots,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state,
        })
    }

    /// Helper to create a test session with given parameters.
    fn create_test_session(id: &str, slots: u32, state: SessionState) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            slots,
            tasks_status: HashMap::from([(TaskState::Pending, 1)]),
            creation_time: Utc::now(),
            completion_time: None,
            state,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
        })
    }

    /// Test that SnapShot correctly filters executors by state.
    #[test]
    fn test_snapshot_find_executors_by_state() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        // Add executors with different states
        let exec_idle1 = create_test_executor("exec-idle-1", 2, ExecutorState::Idle);
        let exec_idle2 = create_test_executor("exec-idle-2", 2, ExecutorState::Idle);
        let exec_bound = create_test_executor("exec-bound", 2, ExecutorState::Bound);
        let exec_void = create_test_executor("exec-void", 2, ExecutorState::Void);

        ss.add_executor(exec_idle1.clone()).unwrap();
        ss.add_executor(exec_idle2.clone()).unwrap();
        ss.add_executor(exec_bound.clone()).unwrap();
        ss.add_executor(exec_void.clone()).unwrap();

        // Find only idle executors
        let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
        assert_eq!(idle_execs.len(), 2);
        assert!(idle_execs.contains_key("exec-idle-1"));
        assert!(idle_execs.contains_key("exec-idle-2"));

        // Find only bound executors
        let bound_execs = ss.find_executors(BOUND_EXECUTOR).unwrap();
        assert_eq!(bound_execs.len(), 1);
        assert!(bound_execs.contains_key("exec-bound"));

        // Find only void executors
        let void_execs = ss.find_executors(VOID_EXECUTOR).unwrap();
        assert_eq!(void_execs.len(), 1);
        assert!(void_execs.contains_key("exec-void"));

        // Find all executors
        let all_execs = ss.find_executors(ALL_EXECUTOR).unwrap();
        assert_eq!(all_execs.len(), 4);
    }

    /// Test that SnapShot correctly filters sessions by state.
    #[test]
    fn test_snapshot_find_sessions_by_state() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        // Add sessions with different states
        let ssn_open1 = create_test_session("ssn-open-1", 2, SessionState::Open);
        let ssn_open2 = create_test_session("ssn-open-2", 2, SessionState::Open);
        let ssn_closed = create_test_session("ssn-closed", 2, SessionState::Closed);

        ss.add_session(ssn_open1.clone()).unwrap();
        ss.add_session(ssn_open2.clone()).unwrap();
        ss.add_session(ssn_closed.clone()).unwrap();

        // Find only open sessions
        let open_ssns = ss.find_sessions(OPEN_SESSION).unwrap();
        assert_eq!(open_ssns.len(), 2);
        assert!(open_ssns.contains_key("ssn-open-1"));
        assert!(open_ssns.contains_key("ssn-open-2"));

        // Find all sessions
        let all_ssns = ss.find_sessions(None).unwrap();
        assert_eq!(all_ssns.len(), 3);
    }

    /// Test that update_executor_state correctly updates the exec_index.
    #[test]
    fn test_snapshot_update_executor_state() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        // Add an idle executor
        let exec = create_test_executor("exec-1", 2, ExecutorState::Idle);
        ss.add_executor(exec.clone()).unwrap();

        // Verify it's in the idle index
        let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
        assert_eq!(idle_execs.len(), 1);

        // Update state to Bound
        ss.update_executor_state(exec.clone(), ExecutorState::Bound)
            .unwrap();

        // Verify it's no longer in idle index
        let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
        assert_eq!(idle_execs.len(), 0);

        // Verify it's now in bound index
        let bound_execs = ss.find_executors(BOUND_EXECUTOR).unwrap();
        assert_eq!(bound_execs.len(), 1);
    }

    /// Test pipelined_executors filters by slots correctly.
    #[test]
    fn test_snapshot_pipelined_executors_filters_by_slots() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        // Add executors with different slots
        let exec_slots2_idle = create_test_executor("exec-2-idle", 2, ExecutorState::Idle);
        let exec_slots4_idle = create_test_executor("exec-4-idle", 4, ExecutorState::Idle);
        let exec_slots2_void = create_test_executor("exec-2-void", 2, ExecutorState::Void);
        let exec_slots2_bound = create_test_executor("exec-2-bound", 2, ExecutorState::Bound);

        ss.add_executor(exec_slots2_idle.clone()).unwrap();
        ss.add_executor(exec_slots4_idle.clone()).unwrap();
        ss.add_executor(exec_slots2_void.clone()).unwrap();
        ss.add_executor(exec_slots2_bound.clone()).unwrap();

        // Create a session with slots=2
        let ssn = create_test_session("ssn-1", 2, SessionState::Open);

        // Get pipelined executors (should only include idle and void with matching slots)
        let pipelined = ss.pipelined_executors(ssn).unwrap();

        // Should include exec-2-idle and exec-2-void (both have slots=2 and are idle/void)
        // Should NOT include exec-4-idle (wrong slots) or exec-2-bound (wrong state)
        assert_eq!(pipelined.len(), 2);

        let ids: Vec<&str> = pipelined.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"exec-2-idle"));
        assert!(ids.contains(&"exec-2-void"));
        assert!(!ids.contains(&"exec-4-idle"));
        assert!(!ids.contains(&"exec-2-bound"));
    }

    /// Test that empty filters return empty results.
    #[test]
    fn test_snapshot_empty_filters() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        // Add some executors
        let exec = create_test_executor("exec-1", 2, ExecutorState::Idle);
        ss.add_executor(exec).unwrap();

        // Filter by state that doesn't exist
        let releasing_execs = ss
            .find_executors(Some(ExecutorFilter {
                state: Some(ExecutorState::Releasing),
                ids: None,
                node: None,
            }))
            .unwrap();
        assert_eq!(releasing_execs.len(), 0);

        // Filter by ID that doesn't exist
        let nonexistent = ss
            .find_executors(Some(ExecutorFilter {
                state: None,
                ids: Some(vec!["nonexistent".to_string()]),
                node: None,
            }))
            .unwrap();
        assert_eq!(nonexistent.len(), 0);
    }

    /// Test that delete_executor removes from both main map and index.
    #[test]
    fn test_snapshot_delete_executor() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });

        let exec = create_test_executor("exec-1", 2, ExecutorState::Idle);
        ss.add_executor(exec.clone()).unwrap();

        // Verify it exists
        let all_execs = ss.find_executors(ALL_EXECUTOR).unwrap();
        assert_eq!(all_execs.len(), 1);

        let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
        assert_eq!(idle_execs.len(), 1);

        // Delete it
        ss.delete_executor(exec).unwrap();

        // Verify it's gone from both
        let all_execs = ss.find_executors(ALL_EXECUTOR).unwrap();
        assert_eq!(all_execs.len(), 0);

        let idle_execs = ss.find_executors(IDLE_EXECUTOR).unwrap();
        assert_eq!(idle_execs.len(), 0);
    }
}

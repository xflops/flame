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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use stdng::{lock_ptr, new_ptr, MutexPtr};

use common::apis::{
    Application, ApplicationFilter, ApplicationState, Executor, ExecutorFilter, ExecutorID,
    ExecutorState, Node, NodeState, ResourceRequirement, Session, SessionFilter, SessionID,
    SessionPredicate, SessionState, Shim, Signal, Task, TaskID, TaskState, ALL_APPLICATION,
    ALL_EXECUTOR, BOUND_EXECUTOR, IDLE_EXECUTOR, OPEN_SESSION, READY_SESSION, VOID_EXECUTOR,
};
use common::ctx::DEFAULT_SESSION_RETRY_LIMITS;
use common::FlameError;
use rpc::flame::v1 as rpc;

pub type SessionInfoPtr = Arc<SessionInfo>;
pub type TaskInfoPtr = Arc<TaskInfo>;
pub type ExecutorInfoPtr = Arc<ExecutorInfo>;
pub type NodeInfoPtr = Arc<NodeInfo>;
pub type AppInfoPtr = Arc<AppInfo>;

#[derive(Clone)]
pub struct SnapShot {
    pub session_retry_limits: u32,

    pub applications: MutexPtr<HashMap<String, AppInfoPtr>>,

    pub sessions: MutexPtr<HashMap<SessionID, SessionInfoPtr>>,
    pub ssn_index: MutexPtr<HashMap<SessionState, HashMap<SessionID, SessionInfoPtr>>>,
    pub executors: MutexPtr<HashMap<ExecutorID, ExecutorInfoPtr>>,
    pub exec_index: MutexPtr<HashMap<ExecutorState, HashMap<ExecutorID, ExecutorInfoPtr>>>,

    pub nodes: MutexPtr<HashMap<String, NodeInfoPtr>>,
}

pub type SnapShotPtr = Arc<SnapShot>;

impl Default for SnapShot {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapShot {
    pub fn new() -> Self {
        Self::new_with_session_retry_limits(DEFAULT_SESSION_RETRY_LIMITS)
    }

    pub fn new_with_session_retry_limits(session_retry_limits: u32) -> Self {
        SnapShot {
            session_retry_limits,
            applications: new_ptr(HashMap::new()),
            sessions: new_ptr(HashMap::new()),
            ssn_index: new_ptr(HashMap::new()),
            executors: new_ptr(HashMap::new()),
            exec_index: new_ptr(HashMap::new()),
            nodes: new_ptr(HashMap::new()),
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
    pub affinity: HashSet<Bytes>,
}

#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    pub id: SessionID,
    pub application: String,

    pub tasks_status: HashMap<TaskState, i32>,

    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,

    pub state: SessionState,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,
    pub resreq: Option<ResourceRequirement>,
    pub retry_count: u32,
    pub task_index: HashMap<TaskState, BTreeMap<TaskID, TaskInfoPtr>>,
}

impl SessionInfo {
    pub fn is_open(&self) -> bool {
        self.state == SessionState::Open
    }

    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutorInfo {
    pub id: ExecutorID,
    pub node: String,
    pub resreq: ResourceRequirement,
    pub shim: Shim,
    /// Application owning the retained service instance.
    pub application: String,
    pub task_id: Option<TaskID>,
    pub ssn_id: Option<SessionID>,

    pub creation_time: DateTime<Utc>,
    /// Last in-memory lifecycle update, used to age Idle retained instances.
    pub latest_updated_timestamp: DateTime<Utc>,
    pub state: ExecutorState,
    pub signal: Option<Signal>,
    /// Last accepted volatile attributes for the retained service instance.
    pub attributes: HashSet<Bytes>,
}

impl ExecutorInfo {
    pub fn is_interrupted(&self) -> bool {
        self.signal == Some(Signal::Interrupt)
    }

    pub fn is_binding(&self, ssn_id: &SessionID) -> bool {
        self.ssn_id.as_ref() == Some(ssn_id)
            && matches!(self.state, ExecutorState::Binding | ExecutorState::Bound)
    }
}

#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    pub name: String,
    pub allocatable: ResourceRequirement,
    pub state: NodeState,
    /// The shims the node's executor manager runs, as it reported them.
    pub shims: Vec<Shim>,
}

impl NodeInfo {
    /// Whether the node's executor manager is connected and can start executors.
    pub fn is_ready(&self) -> bool {
        self.state == NodeState::Ready
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub name: String,
    pub state: ApplicationState,
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
            shims: node.shims.clone(),
        }
    }
}

impl From<&Application> for AppInfo {
    fn from(app: &Application) -> Self {
        AppInfo {
            name: app.name.to_string(),
            state: app.state,
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
            shim: exec.shim,
            application: exec.application.clone(),
            task_id: exec.task_id,
            ssn_id: exec.ssn_id.clone(),
            creation_time: exec.creation_time,
            latest_updated_timestamp: exec.latest_updated_timestamp,
            state: exec.state,
            signal: exec.signal,
            attributes: exec.attributes.clone(),
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
            affinity: task.affinity.clone(),
        }
    }
}

impl TryFrom<&Session> for SessionInfo {
    type Error = FlameError;

    fn try_from(ssn: &Session) -> Result<Self, Self::Error> {
        let mut tasks_status = HashMap::new();
        for state in [
            TaskState::Pending,
            TaskState::Running,
            TaskState::Succeed,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            tasks_status.insert(
                state,
                ssn.task_counts.get(state).min(i32::MAX as u64) as i32,
            );
        }
        let mut task_index = HashMap::new();
        for (state, tasks) in ssn
            .tasks_index
            .iter()
            .filter(|(state, _)| !state.is_terminal())
        {
            let mut task_infos = BTreeMap::new();
            for (task_id, task) in tasks {
                task_infos.insert(*task_id, Arc::new(TaskInfo::from(&*lock_ptr!(task)?)));
            }
            task_index.insert(*state, task_infos);
        }

        Ok(SessionInfo {
            id: ssn.id.clone(),
            application: ssn.application.clone(),
            tasks_status,
            creation_time: ssn.creation_time,
            completion_time: ssn.completion_time,
            state: ssn.status.state,
            min_instances: ssn.min_instances,
            max_instances: ssn.max_instances,
            batch_size: ssn.batch_size,
            priority: ssn.priority,
            resreq: ssn.resreq.clone(),
            retry_count: ssn.retry_count,
            task_index,
        })
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
/// Nodes whose executor manager is connected. Executors can only be created on, or
/// started by, such nodes.
pub const READY_NODE: Option<NodeFilter> = Some(NodeFilter::by_state(NodeState::Ready));

impl SnapShot {
    /// The node of that name, or `None` when the snapshot does not know it.
    pub fn get_node(&self, node_name: &str) -> Result<Option<NodeInfoPtr>, FlameError> {
        let nodes = lock_ptr!(self.nodes)?;
        Ok(nodes.get(node_name).cloned())
    }

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
        filter: Option<ApplicationFilter>,
    ) -> Result<HashMap<String, AppInfoPtr>, FlameError> {
        match filter {
            Some(filter) => self.find_applications_by_filter(filter),
            None => self.find_all_applications(),
        }
    }

    fn find_applications_by_filter(
        &self,
        filter: ApplicationFilter,
    ) -> Result<HashMap<String, AppInfoPtr>, FlameError> {
        let apps = lock_ptr!(self.applications)?;

        let filtered: Vec<AppInfoPtr> = match filter.state {
            None => apps.values().cloned().collect(),
            Some(state) => apps
                .values()
                .filter(|app| app.state == state)
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

        let filtered: Vec<SessionInfoPtr> = match filter.application {
            None => filtered,
            Some(ref application) => filtered
                .into_iter()
                .filter(|ssn| ssn.application == *application)
                .collect(),
        };

        let filtered: Vec<SessionInfoPtr> = match filter.predicate {
            None => filtered,
            Some(predicate) => filtered
                .into_iter()
                .filter(|ssn| predicate.matches(ssn.retry_count, self.session_retry_limits))
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

    pub fn update_executor(&self, exec: ExecutorInfoPtr) -> Result<(), FlameError> {
        self.delete_executor(exec.clone())?;
        self.add_executor(exec)
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
            shim: exec.shim,
            application: exec.application.clone(),
            ssn_id: exec.ssn_id.clone(),
            creation_time: exec.creation_time,
            latest_updated_timestamp: Utc::now(),
            state,
            signal: exec.signal,
            attributes: exec.attributes.clone(),
        });

        self.update_executor(new_exec)
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

        // Match by exact resreq equality when the session has an explicit
        // resource request. With slots fully removed, an executor created for a
        // session is sized by that session's resreq, so equality is the
        // appropriate match condition — the same role `slots == exec.slots`
        // previously played.
        Ok(executors
            .filter(|exec| {
                exec.shim == app_shim
                    && ssn
                        .resreq
                        .as_ref()
                        .map(|rr| rr == &exec.resreq)
                        .unwrap_or(true)
            })
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests;

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
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use stdng::{lock_ptr, MutexPtr};

use common::apis::{
    Application, ApplicationFilter, ApplicationState, Executor, ExecutorFilter, ExecutorID,
    ExecutorState, Node, NodeState, ResourceRequirement, Session, SessionFilter, SessionGID,
    SessionPredicate, SessionState, Shim, Task, TaskName, TaskState, ALL_EXECUTOR, BOUND_EXECUTOR,
    IDLE_EXECUTOR, VOID_EXECUTOR,
};
use common::ctx::DEFAULT_SESSION_RETRY_LIMITS;
use common::FlameError;
use rpc::flame::v1 as rpc;

pub type SessionInfoPtr = Arc<SessionInfo>;
pub type TaskInfoPtr = Arc<TaskInfo>;
pub type ExecutorInfoPtr = Arc<ExecutorInfo>;
pub type NodeInfoPtr = Arc<NodeInfo>;
pub type AppInfoPtr = Arc<AppInfo>;
pub type ScopedName = (String, String);

#[derive(Clone)]
pub struct SnapShot {
    pub session_retry_limits: u32,

    pub applications: MutexPtr<HashMap<ScopedName, AppInfoPtr>>,

    pub sessions: MutexPtr<HashMap<ScopedName, SessionInfoPtr>>,
    pub ssn_index: MutexPtr<HashMap<SessionState, HashMap<ScopedName, SessionInfoPtr>>>,
    pub executors: MutexPtr<HashMap<String, ExecutorInfoPtr>>,
    pub exec_index: MutexPtr<HashMap<ExecutorState, HashMap<String, ExecutorInfoPtr>>>,

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

    pub fn get_application(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Option<AppInfoPtr>, FlameError> {
        let apps = lock_ptr!(self.applications)?;
        Ok(apps
            .get(&(workspace.to_string(), name.to_string()))
            .cloned())
    }

    pub fn get_application_by_name(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Option<AppInfoPtr>, FlameError> {
        self.get_application(workspace, name)
    }
}

#[derive(Debug, Default, Clone)]
pub struct TaskInfo {
    pub id: String,
    pub name: TaskName,
    pub session: String,
    pub workspace: String,

    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,

    pub state: TaskState,
    pub affinity: HashSet<Bytes>,
}

impl TaskInfo {
    pub fn session(&self) -> SessionGID {
        SessionGID::new(&self.workspace, &self.session)
    }
}

#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub workspace: String,
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
    pub task_index: HashMap<TaskState, BTreeMap<TaskName, TaskInfoPtr>>,
}

impl SessionInfo {
    pub fn gid(&self) -> SessionGID {
        SessionGID::new(&self.workspace, &self.name)
    }

    pub fn key(&self) -> ScopedName {
        (self.workspace.clone(), self.name.clone())
    }

    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutorInfo {
    pub id: ExecutorID,
    pub name: String,
    pub node: String,
    pub workspace: String,
    pub resreq: ResourceRequirement,
    pub shim: Shim,
    /// Application owning the retained service instance.
    pub application: String,
    pub task: Option<String>,
    pub session: Option<String>,

    pub creation_time: DateTime<Utc>,
    /// Last in-memory lifecycle update, used to age Idle retained instances.
    pub latest_updated_timestamp: DateTime<Utc>,
    pub state: ExecutorState,
    /// Last accepted volatile attributes for the retained service instance.
    pub attributes: HashSet<Bytes>,
}

impl ExecutorInfo {
    pub fn session(&self) -> Option<SessionGID> {
        self.session
            .as_ref()
            .map(|session| SessionGID::new(&self.workspace, session))
    }
}

#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    pub name: String,
    pub allocatable: ResourceRequirement,
    pub state: NodeState,
}

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub workspace: String,
    pub state: ApplicationState,
    pub shim: Shim, // Required shim type for the application
    pub max_instances: u32,
    pub delay_release: Duration,
}

impl AppInfo {
    pub fn key(&self) -> ScopedName {
        (self.workspace.clone(), self.name.clone())
    }
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
            id: app.id.clone(),
            name: app.name.to_string(),
            workspace: app.workspace.clone(),
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
            name: exec.name.clone(),
            node: exec.node.clone(),
            workspace: exec.workspace.clone(),
            resreq: exec.resreq.clone(),
            shim: exec.shim,
            application: exec.application.clone(),
            task: exec.task.clone(),
            session: exec.session.clone(),
            creation_time: exec.creation_time,
            latest_updated_timestamp: exec.latest_updated_timestamp,
            state: exec.state,
            attributes: exec.attributes.clone(),
        }
    }
}

impl From<&Task> for TaskInfo {
    fn from(task: &Task) -> Self {
        TaskInfo {
            id: task.id.clone(),
            name: task.name,
            session: task.session.clone(),
            workspace: task.workspace.clone(),
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
        for (k, v) in &ssn.tasks_index {
            tasks_status.insert(*k, v.len() as i32);
        }
        let mut task_index = HashMap::new();
        for (state, tasks) in ssn
            .tasks_index
            .iter()
            .filter(|(state, _)| !state.is_terminal())
        {
            let mut task_infos = BTreeMap::new();
            for (number, task) in tasks {
                task_infos.insert(*number, Arc::new(TaskInfo::from(&*lock_ptr!(task)?)));
            }
            task_index.insert(*state, task_infos);
        }

        Ok(SessionInfo {
            id: ssn.id.clone(),
            name: ssn.name.clone(),
            workspace: ssn.workspace.clone(),
            application: ssn.application.clone(),
            tasks_status,
            creation_time: ssn.creation_time,
            completion_time: ssn.completion_time,
            state: ssn.status.state,
            min_instances: ssn.min_instances,
            max_instances: ssn.max_instances,
            batch_size: 1,
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

    /// Query applications within the filter's mandatory workspace.
    pub fn find_applications(
        &self,
        filter: &ApplicationFilter,
    ) -> Result<HashMap<ScopedName, AppInfoPtr>, FlameError> {
        let apps = lock_ptr!(self.applications)?;

        let filtered: Vec<AppInfoPtr> = match filter.state {
            None => apps.values().cloned().collect(),
            Some(state) => apps
                .values()
                .filter(|app| app.state == state)
                .cloned()
                .collect(),
        };

        let filtered = filtered
            .into_iter()
            .filter(|app| app.workspace == filter.workspace);

        Ok(filtered.map(|app| (app.key(), app)).collect())
    }

    /// Iterate every application for internal cluster-wide scheduling.
    pub fn all_applications(&self) -> Result<HashMap<ScopedName, AppInfoPtr>, FlameError> {
        let mut appinfos = HashMap::new();

        {
            let apps = lock_ptr!(self.applications)?;

            for app in apps.values() {
                appinfos.insert(app.key(), app.clone());
            }
        }

        Ok(appinfos)
    }

    /// Query sessions within the filter's mandatory workspace.
    pub fn find_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<HashMap<ScopedName, SessionInfoPtr>, FlameError> {
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

        // Apply local-name filter if specified.
        let filtered: Vec<SessionInfoPtr> = match filter.names {
            None => candidates,
            Some(ref names) => candidates
                .into_iter()
                .filter(|ssn| names.contains(&ssn.name))
                .collect(),
        };

        let filtered: Vec<SessionInfoPtr> = filtered
            .into_iter()
            .filter(|ssn| ssn.workspace == filter.workspace)
            .collect();

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

        Ok(filtered.into_iter().map(|ssn| (ssn.key(), ssn)).collect())
    }

    /// Iterate every session for internal cluster-wide scheduling.
    pub fn all_sessions(&self) -> Result<HashMap<ScopedName, SessionInfoPtr>, FlameError> {
        let mut ssns = HashMap::new();

        {
            let sessions = lock_ptr!(self.sessions)?;

            for ssn in sessions.values() {
                ssns.insert(ssn.key(), ssn.clone());
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
            sessions.insert(ssn.key(), ssn.clone());
        }

        {
            let mut ssn_index = lock_ptr!(self.ssn_index)?;
            ssn_index.entry(ssn.state).or_default();

            if let Some(ssn_list) = ssn_index.get_mut(&ssn.state) {
                ssn_list.insert(ssn.key(), ssn.clone());
            }
        }

        Ok(())
    }

    pub fn add_application(&self, app: AppInfoPtr) -> Result<(), FlameError> {
        {
            let mut apps = lock_ptr!(self.applications)?;
            apps.insert(app.key(), app.clone());
        }

        Ok(())
    }

    pub fn get_session(&self, gid: &SessionGID) -> Result<SessionInfoPtr, FlameError> {
        let sessions = lock_ptr!(self.sessions)?;
        match sessions.get(&(gid.workspace.clone(), gid.session.clone())) {
            Some(ptr) => Ok(ptr.clone()),
            None => Err(FlameError::NotFound(format!(
                "session <{}/{}> not found",
                gid.workspace, gid.session
            ))),
        }
    }

    pub fn delete_session(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        {
            let mut sessions = lock_ptr!(self.sessions)?;
            sessions.remove(&ssn.key());
        }

        {
            let mut ssn_index = lock_ptr!(self.ssn_index)?;
            for ssn_list in &mut ssn_index.values_mut() {
                ssn_list.remove(&ssn.key());
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
    ) -> Result<HashMap<String, ExecutorInfoPtr>, FlameError> {
        match filter {
            Some(filter) => self.find_executors_by_filter(filter),
            None => self.find_all_executors(),
        }
    }

    fn find_executors_by_filter(
        &self,
        filter: ExecutorFilter,
    ) -> Result<HashMap<String, ExecutorInfoPtr>, FlameError> {
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

        // Apply local-name filter if specified.
        let filtered: Vec<ExecutorInfoPtr> = match filter.names {
            None => candidates,
            Some(ref names) => candidates
                .into_iter()
                .filter(|exec| names.contains(&exec.name))
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
            .map(|exec| (exec.name.clone(), exec))
            .collect())
    }

    fn find_all_executors(&self) -> Result<HashMap<String, ExecutorInfoPtr>, FlameError> {
        let mut execs = HashMap::new();

        {
            let executors = lock_ptr!(self.executors)?;

            for e in executors.values() {
                execs.insert(e.name.clone(), e.clone());
            }
        }

        Ok(execs)
    }

    pub fn add_executor(&self, exec: ExecutorInfoPtr) -> Result<(), FlameError> {
        {
            let mut executors = lock_ptr!(self.executors)?;
            executors.insert(exec.name.clone(), exec.clone());
        }

        {
            let mut exec_index = lock_ptr!(self.exec_index)?;
            exec_index.entry(exec.state).or_default();

            if let Some(exec_list) = exec_index.get_mut(&exec.state.clone()) {
                exec_list.insert(exec.name.clone(), exec.clone());
            }
        }

        Ok(())
    }

    pub fn delete_executor(&self, exec: ExecutorInfoPtr) -> Result<(), FlameError> {
        {
            let mut executors = lock_ptr!(self.executors)?;
            executors.remove(&exec.name);
        }
        {
            let mut exec_index = lock_ptr!(self.exec_index)?;
            for exec_list in &mut exec_index.values_mut() {
                exec_list.remove(&exec.name);
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
            name: exec.name.clone(),
            node: exec.node.clone(),
            workspace: exec.workspace.clone(),
            resreq: exec.resreq.clone(),
            task: exec.task.clone(),
            shim: exec.shim,
            application: exec.application.clone(),
            session: exec.session.clone(),
            creation_time: exec.creation_time,
            latest_updated_timestamp: Utc::now(),
            state,
            attributes: exec.attributes.clone(),
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
            .get_application_by_name(&ssn.workspace, &ssn.application)?
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
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn application_filter_maps_rpc_state() {
        let filter = ApplicationFilter::try_from(rpc::ListApplicationsRequest {
            state: Some(rpc::ApplicationState::Disabled as i32),
            workspace: Some("default".to_string()),
        })
        .unwrap();

        assert_eq!(filter.state, Some(ApplicationState::Disabled));
    }

    #[test]
    fn application_filter_rejects_unknown_rpc_state() {
        assert!(ApplicationFilter::try_from(rpc::ListApplicationsRequest {
            state: Some(99),
            workspace: Some("default".to_string())
        })
        .is_err());
    }

    #[test]
    fn application_queries_isolate_repeated_names_by_workspace() {
        let snapshot = SnapShot::new();
        for workspace in ["default", "research"] {
            snapshot
                .add_application(Arc::new(AppInfo {
                    name: "shared".to_string(),
                    workspace: workspace.to_string(),
                    state: ApplicationState::Enabled,
                    ..Default::default()
                }))
                .unwrap();
        }
        for workspace in ["default", "research"] {
            let applications = snapshot
                .find_applications(&ApplicationFilter::new(workspace))
                .unwrap();
            assert_eq!(applications.len(), 1);
            assert!(applications.contains_key(&(workspace.to_string(), "shared".to_string())));
            assert_eq!(
                snapshot
                    .find_applications(
                        &ApplicationFilter::new(workspace).by_state(ApplicationState::Enabled)
                    )
                    .unwrap()
                    .len(),
                1
            );
            assert!(snapshot
                .find_applications(
                    &ApplicationFilter::new(workspace).by_state(ApplicationState::Disabled)
                )
                .unwrap()
                .is_empty());
        }
        assert_eq!(snapshot.all_applications().unwrap().len(), 2);
    }

    #[test]
    fn session_filter_maps_rpc_fields() {
        let filter = SessionFilter::try_from(rpc::ListSessionsRequest {
            application: Some("test-app".to_string()),
            state: Some(rpc::SessionState::Open as i32),
            workspace: Some("default".to_string()),
        })
        .unwrap();

        assert_eq!(filter.application.as_deref(), Some("test-app"));
        assert_eq!(filter.state, Some(SessionState::Open));
    }

    #[test]
    fn session_filter_rejects_unknown_rpc_state() {
        assert!(SessionFilter::try_from(rpc::ListSessionsRequest {
            application: None,
            state: Some(99),
            workspace: Some("default".to_string()),
        })
        .is_err());
    }

    #[test]
    fn executor_rpc_round_trip_preserves_application() {
        let executor = Executor {
            application: "test-app".to_string(),
            ..Default::default()
        };

        let rpc_executor = rpc::Executor::from(&executor);
        assert_eq!(rpc_executor.spec.as_ref().unwrap().application, "test-app");
        assert_eq!(Executor::from(&rpc_executor).application, "test-app");
    }

    #[test]
    fn executor_info_preserves_latest_updated_timestamp() {
        let timestamp = Utc::now() - Duration::minutes(5);
        let executor = Executor {
            latest_updated_timestamp: timestamp,
            ..Default::default()
        };

        assert_eq!(
            ExecutorInfo::from(&executor).latest_updated_timestamp,
            timestamp
        );
    }

    /// Helper to create a test executor with given resreq.
    fn create_test_executor(
        id: &str,
        resreq: ResourceRequirement,
        state: ExecutorState,
    ) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            name: id.to_string(),
            node: "test-node".to_string(),
            workspace: "default".to_string(),
            resreq,
            shim: Shim::Host,
            application: String::new(),
            task: None,
            session: None,
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state,
            attributes: HashSet::new(),
        })
    }

    /// Helper to create a test session with given resreq.
    fn create_test_session(
        id: &str,
        resreq: Option<ResourceRequirement>,
        state: SessionState,
    ) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            name: id.to_string(),
            workspace: "default".to_string(),
            application: "test-app".to_string(),
            tasks_status: HashMap::from([(TaskState::Pending, 1)]),
            creation_time: Utc::now(),
            completion_time: None,
            state,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq,
            retry_count: 0,
            task_index: HashMap::new(),
        })
    }

    #[test]
    fn session_info_ready_uses_retry_limit() {
        let mut session = create_test_session("ssn-ready", Some(unit_rr()), SessionState::Open)
            .as_ref()
            .clone();

        session.retry_count = 1;
        assert!(session.is_ready(2));

        session.retry_count = 2;
        assert!(!session.is_ready(2));
    }

    #[test]
    fn session_info_from_session_copies_retry_count() {
        let session = Session {
            id: "ssn-1".to_string(),
            application: "test-app".to_string(),
            retry_count: 7,
            ..Default::default()
        };

        let info = SessionInfo::try_from(&session).unwrap();

        assert_eq!(info.retry_count, 7);
    }

    /// Default per-slot unit kept for backward-compatible test scaffolding.
    fn unit_rr() -> ResourceRequirement {
        ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        }
    }

    /// Scale `unit_rr` by `n`. Mirrors the old `slots × unit` arithmetic the
    /// pre-cleanup tests relied on.
    fn slots_rr(n: u32) -> ResourceRequirement {
        unit_rr().mul(n)
    }

    /// Test that SnapShot correctly filters executors by state.
    #[test]
    fn test_snapshot_find_executors_by_state() {
        let ss = SnapShot::new();

        // Add executors with different states
        let exec_idle1 = create_test_executor("exec-idle-1", slots_rr(2), ExecutorState::Idle);
        let exec_idle2 = create_test_executor("exec-idle-2", slots_rr(2), ExecutorState::Idle);
        let exec_bound = create_test_executor("exec-bound", slots_rr(2), ExecutorState::Bound);
        let exec_void = create_test_executor("exec-void", slots_rr(2), ExecutorState::Void);

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
        let ss = SnapShot::new();

        // Add sessions with different states
        let ssn_open1 = create_test_session("ssn-open-1", Some(slots_rr(2)), SessionState::Open);
        let ssn_open2 = create_test_session("ssn-open-2", Some(slots_rr(2)), SessionState::Open);
        let ssn_closed = create_test_session("ssn-closed", Some(slots_rr(2)), SessionState::Closed);

        ss.add_session(ssn_open1.clone()).unwrap();
        ss.add_session(ssn_open2.clone()).unwrap();
        ss.add_session(ssn_closed.clone()).unwrap();

        // Find only open sessions
        let open_ssns = ss
            .find_sessions(&SessionFilter::new("default").by_state(SessionState::Open))
            .unwrap();
        assert_eq!(open_ssns.len(), 2);
        assert!(open_ssns.contains_key(&("default".to_string(), "ssn-open-1".to_string())));
        assert!(open_ssns.contains_key(&("default".to_string(), "ssn-open-2".to_string())));

        // Find all sessions
        let all_ssns = ss.find_sessions(&SessionFilter::new("default")).unwrap();
        assert_eq!(all_ssns.len(), 3);
    }

    #[test]
    fn test_snapshot_find_ready_sessions() {
        let ss = SnapShot::new_with_session_retry_limits(2);

        let ready_open = create_test_session("ssn-ready", Some(slots_rr(2)), SessionState::Open);
        let mut not_ready_open =
            create_test_session("ssn-not-ready", Some(slots_rr(2)), SessionState::Open)
                .as_ref()
                .clone();
        not_ready_open.retry_count = 2;
        let closed_ready =
            create_test_session("ssn-closed-ready", Some(slots_rr(2)), SessionState::Closed);

        ss.add_session(ready_open.clone()).unwrap();
        ss.add_session(Arc::new(not_ready_open)).unwrap();
        ss.add_session(closed_ready.clone()).unwrap();

        let ready_ssns = ss
            .find_sessions(
                &SessionFilter::new("default")
                    .by_state(SessionState::Open)
                    .with_predicate(SessionPredicate::Ready),
            )
            .unwrap();

        assert_eq!(ready_ssns.len(), 1);
        assert!(ready_ssns.contains_key(&("default".to_string(), "ssn-ready".to_string())));
    }

    #[test]
    fn session_queries_isolate_repeated_names_by_workspace() {
        let snapshot = SnapShot::new();
        for workspace in ["default", "research"] {
            let mut session = create_test_session("shared", None, SessionState::Open)
                .as_ref()
                .clone();
            session.workspace = workspace.to_string();
            snapshot.add_session(Arc::new(session)).unwrap();
        }
        for workspace in ["default", "research"] {
            for filter in [
                SessionFilter::new(workspace),
                SessionFilter::new(workspace).by_names(vec!["shared".to_string()]),
                SessionFilter::new(workspace).by_state(SessionState::Open),
                SessionFilter::new(workspace).by_application("test-app"),
            ] {
                let sessions = snapshot.find_sessions(&filter).unwrap();
                assert_eq!(sessions.len(), 1);
                assert!(sessions.contains_key(&(workspace.to_string(), "shared".to_string())));
            }
            assert!(snapshot
                .find_sessions(&SessionFilter::new(workspace).by_names(vec![]))
                .unwrap()
                .is_empty());
        }
        assert_eq!(snapshot.all_sessions().unwrap().len(), 2);
    }

    /// Test that update_executor_state correctly updates the exec_index.
    #[test]
    fn test_snapshot_update_executor_state() {
        let ss = SnapShot::new();
        let previous_timestamp = Utc::now() - Duration::minutes(5);

        // Add an idle executor
        let exec = Arc::new(ExecutorInfo {
            application: "test-app".to_string(),
            attributes: HashSet::from([Bytes::from_static(b"key")]),
            latest_updated_timestamp: previous_timestamp,
            ..(*create_test_executor("exec-1", slots_rr(2), ExecutorState::Idle)).clone()
        });
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
        let updated = bound_execs.get("exec-1").unwrap();
        assert_eq!(updated.application, "test-app");
        assert!(updated.latest_updated_timestamp > previous_timestamp);
        assert_eq!(
            updated.attributes,
            HashSet::from([Bytes::from_static(b"key")])
        );
    }

    /// Test pipelined_executors filters by resreq equality correctly.
    #[test]
    fn test_snapshot_pipelined_executors_filters_by_resreq() {
        let ss = SnapShot::new();

        // Register the application so `pipelined_executors` can look up its shim.
        ss.add_application(Arc::new(AppInfo {
            id: "debug-app-id".to_string(),
            name: "test-app".to_string(),
            workspace: "default".to_string(),
            state: ApplicationState::Enabled,
            shim: Shim::Host,
            max_instances: 0,
            delay_release: chrono::Duration::zero(),
        }))
        .unwrap();

        // Executors sized by different resreq values.
        let exec_r2_idle = create_test_executor("exec-r2-idle", slots_rr(2), ExecutorState::Idle);
        let exec_r4_idle = create_test_executor("exec-r4-idle", slots_rr(4), ExecutorState::Idle);
        let exec_r2_void = create_test_executor("exec-r2-void", slots_rr(2), ExecutorState::Void);
        let exec_r2_bound =
            create_test_executor("exec-r2-bound", slots_rr(2), ExecutorState::Bound);

        ss.add_executor(exec_r2_idle.clone()).unwrap();
        ss.add_executor(exec_r4_idle.clone()).unwrap();
        ss.add_executor(exec_r2_void.clone()).unwrap();
        ss.add_executor(exec_r2_bound.clone()).unwrap();

        // Session asks for resreq equivalent to "2 × unit".
        let ssn = create_test_session("ssn-1", Some(slots_rr(2)), SessionState::Open);

        // Get pipelined executors (should only include idle and void with matching resreq).
        let pipelined = ss.pipelined_executors(ssn).unwrap();

        // Should include exec-r2-idle and exec-r2-void (resreq matches and state is idle/void).
        // Should NOT include exec-r4-idle (wrong resreq) or exec-r2-bound (wrong state).
        assert_eq!(pipelined.len(), 2);

        let names: Vec<&str> = pipelined.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"exec-r2-idle"));
        assert!(names.contains(&"exec-r2-void"));
        assert!(!names.contains(&"exec-r4-idle"));
        assert!(!names.contains(&"exec-r2-bound"));
    }

    /// Test that empty filters return empty results.
    #[test]
    fn test_snapshot_empty_filters() {
        let ss = SnapShot::new();

        // Add some executors
        let exec = create_test_executor("exec-1", slots_rr(2), ExecutorState::Idle);
        ss.add_executor(exec).unwrap();

        // Filter by state that doesn't exist
        let releasing_execs = ss
            .find_executors(Some(ExecutorFilter {
                state: Some(ExecutorState::Releasing),
                names: None,
                node: None,
            }))
            .unwrap();
        assert_eq!(releasing_execs.len(), 0);

        // Filter by name that doesn't exist.
        let nonexistent = ss
            .find_executors(Some(ExecutorFilter {
                state: None,
                names: Some(vec!["nonexistent".to_string()]),
                node: None,
            }))
            .unwrap();
        assert_eq!(nonexistent.len(), 0);
    }

    /// Test that delete_executor removes from both main map and index.
    #[test]
    fn test_snapshot_delete_executor() {
        let ss = SnapShot::new();

        let exec = create_test_executor("exec-1", slots_rr(2), ExecutorState::Idle);
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

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
use std::{env, fmt};

use chrono::{DateTime, Duration, Utc};
#[cfg(target_os = "linux")]
use rustix::system;
use stdng::{lock_ptr, MutexPtr};

use rpc::flame as rpc;

use crate::FlameError;

pub const DEFAULT_MAX_INSTANCES: u32 = 1_000_000;
pub const DEFAULT_DELAY_RELEASE: Duration = Duration::seconds(60);

pub type SessionID = String;
pub type TaskID = i64;
pub type ExecutorID = String;
pub type ApplicationID = String;
pub type TaskPtr = MutexPtr<Task>;
pub type SessionPtr = MutexPtr<Session>;
pub type NodePtr = MutexPtr<Node>;
pub type ApplicationPtr = MutexPtr<Application>;

type Message = bytes::Bytes;
pub type TaskInput = Message;
pub type TaskOutput = Message;
pub type CommonData = Message;

#[derive(Clone, Debug)]
pub struct EventOwner {
    pub task_id: TaskID,
    pub session_id: SessionID,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub code: i32,
    pub message: Option<String>,
    pub creation_time: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct TaskResult {
    pub state: TaskState,
    pub output: Option<TaskOutput>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, strum_macros::Display)]
pub enum ApplicationState {
    #[default]
    Enabled = 0,
    Disabled = 1,
}

#[derive(Clone, Debug, Default, Copy, PartialEq, Eq, Hash)]
pub enum Shim {
    #[default]
    Host = 0,
    Wasm = 1,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationSchema {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Application {
    pub name: String,
    pub workspace: String,
    pub version: u32,
    pub state: ApplicationState,
    pub creation_time: DateTime<Utc>,
    pub shim: Shim, // Required shim type (Host or Wasm)
    pub image: Option<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub environments: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub max_instances: u32,
    pub delay_release: Duration,
    pub schema: Option<ApplicationSchema>,
    pub url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ApplicationAttributes {
    pub shim: Shim, // Required shim type (Host or Wasm)
    pub image: Option<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub environments: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub max_instances: u32,
    pub delay_release: Duration,
    pub schema: Option<ApplicationSchema>,
    pub url: Option<String>,
}

impl Default for ApplicationAttributes {
    fn default() -> Self {
        Self {
            shim: Shim::Host, // Default to Host shim
            image: None,
            description: None,
            labels: vec![],
            command: None,
            arguments: vec![],
            environments: HashMap::new(),
            working_directory: None,
            max_instances: DEFAULT_MAX_INSTANCES,
            delay_release: DEFAULT_DELAY_RELEASE,
            schema: Some(ApplicationSchema::default()),
            url: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionAttributes {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,
    pub common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
}

impl Default for SessionAttributes {
    fn default() -> Self {
        Self {
            id: String::new(),
            application: String::new(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, strum_macros::Display)]
pub enum SessionState {
    #[default]
    Open = 0,
    Closed = 1,
}

#[derive(Clone, Debug, Default)]
pub struct SessionStatus {
    pub state: SessionState,
}

#[derive(Debug, Default)]
pub struct Session {
    pub id: SessionID,
    pub workspace: String,
    pub application: String,
    pub slots: u32,
    pub version: u32,
    pub common_data: Option<CommonData>,
    pub tasks: HashMap<TaskID, TaskPtr>,
    pub tasks_index: HashMap<TaskState, HashMap<TaskID, TaskPtr>>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,

    pub events: Vec<Event>,

    pub status: SessionStatus,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, strum_macros::Display)]
pub enum TaskState {
    #[default]
    Pending = 0,
    Running = 1,
    Succeed = 2,
    Failed = 3,
    Cancelled = 4,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TaskGID {
    pub ssn_id: SessionID,
    pub task_id: TaskID,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: TaskID,
    pub ssn_id: SessionID,
    pub workspace: String,
    pub version: u32,

    pub input: Option<TaskInput>,
    pub output: Option<TaskOutput>,

    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,

    pub events: Vec<Event>,

    pub state: TaskState,
}

impl Task {
    pub fn is_completed(&self) -> bool {
        self.state.is_terminal()
    }

    pub fn gid(&self) -> TaskGID {
        TaskGID {
            ssn_id: self.ssn_id.clone(),
            task_id: self.id,
        }
    }
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq, Hash, strum_macros::Display)]
pub enum ExecutorState {
    #[default]
    Unknown = 0,
    Void = 1,
    Idle = 2,
    Binding = 3,
    Bound = 4,
    Unbinding = 5,
    Releasing = 6,
    Released = 7,
}

#[derive(Clone, Debug)]
pub struct TaskContext {
    pub task_id: String,
    pub session_id: String,
    pub input: Option<TaskInput>,
}

#[derive(Clone, Debug)]
pub struct SessionContext {
    pub session_id: String,
    pub application: ApplicationContext,
    pub slots: u32,
    pub common_data: Option<CommonData>,
}

#[derive(Clone, Debug)]
pub struct ApplicationContext {
    pub name: String,
    pub shim: Shim, // Required shim type for the application
    pub image: Option<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
    pub environments: HashMap<String, String>,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, strum_macros::Display)]
pub enum NodeState {
    #[default]
    Unknown = 0, // Node state unknown (e.g., disconnected, cleanup timer running)
    Ready = 1,    // Node is connected and ready
    NotReady = 2, // Node is not ready (e.g., cleanup completed after timeout)
}

#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    pub arch: String,
    pub os: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceRequirement {
    pub cpu: u64,
    pub memory: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    pub capacity: ResourceRequirement,
    pub allocatable: ResourceRequirement,
    pub info: NodeInfo,
    pub state: NodeState,
}

#[cfg(not(target_os = "linux"))]
fn uname() -> String {
    String::from("unknown-node")
}

#[cfg(target_os = "linux")]
fn uname() -> String {
    system::uname().nodename().to_string_lossy().to_string()
}

#[cfg(not(target_os = "linux"))]
fn totalram() -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn totalram() -> u64 {
    system::sysinfo().totalram
}

impl Node {
    pub fn new() -> Self {
        let name = uname();
        let mut node = Node {
            name,
            state: NodeState::Ready,
            ..Default::default()
        };

        node.refresh();

        node
    }

    pub fn refresh(&mut self) {
        let memory = totalram();
        let cpu = num_cpus::get() as u64;

        let capacity = ResourceRequirement { cpu, memory };
        let allocatable = capacity.clone();

        let info = NodeInfo {
            arch: env::consts::ARCH.to_string(),
            os: env::consts::OS.to_string(),
        };

        self.capacity = capacity;
        self.allocatable = allocatable;
        self.info = info;
    }
}

impl From<ResourceRequirement> for rpc::ResourceRequirement {
    fn from(req: ResourceRequirement) -> Self {
        Self {
            cpu: req.cpu,
            memory: req.memory,
            gpu: 0,
        }
    }
}

impl From<rpc::ResourceRequirement> for ResourceRequirement {
    fn from(req: rpc::ResourceRequirement) -> Self {
        Self {
            cpu: req.cpu,
            memory: req.memory,
        }
    }
}

impl From<&str> for ResourceRequirement {
    fn from(s: &str) -> Self {
        Self::from(&s.to_string())
    }
}

impl From<&String> for ResourceRequirement {
    fn from(s: &String) -> Self {
        let parts = s.split(',');
        let mut cpu = 0;
        let mut memory = 0;
        for p in parts {
            let mut parts = p.split('=').map(|s| s.trim());
            let key = parts.next();
            let value = parts.next();
            match (key, value) {
                (Some("cpu"), Some(value)) => cpu = value.parse::<u64>().unwrap_or(0),
                (Some("memory"), Some(value)) => memory = Self::parse_memory(value),
                (Some("mem"), Some(value)) => memory = Self::parse_memory(value),
                _ => {
                    tracing::error!("Invalid resource requirement: {s}");
                }
            }
        }
        Self { cpu, memory }
    }
}

impl ResourceRequirement {
    pub fn new(slots: u32, unit: &ResourceRequirement) -> Self {
        Self {
            cpu: slots as u64 * unit.cpu,
            memory: slots as u64 * unit.memory,
        }
    }

    pub fn to_slots(&self, unit: &ResourceRequirement) -> u32 {
        (self.cpu / unit.cpu).min(self.memory / unit.memory) as u32
    }

    fn parse_memory(s: &str) -> u64 {
        let s = s.to_lowercase();
        let v = s[..s.len() - 1].parse::<u64>().unwrap_or(0);
        let unit = s[s.len() - 1..].to_string();
        // TODO(k82cn): return error if the unit is not valid.
        match unit.as_str() {
            "k" => v * 1024,
            "m" => v * 1024 * 1024,
            "g" => v * 1024 * 1024 * 1024,
            _ => s.parse::<u64>().unwrap_or(0),
        }
    }
}

impl From<NodeInfo> for rpc::NodeInfo {
    fn from(info: NodeInfo) -> Self {
        Self {
            arch: info.arch,
            os: info.os,
        }
    }
}

impl From<rpc::NodeInfo> for NodeInfo {
    fn from(info: rpc::NodeInfo) -> Self {
        Self {
            arch: info.arch,
            os: info.os,
        }
    }
}

impl From<NodeState> for rpc::NodeState {
    fn from(state: NodeState) -> Self {
        match state {
            NodeState::Unknown => rpc::NodeState::Unknown,
            NodeState::Ready => rpc::NodeState::Ready,
            NodeState::NotReady => rpc::NodeState::NotReady,
        }
    }
}

impl From<rpc::NodeState> for NodeState {
    fn from(state: rpc::NodeState) -> Self {
        match state {
            rpc::NodeState::Unknown => NodeState::Unknown,
            rpc::NodeState::Ready => NodeState::Ready,
            rpc::NodeState::NotReady => NodeState::NotReady,
        }
    }
}

impl From<NodeState> for i32 {
    fn from(state: NodeState) -> Self {
        match state {
            NodeState::Unknown => 0,
            NodeState::Ready => 1,
            NodeState::NotReady => 2,
        }
    }
}

impl From<i32> for NodeState {
    fn from(state: i32) -> Self {
        match state {
            0 => NodeState::Unknown,
            1 => NodeState::Ready,
            2 => NodeState::NotReady,
            _ => NodeState::Unknown,
        }
    }
}

impl From<Node> for rpc::Node {
    fn from(node: Node) -> Self {
        let status = Some(rpc::NodeStatus {
            state: node.state.into(),
            capacity: Some(node.capacity.into()),
            allocatable: Some(node.allocatable.into()),
            info: Some(node.info.into()),
            addresses: vec![],
            last_heartbeat_time: 0,
        });

        Self {
            metadata: Some(rpc::Metadata {
                id: node.name.clone(),
                name: node.name.clone(),
                workspace: Some(WORKSPACE_SYSTEM.to_string()),
            }),
            spec: Some(rpc::NodeSpec {
                hostname: node.name.clone(),
            }),
            status,
        }
    }
}

impl From<&Node> for rpc::Node {
    fn from(node: &Node) -> Self {
        rpc::Node::from(node.clone())
    }
}

impl From<rpc::Node> for Node {
    fn from(node: rpc::Node) -> Self {
        let status = node.status.unwrap_or_default();
        let metadata = node.metadata.unwrap_or_default();
        Self {
            name: metadata.name,
            capacity: status.capacity.unwrap_or_default().into(),
            allocatable: status.allocatable.unwrap_or_default().into(),
            info: status.info.unwrap_or_default().into(),
            state: status.state.into(),
        }
    }
}

impl Session {
    pub fn is_closed(&self) -> bool {
        self.status.state == SessionState::Closed
    }

    pub fn update_task(&mut self, task: &Task) -> Result<(), FlameError> {
        let task_ptr = TaskPtr::new(task.clone().into());

        let old_task_ptr = self.tasks.get(&task.id);
        if let Some(old_task_ptr) = old_task_ptr {
            let old_task = lock_ptr!(old_task_ptr)?;
            if old_task.version >= task.version {
                tracing::debug!(
                    "Update task: <{task_id}> with an old version (old={old_version}, new={new_version}), ignore it.",
                    task_id = task.id,
                    old_version = old_task.version,
                    new_version = task.version
                );
                return Ok(());
            }
        }

        tracing::debug!(
            "Updating task <{}> from state {:?} to {:?} (version {})",
            task.id,
            self.tasks
                .get(&task.id)
                .and_then(|t| lock_ptr!(t).ok())
                .map(|t| t.state),
            task.state,
            task.version
        );

        self.tasks.insert(task.id, task_ptr.clone());
        self.tasks_index.entry(task.state).or_default();

        // Remove the task from all states first.
        for state in self.tasks_index.values_mut() {
            state.remove(&task.id);
        }

        // Add the task to the new state.
        self.tasks_index
            .get_mut(&task.state)
            .unwrap()
            .insert(task.id, task_ptr);

        // Log the current tasks_index state for debugging
        let pending_count = self
            .tasks_index
            .get(&TaskState::Pending)
            .map(|m| m.len())
            .unwrap_or(0);
        let running_count = self
            .tasks_index
            .get(&TaskState::Running)
            .map(|m| m.len())
            .unwrap_or(0);
        tracing::debug!(
            "Session <{}> tasks_index after update: pending={}, running={}",
            self.id,
            pending_count,
            running_count
        );

        Ok(())
    }

    pub fn pop_pending_task(&mut self) -> Option<TaskPtr> {
        let pending_tasks = self.tasks_index.get_mut(&TaskState::Pending)?;
        if let Some((task_id, _)) = pending_tasks.clone().iter().next() {
            return pending_tasks.remove(task_id);
        }

        None
    }

    /// Validate that the provided session attributes match this session's spec.
    /// Returns error if specs don't match.
    pub fn validate_spec(&self, attr: &SessionAttributes) -> Result<(), FlameError> {
        if self.application != attr.application {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: application differs (expected '{}', got '{}')",
                self.id, self.application, attr.application
            )));
        }
        if self.slots != attr.slots {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: slots differs (expected {}, got {})",
                self.id, self.slots, attr.slots
            )));
        }
        if self.min_instances != attr.min_instances {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: min_instances differs (expected {}, got {})",
                self.id, self.min_instances, attr.min_instances
            )));
        }
        if self.max_instances != attr.max_instances {
            return Err(FlameError::InvalidConfig(format!(
                "session <{}> spec mismatch: max_instances differs (expected {:?}, got {:?})",
                self.id, self.max_instances, attr.max_instances
            )));
        }
        Ok(())
    }
}

impl Clone for Session {
    fn clone(&self) -> Self {
        let mut ssn = Session {
            id: self.id.clone(),
            workspace: self.workspace.clone(),
            application: self.application.clone(),
            slots: self.slots,
            version: self.version,
            common_data: self.common_data.clone(),
            tasks: HashMap::new(),
            tasks_index: HashMap::new(),
            creation_time: self.creation_time,
            completion_time: self.completion_time,
            events: self.events.clone(),
            status: self.status.clone(),
            min_instances: self.min_instances,
            max_instances: self.max_instances,
        };

        for (id, t) in &self.tasks {
            match t.lock() {
                Ok(t) => {
                    if let Err(e) = ssn.update_task(&t) {
                        tracing::error!("Failed to update task: <{id}> for session: <{ssn_id}>, ignore it during clone: {e}", ssn_id = self.id);
                    }
                }
                Err(_) => {
                    tracing::error!("Failed to lock task: <{id}> for session: <{ssn_id}>, ignore it during clone.", ssn_id = self.id);
                }
            }
        }

        ssn
    }
}

impl From<Event> for rpc::Event {
    fn from(event: Event) -> Self {
        Self {
            code: event.code,
            message: event.message,
            creation_time: event.creation_time.timestamp(),
        }
    }
}

impl From<rpc::Event> for Event {
    fn from(event: rpc::Event) -> Self {
        Self {
            code: event.code,
            message: event.message,
            creation_time: DateTime::from_timestamp(event.creation_time, 0).unwrap(),
        }
    }
}

impl TryFrom<rpc::Task> for TaskContext {
    type Error = FlameError;

    fn try_from(task: rpc::Task) -> Result<Self, Self::Error> {
        let metadata = task
            .metadata
            .ok_or(FlameError::InvalidConfig("metadata".to_string()))?;

        let spec = task
            .spec
            .ok_or(FlameError::InvalidConfig("spec".to_string()))?;

        Ok(TaskContext {
            task_id: metadata.id.clone(),
            session_id: spec.session_id.to_string(),
            input: spec.input.map(TaskInput::from),
        })
    }
}

impl TryFrom<rpc::Application> for ApplicationContext {
    type Error = FlameError;

    fn try_from(app: rpc::Application) -> Result<Self, Self::Error> {
        let metadata = app
            .metadata
            .ok_or(FlameError::InvalidConfig("metadata".to_string()))?;

        let spec = app
            .spec
            .ok_or(FlameError::InvalidConfig("spec".to_string()))?;

        Ok(ApplicationContext {
            name: metadata.name.clone(),
            shim: Shim::from(spec.shim()), // Get shim from spec
            image: spec.image.clone(),
            command: spec.command.clone(),
            arguments: spec.arguments.clone(),
            working_directory: spec.working_directory.clone(),
            environments: spec
                .environments
                .clone()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            url: spec.url.clone(),
        })
    }
}

impl From<TaskContext> for rpc::TaskContext {
    fn from(ctx: TaskContext) -> Self {
        Self {
            task_id: ctx.task_id.clone(),
            session_id: ctx.session_id.clone(),
            input: ctx.input.map(|d| d.into()),
        }
    }
}

impl From<SessionContext> for rpc::SessionContext {
    fn from(ctx: SessionContext) -> Self {
        Self {
            session_id: ctx.session_id.clone(),
            application: Some(ctx.application.into()),
            common_data: ctx.common_data.map(|d| d.into()),
        }
    }
}

impl From<ApplicationContext> for rpc::ApplicationContext {
    fn from(ctx: ApplicationContext) -> Self {
        Self {
            name: ctx.name.clone(),
            shim: rpc::Shim::from(ctx.shim).into(), // Include shim in context
            image: ctx.image.clone(),
            command: ctx.command.clone(),
            working_directory: ctx.working_directory.clone(),
            url: ctx.url.clone(),
        }
    }
}

impl TryFrom<(rpc::Application, rpc::Session)> for SessionContext {
    type Error = FlameError;
    fn try_from((app, ssn): (rpc::Application, rpc::Session)) -> Result<Self, Self::Error> {
        let metadata = ssn
            .metadata
            .ok_or(FlameError::InvalidConfig("metadata".to_string()))?;

        let spec = ssn
            .spec
            .ok_or(FlameError::InvalidConfig("spec".to_string()))?;

        let application = ApplicationContext::try_from(app)?;

        Ok(SessionContext {
            session_id: metadata.id,
            application,
            slots: spec.slots,
            common_data: spec.common_data.map(CommonData::from),
        })
    }
}

impl From<Task> for rpc::Task {
    fn from(task: Task) -> Self {
        rpc::Task::from(&task)
    }
}

impl From<&Task> for rpc::Task {
    fn from(task: &Task) -> Self {
        let metadata = Some(rpc::Metadata {
            id: task.id.to_string(),
            name: task.id.to_string(),
            workspace: Some(task.workspace.clone()),
        });

        let spec = Some(rpc::TaskSpec {
            session_id: task.ssn_id.to_string(),
            input: task.input.clone().map(TaskInput::into),
            output: task.output.clone().map(TaskOutput::into),
        });
        let status = Some(rpc::TaskStatus {
            state: task.state as i32,
            creation_time: task.creation_time.timestamp(),
            completion_time: task.completion_time.map(|s| s.timestamp()),
            events: task.events.clone().into_iter().map(Event::into).collect(),
        });
        rpc::Task {
            metadata,
            spec,
            status,
        }
    }
}

impl From<Session> for rpc::Session {
    fn from(ssn: Session) -> Self {
        rpc::Session::from(&ssn)
    }
}

impl From<&Session> for rpc::Session {
    fn from(ssn: &Session) -> Self {
        let mut status = rpc::SessionStatus {
            state: ssn.status.state as i32,
            creation_time: ssn.creation_time.timestamp(),
            completion_time: ssn.completion_time.map(|s| s.timestamp()),
            failed: 0,
            pending: 0,
            running: 0,
            succeed: 0,
            cancelled: 0,
            events: ssn.events.clone().into_iter().map(Event::into).collect(),
        };
        for (s, v) in &ssn.tasks_index {
            match s {
                TaskState::Pending => status.pending = v.len() as i32,
                TaskState::Running => status.running = v.len() as i32,
                TaskState::Succeed => status.succeed = v.len() as i32,
                TaskState::Failed => status.failed = v.len() as i32,
                TaskState::Cancelled => status.cancelled = v.len() as i32,
            }
        }

        rpc::Session {
            metadata: Some(rpc::Metadata {
                id: ssn.id.to_string(),
                name: ssn.id.to_string(),
                workspace: Some(ssn.workspace.clone()),
            }),
            spec: Some(rpc::SessionSpec {
                application: ssn.application.clone(),
                slots: ssn.slots,
                common_data: ssn.common_data.clone().map(CommonData::into),
                min_instances: ssn.min_instances,
                max_instances: ssn.max_instances,
                credential: None,
            }),
            status: Some(status),
        }
    }
}

impl From<ApplicationSchema> for rpc::ApplicationSchema {
    fn from(schema: ApplicationSchema) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

impl From<rpc::ApplicationSchema> for ApplicationSchema {
    fn from(schema: rpc::ApplicationSchema) -> Self {
        Self {
            input: schema.input,
            output: schema.output,
            common_data: schema.common_data,
        }
    }
}

impl TryFrom<rpc::Application> for Application {
    type Error = FlameError;
    fn try_from(app: rpc::Application) -> Result<Self, Self::Error> {
        Application::try_from(&app)
    }
}

impl TryFrom<&rpc::Application> for Application {
    type Error = FlameError;
    fn try_from(app: &rpc::Application) -> Result<Self, Self::Error> {
        let metadata = app.metadata.clone().ok_or(FlameError::InvalidConfig(
            "application metadata is empty".to_string(),
        ))?;

        let spec = app.spec.clone().ok_or(FlameError::InvalidConfig(
            "application spec is empty".to_string(),
        ))?;

        let status = app.status.ok_or(FlameError::InvalidConfig(
            "application status is empty".to_string(),
        ))?;

        Ok(Application {
            name: metadata.name.clone(),
            workspace: metadata
                .workspace
                .clone()
                .unwrap_or_else(|| WORKSPACE_DEFAULT.to_string()),
            version: 0,
            state: ApplicationState::from(status.state()),
            creation_time: DateTime::<Utc>::from_timestamp(status.creation_time, 0).ok_or(
                FlameError::InvalidState("invalid creation time".to_string()),
            )?,
            shim: Shim::from(spec.shim()),
            image: spec.image.clone(),
            description: spec.description.clone(),
            labels: spec.labels.clone(),
            command: spec.command.clone(),
            arguments: spec.arguments.to_vec(),
            environments: spec
                .environments
                .clone()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            working_directory: spec.working_directory,
            max_instances: spec.max_instances.unwrap_or(DEFAULT_MAX_INSTANCES),
            delay_release: spec
                .delay_release
                .map(Duration::seconds)
                .unwrap_or(DEFAULT_DELAY_RELEASE),
            schema: spec.schema.map(ApplicationSchema::from),
            url: spec.url.clone(),
        })
    }
}

impl From<Application> for rpc::Application {
    fn from(app: Application) -> Self {
        rpc::Application::from(&app)
    }
}

impl From<&Application> for rpc::Application {
    fn from(app: &Application) -> Self {
        let spec = Some(rpc::ApplicationSpec {
            shim: rpc::Shim::from(app.shim).into(), // Include shim in spec
            image: app.image.clone(),
            description: app.description.clone(),
            labels: app.labels.clone(),
            command: app.command.clone(),
            arguments: app.arguments.to_vec(),
            environments: app
                .environments
                .clone()
                .into_iter()
                .map(|(k, v)| rpc::Environment { name: k, value: v })
                .collect(),
            working_directory: app.working_directory.clone(),
            max_instances: Some(app.max_instances),
            delay_release: Some(app.delay_release.num_seconds()),
            schema: app.schema.clone().map(rpc::ApplicationSchema::from),
            url: app.url.clone(),
        });
        let metadata = Some(rpc::Metadata {
            id: app.name.clone(),
            name: app.name.clone(),
            workspace: Some(app.workspace.clone()),
        });

        let status = Some(rpc::ApplicationStatus {
            state: app.state.into(),
            creation_time: app.creation_time.timestamp(),
        });
        rpc::Application {
            metadata,
            spec,
            status,
        }
    }
}

impl From<rpc::ApplicationSpec> for ApplicationAttributes {
    fn from(spec: rpc::ApplicationSpec) -> Self {
        Self {
            shim: Shim::from(spec.shim()), // Get shim from spec
            image: spec.image.clone(),
            description: spec.description.clone(),
            labels: spec.labels.clone(),
            command: spec.command.clone(),
            arguments: spec.arguments.clone(),
            environments: spec
                .environments
                .clone()
                .into_iter()
                .map(|e| (e.name, e.value))
                .collect(),
            // Treat empty string as None due to protobuf limitation
            working_directory: spec.working_directory.clone().filter(|wd| !wd.is_empty()),
            max_instances: spec.max_instances.unwrap_or(DEFAULT_MAX_INSTANCES),
            delay_release: spec
                .delay_release
                .map(Duration::seconds)
                .unwrap_or(DEFAULT_DELAY_RELEASE),
            schema: spec.schema.map(ApplicationSchema::from),
            url: spec.url.clone(),
        }
    }
}

impl From<ApplicationState> for rpc::ApplicationState {
    fn from(s: ApplicationState) -> Self {
        match s {
            ApplicationState::Disabled => Self::Disabled,
            ApplicationState::Enabled => Self::Enabled,
        }
    }
}

impl From<rpc::ApplicationState> for ApplicationState {
    fn from(s: rpc::ApplicationState) -> Self {
        match s {
            rpc::ApplicationState::Disabled => Self::Disabled,
            rpc::ApplicationState::Enabled => Self::Enabled,
        }
    }
}

impl TryFrom<i32> for ApplicationState {
    type Error = FlameError;
    fn try_from(s: i32) -> Result<Self, Self::Error> {
        let state = rpc::ApplicationState::try_from(s)
            .map_err(|_| FlameError::InvalidState("unknown application state".to_string()))?;
        Ok(Self::from(state))
    }
}
impl From<ApplicationState> for i32 {
    fn from(s: ApplicationState) -> Self {
        s as i32
    }
}

impl From<rpc::SessionState> for SessionState {
    fn from(s: rpc::SessionState) -> Self {
        match s {
            rpc::SessionState::Open => SessionState::Open,
            rpc::SessionState::Closed => SessionState::Closed,
        }
    }
}

impl From<SessionState> for rpc::SessionState {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Open => rpc::SessionState::Open,
            SessionState::Closed => rpc::SessionState::Closed,
        }
    }
}
impl TryFrom<i32> for SessionState {
    type Error = FlameError;
    fn try_from(s: i32) -> Result<Self, Self::Error> {
        let state = rpc::SessionState::try_from(s)
            .map_err(|_| FlameError::InvalidState("invalid session state".to_string()))?;

        Ok(Self::from(state))
    }
}

impl From<SessionState> for i32 {
    fn from(s: SessionState) -> Self {
        s as i32
    }
}

impl From<rpc::TaskState> for TaskState {
    fn from(s: rpc::TaskState) -> Self {
        match s {
            rpc::TaskState::Pending => TaskState::Pending,
            rpc::TaskState::Running => TaskState::Running,
            rpc::TaskState::Succeed => TaskState::Succeed,
            rpc::TaskState::Failed => TaskState::Failed,
            rpc::TaskState::Cancelled => TaskState::Cancelled,
        }
    }
}

impl From<TaskState> for rpc::TaskState {
    fn from(state: TaskState) -> Self {
        match state {
            TaskState::Pending => rpc::TaskState::Pending,
            TaskState::Running => rpc::TaskState::Running,
            TaskState::Succeed => rpc::TaskState::Succeed,
            TaskState::Failed => rpc::TaskState::Failed,
            TaskState::Cancelled => rpc::TaskState::Cancelled,
        }
    }
}

impl TryFrom<i32> for TaskState {
    type Error = FlameError;
    fn try_from(s: i32) -> Result<Self, Self::Error> {
        let state = rpc::TaskState::try_from(s)
            .map_err(|_| FlameError::InvalidState("invalid task state".to_string()))?;

        Ok(Self::from(state))
    }
}

impl From<TaskState> for i32 {
    fn from(s: TaskState) -> Self {
        s as i32
    }
}

impl TryFrom<String> for Shim {
    type Error = FlameError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "host" => Ok(Self::Host),
            "wasm" => Ok(Self::Wasm),
            _ => Err(FlameError::InvalidConfig(format!("invalid shim: {s}"))),
        }
    }
}

impl From<rpc::Shim> for Shim {
    fn from(s: rpc::Shim) -> Self {
        match s {
            rpc::Shim::Host => Self::Host,
            rpc::Shim::Wasm => Self::Wasm,
        }
    }
}

impl From<Shim> for rpc::Shim {
    fn from(s: Shim) -> Self {
        match s {
            Shim::Host => Self::Host,
            Shim::Wasm => Self::Wasm,
        }
    }
}

impl TryFrom<i32> for Shim {
    type Error = FlameError;

    fn try_from(v: i32) -> Result<Self, Self::Error> {
        let s = rpc::Shim::try_from(v)
            .map_err(|_| FlameError::InvalidState("invalid shim".to_string()))?;
        Ok(Self::from(s))
    }
}

impl From<Shim> for i32 {
    fn from(s: Shim) -> Self {
        s as i32
    }
}

impl fmt::Display for TaskGID {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.ssn_id, self.task_id)
    }
}

impl From<rpc::ExecutorState> for ExecutorState {
    fn from(s: rpc::ExecutorState) -> Self {
        match s {
            rpc::ExecutorState::ExecutorVoid => ExecutorState::Void,
            rpc::ExecutorState::ExecutorIdle => ExecutorState::Idle,
            rpc::ExecutorState::ExecutorBinding => ExecutorState::Binding,
            rpc::ExecutorState::ExecutorBound => ExecutorState::Bound,
            rpc::ExecutorState::ExecutorUnbinding => ExecutorState::Unbinding,
            rpc::ExecutorState::ExecutorReleasing => ExecutorState::Releasing,
            rpc::ExecutorState::ExecutorReleased => ExecutorState::Released,
            _ => ExecutorState::Unknown,
        }
    }
}

impl From<ExecutorState> for rpc::ExecutorState {
    fn from(s: ExecutorState) -> Self {
        match s {
            ExecutorState::Void => rpc::ExecutorState::ExecutorVoid,
            ExecutorState::Idle => rpc::ExecutorState::ExecutorIdle,
            ExecutorState::Binding => rpc::ExecutorState::ExecutorBinding,
            ExecutorState::Bound => rpc::ExecutorState::ExecutorBound,
            ExecutorState::Unbinding => rpc::ExecutorState::ExecutorUnbinding,
            ExecutorState::Releasing => rpc::ExecutorState::ExecutorReleasing,
            ExecutorState::Released => rpc::ExecutorState::ExecutorReleased,
            _ => rpc::ExecutorState::ExecutorUnknown,
        }
    }
}
impl From<i32> for ExecutorState {
    fn from(s: i32) -> Self {
        match s {
            1 => ExecutorState::Void,
            2 => ExecutorState::Idle,
            3 => ExecutorState::Binding,
            4 => ExecutorState::Bound,
            5 => ExecutorState::Unbinding,
            6 => ExecutorState::Releasing,
            7 => ExecutorState::Released,
            _ => ExecutorState::Unknown,
        }
    }
}

impl From<ExecutorState> for i32 {
    fn from(s: ExecutorState) -> Self {
        s as i32
    }
}

impl From<rpc::TaskResult> for TaskResult {
    fn from(result: rpc::TaskResult) -> Self {
        let state = if result.return_code != 0 {
            TaskState::Failed
        } else {
            TaskState::Succeed
        };

        Self {
            state,
            output: result.output.map(TaskOutput::from),
            message: result.message,
        }
    }
}

impl TryFrom<TaskResult> for rpc::TaskResult {
    type Error = FlameError;

    fn try_from(result: TaskResult) -> Result<Self, Self::Error> {
        let return_code = match result.state {
            TaskState::Failed => -1,
            TaskState::Succeed => 0,
            _ => {
                return Err(FlameError::InvalidState(format!(
                    "invalid task state: {:?}",
                    result.state
                )))
            }
        };

        Ok(Self {
            return_code,
            output: result.output.map(TaskOutput::into),
            message: result.message,
        })
    }
}

impl From<&Task> for EventOwner {
    fn from(task: &Task) -> Self {
        Self {
            task_id: task.id,
            session_id: task.ssn_id.clone(),
        }
    }
}

impl From<&TaskGID> for EventOwner {
    fn from(gid: &TaskGID) -> Self {
        Self {
            task_id: gid.task_id,
            session_id: gid.ssn_id.clone(),
        }
    }
}

impl From<Task> for EventOwner {
    fn from(task: Task) -> Self {
        Self::from(&task)
    }
}

impl From<TaskGID> for EventOwner {
    fn from(gid: TaskGID) -> Self {
        Self::from(&gid)
    }
}

// ============================================================
// RBAC: User, Role, Workspace
// ============================================================

/// Pre-defined workspace constants
pub const WORKSPACE_DEFAULT: &str = "default";
pub const WORKSPACE_SYSTEM: &str = "system";

/// User represents an authenticated identity
#[derive(Clone, Debug, Default)]
pub struct User {
    pub name: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub certificate_cn: String,
    pub enabled: bool,
    pub creation_time: DateTime<Utc>,
    pub last_login_time: Option<DateTime<Utc>>,
    /// Role names assigned to this user
    pub roles: Vec<String>,
}

/// Role defines a named collection of permissions for workspaces
#[derive(Clone, Debug, Default)]
pub struct Role {
    pub name: String,
    pub description: Option<String>,
    /// Permission strings (e.g., "session:create", "application:*")
    pub permissions: Vec<String>,
    /// Workspaces this role grants access to ("*" for all)
    pub workspaces: Vec<String>,
    pub creation_time: DateTime<Utc>,
}

impl Role {
    /// Check if this role has the required permission
    pub fn has_permission(&self, required: &str) -> bool {
        let (req_resource, req_action) = required.split_once(':').unwrap_or((required, "*"));

        for perm in &self.permissions {
            let (perm_resource, perm_action) = perm.split_once(':').unwrap_or((perm, "*"));

            let resource_match = perm_resource == "*" || perm_resource == req_resource;
            let action_match = perm_action == "*" || perm_action == req_action;

            if resource_match && action_match {
                return true;
            }
        }

        false
    }

    /// Check if this role grants access to the specified workspace
    pub fn has_workspace(&self, workspace: &str) -> bool {
        self.workspaces.iter().any(|w| w == "*" || w == workspace)
    }
}

/// Workspace represents a logical isolation boundary
#[derive(Clone, Debug, Default)]
pub struct Workspace {
    pub name: String,
    pub description: Option<String>,
    pub labels: HashMap<String, String>,
    pub creation_time: DateTime<Utc>,
}

impl From<User> for rpc::User {
    fn from(user: User) -> Self {
        rpc::User::from(&user)
    }
}

impl From<&User> for rpc::User {
    fn from(user: &User) -> Self {
        rpc::User {
            metadata: Some(rpc::Metadata {
                id: user.name.clone(),
                name: user.name.clone(),
                workspace: None,
            }),
            spec: Some(rpc::UserSpec {
                display_name: user.display_name.clone().unwrap_or_default(),
                email: user.email.clone().unwrap_or_default(),
                role_refs: user.roles.clone(),
                certificate_cn: user.certificate_cn.clone(),
            }),
            status: Some(rpc::UserStatus {
                creation_time: user.creation_time.timestamp(),
                last_login_time: user.last_login_time.map(|t| t.timestamp()),
                enabled: user.enabled,
            }),
        }
    }
}

impl TryFrom<rpc::User> for User {
    type Error = FlameError;

    fn try_from(user: rpc::User) -> Result<Self, Self::Error> {
        let metadata = user
            .metadata
            .ok_or_else(|| FlameError::InvalidConfig("user metadata is empty".to_string()))?;
        let spec = user
            .spec
            .ok_or_else(|| FlameError::InvalidConfig("user spec is empty".to_string()))?;
        let status = user
            .status
            .ok_or_else(|| FlameError::InvalidConfig("user status is empty".to_string()))?;

        Ok(User {
            name: metadata.name,
            display_name: if spec.display_name.is_empty() {
                None
            } else {
                Some(spec.display_name)
            },
            email: if spec.email.is_empty() {
                None
            } else {
                Some(spec.email)
            },
            certificate_cn: spec.certificate_cn,
            enabled: status.enabled,
            creation_time: DateTime::from_timestamp(status.creation_time, 0)
                .ok_or_else(|| FlameError::InvalidState("invalid creation time".to_string()))?,
            last_login_time: status
                .last_login_time
                .and_then(|t| DateTime::from_timestamp(t, 0)),
            roles: spec.role_refs,
        })
    }
}

impl From<Role> for rpc::Role {
    fn from(role: Role) -> Self {
        rpc::Role::from(&role)
    }
}

impl From<&Role> for rpc::Role {
    fn from(role: &Role) -> Self {
        rpc::Role {
            metadata: Some(rpc::Metadata {
                id: role.name.clone(),
                name: role.name.clone(),
                workspace: None,
            }),
            spec: Some(rpc::RoleSpec {
                description: role.description.clone().unwrap_or_default(),
                permissions: role.permissions.clone(),
                workspaces: role.workspaces.clone(),
            }),
            status: Some(rpc::RoleStatus {
                creation_time: role.creation_time.timestamp(),
                user_count: 0,
            }),
        }
    }
}

impl TryFrom<rpc::Role> for Role {
    type Error = FlameError;

    fn try_from(role: rpc::Role) -> Result<Self, Self::Error> {
        let metadata = role
            .metadata
            .ok_or_else(|| FlameError::InvalidConfig("role metadata is empty".to_string()))?;
        let spec = role
            .spec
            .ok_or_else(|| FlameError::InvalidConfig("role spec is empty".to_string()))?;
        let status = role
            .status
            .ok_or_else(|| FlameError::InvalidConfig("role status is empty".to_string()))?;

        Ok(Role {
            name: metadata.name,
            description: if spec.description.is_empty() {
                None
            } else {
                Some(spec.description)
            },
            permissions: spec.permissions,
            workspaces: spec.workspaces,
            creation_time: DateTime::from_timestamp(status.creation_time, 0)
                .ok_or_else(|| FlameError::InvalidState("invalid creation time".to_string()))?,
        })
    }
}

impl From<Workspace> for rpc::Workspace {
    fn from(ws: Workspace) -> Self {
        rpc::Workspace::from(&ws)
    }
}

impl From<&Workspace> for rpc::Workspace {
    fn from(ws: &Workspace) -> Self {
        rpc::Workspace {
            metadata: Some(rpc::Metadata {
                id: ws.name.clone(),
                name: ws.name.clone(),
                workspace: None,
            }),
            spec: Some(rpc::WorkspaceSpec {
                description: ws.description.clone().unwrap_or_default(),
                labels: ws.labels.clone(),
            }),
            status: Some(rpc::WorkspaceStatus {
                creation_time: ws.creation_time.timestamp(),
                session_count: 0,
                application_count: 0,
            }),
        }
    }
}

impl TryFrom<rpc::Workspace> for Workspace {
    type Error = FlameError;

    fn try_from(ws: rpc::Workspace) -> Result<Self, Self::Error> {
        let metadata = ws
            .metadata
            .ok_or_else(|| FlameError::InvalidConfig("workspace metadata is empty".to_string()))?;
        let spec = ws
            .spec
            .ok_or_else(|| FlameError::InvalidConfig("workspace spec is empty".to_string()))?;
        let status = ws
            .status
            .ok_or_else(|| FlameError::InvalidConfig("workspace status is empty".to_string()))?;

        Ok(Workspace {
            name: metadata.name,
            description: if spec.description.is_empty() {
                None
            } else {
                Some(spec.description)
            },
            labels: spec.labels,
            creation_time: DateTime::from_timestamp(status.creation_time, 0)
                .ok_or_else(|| FlameError::InvalidState("invalid creation time".to_string()))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resreq_from_string() {
        let cases = vec![
            ("cpu=1,mem=256", (1, 256)),
            ("cpu=1,mem=1k", (1, 1024)),
            ("cpu=1,memory=1m", (1, 1024 * 1024)),
            ("cpu=1,memory=1g", (1, 1024 * 1024 * 1024)),
        ];

        for (input, expected) in cases {
            let resreq = ResourceRequirement::from(input);
            assert_eq!(resreq.cpu, expected.0);
            assert_eq!(resreq.memory, expected.1);
        }
    }

    #[test]
    fn test_shim_default() {
        let shim = Shim::default();
        assert_eq!(shim, Shim::Host);
    }

    #[test]
    fn test_shim_from_string() {
        assert_eq!(Shim::try_from("host".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("Host".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("HOST".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("wasm".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("Wasm".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("WASM".to_string()).unwrap(), Shim::Wasm);
        assert!(Shim::try_from("invalid".to_string()).is_err());
    }

    #[test]
    fn test_application_attributes_default_shim() {
        let attrs = ApplicationAttributes::default();
        assert_eq!(attrs.shim, Shim::Host);
    }

    #[test]
    fn test_role_has_permission() {
        let role = Role {
            name: "developer".to_string(),
            permissions: vec!["session:*".to_string(), "application:read".to_string()],
            workspaces: vec!["team-a".to_string()],
            ..Default::default()
        };

        assert!(role.has_permission("session:create"));
        assert!(role.has_permission("session:delete"));
        assert!(role.has_permission("application:read"));
        assert!(!role.has_permission("application:create"));
        assert!(!role.has_permission("workspace:create"));
    }

    #[test]
    fn test_role_has_permission_wildcard() {
        let role = Role {
            name: "admin".to_string(),
            permissions: vec!["*:*".to_string()],
            workspaces: vec!["*".to_string()],
            ..Default::default()
        };

        assert!(role.has_permission("session:create"));
        assert!(role.has_permission("application:delete"));
        assert!(role.has_permission("workspace:update"));
    }

    #[test]
    fn test_role_has_workspace() {
        let role = Role {
            name: "developer".to_string(),
            permissions: vec!["session:*".to_string()],
            workspaces: vec!["team-a".to_string(), "team-b".to_string()],
            ..Default::default()
        };

        assert!(role.has_workspace("team-a"));
        assert!(role.has_workspace("team-b"));
        assert!(!role.has_workspace("team-c"));
    }

    #[test]
    fn test_role_has_workspace_wildcard() {
        let role = Role {
            name: "admin".to_string(),
            permissions: vec!["*:*".to_string()],
            workspaces: vec!["*".to_string()],
            ..Default::default()
        };

        assert!(role.has_workspace("any-workspace"));
        assert!(role.has_workspace("production"));
    }
}

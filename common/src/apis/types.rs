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

use std::collections::{BTreeMap, HashMap};
use std::{env, fmt};

use chrono::{DateTime, Duration, Utc};
#[cfg(target_os = "linux")]
use rustix::system;
use stdng::MutexPtr;

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

#[derive(Clone, Debug, Default)]
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
    pub version: u32,
    pub state: ApplicationState,
    pub creation_time: DateTime<Utc>,
    pub shim: Shim,
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
    pub shim: Shim,
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
            shim: Shim::Host,
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
    pub batch_size: u32,
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
            batch_size: 1,
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
    pub application: String,
    pub slots: u32,
    pub version: u32,
    pub common_data: Option<CommonData>,
    pub tasks: HashMap<TaskID, TaskPtr>,
    pub tasks_index: HashMap<TaskState, BTreeMap<TaskID, TaskPtr>>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub events: Vec<Event>,
    pub status: SessionStatus,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
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
    pub version: u32,
    pub input: Option<TaskInput>,
    pub output: Option<TaskOutput>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub events: Vec<Event>,
    pub state: TaskState,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: 0,
            ssn_id: String::new(),
            version: 0,
            input: None,
            output: None,
            creation_time: Utc::now(),
            completion_time: None,
            events: Vec::new(),
            state: TaskState::default(),
        }
    }
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
    pub shim: Shim,
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
    Unknown = 0,
    Ready = 1,
    NotReady = 2,
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

    pub(crate) fn parse_memory(s: &str) -> u64 {
        if s.is_empty() {
            return 0;
        }
        let s = s.to_lowercase();
        let v = s[..s.len() - 1].parse::<u64>().unwrap_or(0);
        let unit = s[s.len() - 1..].to_string();
        match unit.as_str() {
            "k" => v * 1024,
            "m" => v * 1024 * 1024,
            "g" => v * 1024 * 1024 * 1024,
            _ => s.parse::<u64>().unwrap_or(0),
        }
    }
}

impl fmt::Display for TaskGID {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.ssn_id, self.task_id)
    }
}

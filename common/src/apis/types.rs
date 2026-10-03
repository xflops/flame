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
use std::env;

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
#[cfg(target_os = "linux")]
use rustix::system;
use stdng::MutexPtr;
use uuid::Uuid;

pub const DEFAULT_MAX_INSTANCES: u32 = 1_000_000;
pub const DEFAULT_DELAY_RELEASE: Duration = Duration::seconds(60);
pub const BIND_RESULT_OK: i32 = 0;
pub const BIND_RESULT_SHIM_CREATE_FAILED: i32 = 11;
pub const BIND_RESULT_ON_SESSION_ENTER_FAILED: i32 = 12;
pub const BIND_RESULT_UNKNOWN_FAILED: i32 = 19;
pub const SESSION_BIND_FAILED: i32 = 1001;
pub const SESSION_RETRY_LIMIT_REACHED: i32 = 1002;

pub type ExecutorID = String;
pub type ApplicationID = String;
/// Numeric task name, scoped to its owning session.
pub type TaskName = u64;
pub const DEFAULT_WORKSPACE: &str = "default";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub create_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionGID {
    pub workspace: String,
    pub session: String,
}

impl SessionGID {
    pub fn new(workspace: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            session: session.into(),
        }
    }
}

impl std::fmt::Display for SessionGID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.workspace, self.session)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ExecutorGID {
    pub workspace: String,
    pub executor: String,
}

impl ExecutorGID {
    pub fn new(workspace: impl Into<String>, executor: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            executor: executor.into(),
        }
    }
}

pub fn new_metadata_id() -> String {
    Uuid::new_v4().to_string()
}
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
    pub workspace: String,
    pub session: String,
    pub task: Option<String>,
}

impl EventOwner {
    pub fn session(workspace: String, session: String) -> Self {
        Self {
            workspace,
            session,
            task: None,
        }
    }
    pub fn task(workspace: String, session: String, task: String) -> Self {
        Self {
            workspace,
            session,
            task: Some(task),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub code: i32,
    pub message: Option<String>,
    pub creation_time: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlameResult {
    pub return_code: i32,
    pub message: Option<String>,
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
    Cri = 3,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationSchema {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Application {
    pub id: String,
    pub workspace: String,
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
    pub installer: Option<String>,
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
    pub installer: Option<String>,
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
            installer: None,
        }
    }
}

/// Cache package references must stay inside the application's workspace.
pub fn validate_application_url(
    workspace: &str,
    value: Option<&str>,
) -> Result<(), crate::FlameError> {
    let Some(value) = value else {
        return Ok(());
    };
    let scheme = value.split(':').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "grpc" | "grpcs" | "grpc+tls" | "grpcs-proxy"
    ) {
        return Ok(());
    }
    let parsed = url::Url::parse(value).map_err(|error| {
        crate::FlameError::InvalidConfig(format!("invalid application cache URL: {error}"))
    })?;
    let parts: Vec<_> = parsed.path().trim_start_matches('/').split('/').collect();
    if parsed.host_str().is_none()
        || parts.len() != 4
        || parts.iter().any(|part| part.is_empty())
        || parts[0] != workspace
    {
        return Err(crate::FlameError::InvalidConfig(format!(
            "application cache URL must reference an object in workspace {workspace}"
        )));
    }
    Ok(())
}

#[derive(Clone)]
pub struct SessionAttributes {
    pub workspace: String,
    pub name: String,
    pub application: String,
    pub common_data: Option<CommonData>,
    pub tokens: HashMap<String, String>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,
    pub resreq: Option<ResourceRequirement>,
}

impl std::fmt::Debug for SessionAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAttributes")
            .field("name", &self.name)
            .field("application", &self.application)
            .field("tokens", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Default for SessionAttributes {
    fn default() -> Self {
        Self {
            workspace: DEFAULT_WORKSPACE.to_string(),
            name: String::new(),
            application: String::new(),
            common_data: None,
            tokens: HashMap::new(),
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
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

#[derive(Default, Clone)]
pub struct Session {
    pub id: String,
    pub workspace: String,
    pub name: String,
    pub application: String,
    pub version: u32,
    pub common_data: Option<CommonData>,
    pub tokens: HashMap<String, String>,
    pub tasks: HashMap<TaskName, TaskPtr>,
    pub tasks_index: HashMap<TaskState, BTreeMap<TaskName, TaskPtr>>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub events: Vec<Event>,
    pub status: SessionStatus,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,
    pub resreq: Option<ResourceRequirement>,
    pub retry_count: u32,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("application", &self.application)
            .field("status", &self.status)
            .field("tokens", &"<redacted>")
            .finish_non_exhaustive()
    }
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

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub workspace: String,
    pub session: String,
    pub name: TaskName,
    pub version: u32,
    pub input: Option<TaskInput>,
    pub output: Option<TaskOutput>,
    /// Opaque application-defined data-locality keys.
    pub affinity: HashSet<Bytes>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub events: Vec<Event>,
    pub state: TaskState,
}

/// Extensible controls supplied when a task is created.
#[derive(Clone, Debug, Default)]
pub struct TaskOptions {
    pub affinity: HashSet<Bytes>,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: String::new(),
            workspace: DEFAULT_WORKSPACE.to_string(),
            session: String::new(),
            name: 0,
            version: 0,
            input: None,
            output: None,
            affinity: HashSet::new(),
            creation_time: Utc::now(),
            completion_time: None,
            events: Vec::new(),
            state: TaskState::default(),
        }
    }
}

impl Task {
    pub fn session(&self) -> SessionGID {
        SessionGID::new(&self.workspace, &self.session)
    }

    pub fn is_completed(&self) -> bool {
        self.state.is_terminal()
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
    pub task: String,
    pub session: String,
    pub workspace: String,
    pub input: Option<TaskInput>,
}

#[derive(Clone)]
pub struct SessionContext {
    pub session: String,
    pub workspace: String,
    pub application: ApplicationContext,
    pub common_data: Option<CommonData>,
    pub tokens: HashMap<String, String>,
}

impl std::fmt::Debug for SessionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContext")
            .field("session", &self.session)
            .field("application", &self.application)
            .field("tokens", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct ApplicationContext {
    pub name: String,
    pub workspace: String,
    pub shim: Shim,
    pub image: Option<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
    pub environments: HashMap<String, String>,
    pub url: Option<String>,
    pub installer: Option<String>,
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
    pub gpu: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub id: String,
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

fn detect_gpus() -> i32 {
    if let Ok(devices) = std::env::var("CUDA_VISIBLE_DEVICES") {
        if devices.is_empty() || devices == "-1" {
            return 0;
        }
        return devices.split(',').count() as i32;
    }

    match nvml_wrapper::Nvml::init() {
        Ok(nvml) => match nvml.device_count() {
            Ok(count) => {
                tracing::info!("Detected {} GPU(s) via NVML", count);
                count as i32
            }
            Err(e) => {
                tracing::warn!("NVML initialized but failed to get device count: {}", e);
                0
            }
        },
        Err(e) => {
            tracing::debug!("NVML not available, GPU detection skipped: {}", e);
            0
        }
    }
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
        let gpu = detect_gpus();
        let capacity = ResourceRequirement { cpu, memory, gpu };
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
        Self::parse(s).unwrap_or_else(|e| {
            tracing::error!("Invalid resource requirement <{}>: {}", s, e);
            Self::default()
        })
    }
}

impl ResourceRequirement {
    pub fn parse(s: &str) -> Result<Self, crate::FlameError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(crate::FlameError::InvalidConfig(
                "resource requirement cannot be empty".to_string(),
            ));
        }

        let mut cpu = 0;
        let mut memory = 0;
        let mut gpu = 0;
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(crate::FlameError::InvalidConfig(format!(
                    "invalid empty resource requirement entry in '{s}'"
                )));
            }

            let (key, value) = part.split_once('=').ok_or_else(|| {
                crate::FlameError::InvalidConfig(format!(
                    "invalid resource requirement entry '{part}', expected key=value"
                ))
            })?;
            if value.contains('=') {
                return Err(crate::FlameError::InvalidConfig(format!(
                    "invalid resource requirement entry '{part}', expected one '='"
                )));
            }

            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if value.is_empty() {
                return Err(crate::FlameError::InvalidConfig(format!(
                    "missing value for resource requirement key '{key}'"
                )));
            }

            match key.as_str() {
                "cpu" => {
                    cpu = value.parse::<u64>().map_err(|e| {
                        crate::FlameError::InvalidConfig(format!(
                            "invalid cpu resource value '{value}': {e}"
                        ))
                    })?
                }
                "memory" | "mem" => memory = Self::parse_memory_result(value)?,
                "gpu" => {
                    gpu = value.parse::<i32>().map_err(|e| {
                        crate::FlameError::InvalidConfig(format!(
                            "invalid gpu resource value '{value}': {e}"
                        ))
                    })?
                }
                _ => {
                    return Err(crate::FlameError::InvalidConfig(format!(
                        "unknown resource requirement key '{key}'"
                    )))
                }
            }
        }

        Ok(Self { cpu, memory, gpu })
    }

    /// Returns a new `ResourceRequirement` with each field scaled by `n`.
    ///
    /// `cpu` and `memory` are widened multiplications in `u64` (cast `n` to `u64`).
    /// `gpu` uses `i32 * i32` (cast `n` to `i32`). Useful for deriving a session's
    /// total resource demand from a per-task unit and a task count, e.g.
    /// `unit.mul(slots)` for a session's per-executor allocation.
    pub fn mul(&self, n: u32) -> Self {
        Self {
            cpu: self.cpu * (n as u64),
            memory: self.memory * (n as u64),
            gpu: self.gpu * (n as i32),
        }
    }

    /// Returns per-field minimum of `self` and `other`.
    ///
    /// Each field is reduced independently — the result is not necessarily equal
    /// to either operand. Useful for clamping a demand to remaining cluster
    /// capacity per resource dimension.
    ///
    /// Note: receiver is `self` (by value), not `&self`, to avoid Rust method
    /// resolution preferring `Ord::min` (which `ResourceRequirement` derives via
    /// `#[derive(Ord)]`). Trait methods that match the receiver type without
    /// auto-ref win over inherent methods that need auto-ref, so `&self` here
    /// would shadow this method behind `Ord::min`. The struct is small and
    /// `Clone`, so consuming the receiver costs little.
    pub fn min(self, other: &Self) -> Self {
        Self {
            cpu: self.cpu.min(other.cpu),
            memory: self.memory.min(other.memory),
            gpu: self.gpu.min(other.gpu),
        }
    }

    /// Returns per-field maximum of `self` and `other`.
    ///
    /// Each field is increased independently — the result is not necessarily
    /// equal to either operand. Useful for enforcing a per-field floor (e.g.
    /// `min_instances * unit`) on a demand value.
    ///
    /// Note: receiver is `self` (by value), not `&self`, for the same reason as
    /// `min` — to avoid being shadowed by `Ord::max`.
    pub fn max(self, other: &Self) -> Self {
        Self {
            cpu: self.cpu.max(other.cpu),
            memory: self.memory.max(other.memory),
            gpu: self.gpu.max(other.gpu),
        }
    }

    fn parse_memory_result(s: &str) -> Result<u64, crate::FlameError> {
        crate::ctx::parse_memory_size(s)
    }

    /// Returns true iff cpu, memory, and gpu are all equal between self and other.
    pub fn equal(&self, other: &Self) -> bool {
        self.cpu == other.cpu && self.memory == other.memory && self.gpu == other.gpu
    }

    /// Returns true iff every field of self is `<=` the corresponding field of other.
    /// Per-field semantics: the predicate must hold for cpu, memory, AND gpu simultaneously.
    pub fn less_equal(&self, other: &Self) -> bool {
        self.cpu <= other.cpu && self.memory <= other.memory && self.gpu <= other.gpu
    }

    /// Returns true iff every field of self is strictly `<` the corresponding field of other.
    /// Per-field semantics: the predicate must hold for cpu, memory, AND gpu simultaneously.
    pub fn less(&self, other: &Self) -> bool {
        self.cpu < other.cpu && self.memory < other.memory && self.gpu < other.gpu
    }

    /// Returns true iff every field of self is `>=` the corresponding field of other.
    /// Per-field semantics: the predicate must hold for cpu, memory, AND gpu simultaneously.
    pub fn great_equal(&self, other: &Self) -> bool {
        self.cpu >= other.cpu && self.memory >= other.memory && self.gpu >= other.gpu
    }

    /// Returns true iff every field of self is strictly `>` the corresponding field of other.
    /// Per-field semantics: the predicate must hold for cpu, memory, AND gpu simultaneously.
    pub fn great(&self, other: &Self) -> bool {
        self.cpu > other.cpu && self.memory > other.memory && self.gpu > other.gpu
    }

    /// In-place addition of all three fields. Returns `&mut self` to allow chaining.
    pub fn add(&mut self, other: &Self) -> &mut Self {
        self.cpu += other.cpu;
        self.memory += other.memory;
        self.gpu += other.gpu;
        self
    }

    /// In-place subtraction of all three fields. Returns `&mut self` to allow chaining.
    ///
    /// Returns `Err(FlameError::InvalidConfig)` when any field of self is `<` the
    /// corresponding field of other (i.e. when `!self.great_equal(other)`), to avoid
    /// underflow on `u64`/`i32` and to surface the misuse to callers. The error
    /// message includes both operands' values to aid debugging.
    pub fn sub(&mut self, other: &Self) -> Result<&mut Self, crate::FlameError> {
        if !self.great_equal(other) {
            return Err(crate::FlameError::InvalidConfig(format!(
                "ResourceRequirement::sub underflow: self=(cpu={}, memory={}, gpu={}) < other=(cpu={}, memory={}, gpu={})",
                self.cpu, self.memory, self.gpu, other.cpu, other.memory, other.gpu
            )));
        }
        self.cpu -= other.cpu;
        self.memory -= other.memory;
        self.gpu -= other.gpu;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rr(cpu: u64, memory: u64, gpu: i32) -> ResourceRequirement {
        ResourceRequirement { cpu, memory, gpu }
    }

    #[test]
    fn equal_true_when_all_fields_match() {
        assert!(rr(1, 2, 3).equal(&rr(1, 2, 3)));
    }

    #[test]
    fn equal_false_when_cpu_differs() {
        assert!(!rr(2, 2, 3).equal(&rr(1, 2, 3)));
    }

    #[test]
    fn equal_false_when_memory_differs() {
        assert!(!rr(1, 4, 3).equal(&rr(1, 2, 3)));
    }

    #[test]
    fn equal_false_when_gpu_differs() {
        assert!(!rr(1, 2, 5).equal(&rr(1, 2, 3)));
    }

    #[test]
    fn less_equal_true_when_strictly_less() {
        assert!(rr(1, 2, 3).less_equal(&rr(2, 4, 5)));
    }

    #[test]
    fn less_equal_true_when_equal() {
        assert!(rr(1, 2, 3).less_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn less_equal_false_when_one_field_greater() {
        assert!(!rr(1, 5, 3).less_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn less_equal_false_when_mixed_one_less_one_greater() {
        // cpu less, gpu greater → must be false (per-field, all must be <=)
        assert!(!rr(0, 2, 9).less_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn less_true_when_strictly_less_in_all_fields() {
        assert!(rr(1, 2, 3).less(&rr(2, 4, 5)));
    }

    #[test]
    fn less_false_when_equal() {
        assert!(!rr(1, 2, 3).less(&rr(1, 2, 3)));
    }

    #[test]
    fn less_false_when_one_field_equal() {
        // cpu equal, others strictly less → must be false (strict on all)
        assert!(!rr(1, 1, 1).less(&rr(1, 2, 3)));
    }

    #[test]
    fn great_equal_true_when_strictly_greater() {
        assert!(rr(2, 4, 5).great_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn great_equal_true_when_equal() {
        assert!(rr(1, 2, 3).great_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn great_equal_false_when_one_field_less() {
        assert!(!rr(2, 1, 5).great_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn great_equal_false_when_mixed_one_greater_one_less() {
        assert!(!rr(2, 2, 1).great_equal(&rr(1, 2, 3)));
    }

    #[test]
    fn great_true_when_strictly_greater_in_all_fields() {
        assert!(rr(2, 4, 5).great(&rr(1, 2, 3)));
    }

    #[test]
    fn great_false_when_equal() {
        assert!(!rr(1, 2, 3).great(&rr(1, 2, 3)));
    }

    #[test]
    fn great_false_when_one_field_equal() {
        // memory equal, others strictly greater → must be false (strict on all)
        assert!(!rr(2, 2, 5).great(&rr(1, 2, 3)));
    }

    #[test]
    fn flame_result_converts_to_and_from_rpc_result() {
        let result = FlameResult {
            return_code: 12,
            message: Some("bind failed".to_string()),
        };

        let rpc_result: rpc::flame::v1::Result = result.clone().into();
        assert_eq!(rpc_result.return_code, 12);
        assert_eq!(rpc_result.message.as_deref(), Some("bind failed"));

        let parsed = FlameResult::from(rpc_result);
        assert_eq!(parsed, result);
    }

    #[test]
    fn add_sums_all_three_fields() {
        let mut a = rr(1, 2, 3);
        let b = rr(4, 5, 6);
        a.add(&b);
        assert_eq!(a, rr(5, 7, 9));
    }

    #[test]
    fn add_returns_mut_self_for_chaining() {
        let mut a = rr(1, 1, 1);
        let b = rr(2, 3, 4);
        // Chain two adds; the second consumes the &mut self returned by the first.
        a.add(&b).add(&b);
        assert_eq!(a, rr(5, 7, 9));
    }

    #[test]
    fn sub_success_when_all_fields_subtractable() {
        let mut a = rr(10, 20, 30);
        let b = rr(1, 2, 3);
        let res = a.sub(&b);
        assert!(res.is_ok());
        assert_eq!(a, rr(9, 18, 27));
    }

    #[test]
    fn sub_boundary_equal_operands_yields_zeros() {
        let mut a = rr(5, 6, 7);
        let b = rr(5, 6, 7);
        let res = a.sub(&b);
        assert!(res.is_ok());
        assert_eq!(a, rr(0, 0, 0));
    }

    #[test]
    fn sub_fails_when_cpu_would_underflow() {
        let mut a = rr(0, 10, 10);
        let b = rr(1, 1, 1);
        let res = a.sub(&b);
        assert!(matches!(res, Err(crate::FlameError::InvalidConfig(_))));
        // self must remain unchanged on error.
        assert_eq!(a, rr(0, 10, 10));
    }

    #[test]
    fn sub_fails_when_memory_would_underflow() {
        let mut a = rr(10, 0, 10);
        let b = rr(1, 1, 1);
        let res = a.sub(&b);
        assert!(matches!(res, Err(crate::FlameError::InvalidConfig(_))));
        assert_eq!(a, rr(10, 0, 10));
    }

    #[test]
    fn sub_fails_when_gpu_would_underflow() {
        let mut a = rr(10, 10, 0);
        let b = rr(1, 1, 1);
        let res = a.sub(&b);
        assert!(matches!(res, Err(crate::FlameError::InvalidConfig(_))));
        assert_eq!(a, rr(10, 10, 0));
    }

    #[test]
    fn sub_fails_when_mixed_one_greater_one_less() {
        // cpu greater, memory less → must fail (any field < other field is an error)
        let mut a = rr(10, 0, 5);
        let b = rr(1, 1, 1);
        let res = a.sub(&b);
        assert!(matches!(res, Err(crate::FlameError::InvalidConfig(_))));
        assert_eq!(a, rr(10, 0, 5));
    }

    // ── mul ──────────────────────────────────────────────────────────────────

    #[test]
    fn mul_typical_scales_each_field() {
        // (cpu=2, mem=1Gi, gpu=1) × 4 = (cpu=8, mem=4Gi, gpu=4)
        let r = rr(2, 1024 * 1024 * 1024, 1);
        assert_eq!(r.mul(4), rr(8, 4u64 * 1024 * 1024 * 1024, 4));
    }

    #[test]
    fn mul_by_zero_yields_zeros() {
        let r = rr(2, 1024 * 1024 * 1024, 1);
        assert_eq!(r.mul(0), rr(0, 0, 0));
    }

    #[test]
    fn mul_by_one_is_unchanged() {
        let r = rr(2, 1024 * 1024 * 1024, 1);
        assert_eq!(r.mul(1), r);
    }

    // ── min ──────────────────────────────────────────────────────────────────

    #[test]
    fn min_per_field_mixed_picks_smaller_per_dimension() {
        // (cpu:5, mem:10, gpu:1).min(cpu:8, mem:4, gpu:0) = (cpu:5, mem:4, gpu:0)
        let a = rr(5, 10, 1);
        let b = rr(8, 4, 0);
        assert_eq!(a.min(&b), rr(5, 4, 0));
    }

    #[test]
    fn min_with_self_is_self() {
        let r = rr(3, 7, 2);
        assert_eq!(r.clone().min(&r), r);
    }

    #[test]
    fn min_is_symmetric() {
        let a = rr(5, 10, 1);
        let b = rr(8, 4, 0);
        assert_eq!(a.clone().min(&b), b.clone().min(&a));
    }

    // ── max ──────────────────────────────────────────────────────────────────

    #[test]
    fn max_per_field_mixed_picks_larger_per_dimension() {
        // Mirror of min: (cpu:5, mem:10, gpu:1).max(cpu:8, mem:4, gpu:0) = (cpu:8, mem:10, gpu:1)
        let a = rr(5, 10, 1);
        let b = rr(8, 4, 0);
        assert_eq!(a.max(&b), rr(8, 10, 1));
    }

    #[test]
    fn max_with_self_is_self() {
        let r = rr(3, 7, 2);
        assert_eq!(r.clone().max(&r), r);
    }

    #[test]
    fn max_is_symmetric() {
        let a = rr(5, 10, 1);
        let b = rr(8, 4, 0);
        assert_eq!(a.clone().max(&b), b.clone().max(&a));
    }

    #[test]
    fn sub_error_message_includes_both_operands() {
        let mut a = rr(0, 10, 10);
        let b = rr(1, 2, 3);
        let err = a.sub(&b).unwrap_err();
        let msg = match err {
            crate::FlameError::InvalidConfig(m) => m,
            other => panic!("expected InvalidConfig, got {:?}", other),
        };
        // self values
        assert!(msg.contains("cpu=0"));
        assert!(msg.contains("memory=10"));
        // other values
        assert!(msg.contains("cpu=1"));
        assert!(msg.contains("memory=2"));
        assert!(msg.contains("gpu=3"));
    }
}

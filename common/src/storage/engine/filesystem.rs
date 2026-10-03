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

//! Filesystem-based storage engine implementation.
//!
//! This module implements a high-performance storage engine that uses the filesystem
//! directly instead of a database. It uses a single-file architecture per session
//! for tasks to minimize filesystem metadata operations.
//!
//! # Architecture
//!
//! ```text
//! <work_dir>/data/workspaces/<workspace>/
//! ├── workspace.json
//! ├── sessions/<session>/
//! │   ├── metadata          # Session metadata (JSON)
//! │   ├── tasks.bin         # TaskMetadata records (fixed-size, indexed by task number)
//! │   ├── inputs.bin        # Concatenated input data (append-only)
//! │   └── outputs.bin       # Concatenated output data (append-only)
//! ├── applications/<application>/metadata
//! └── executors/<executor>/metadata
//! ```
//!
//! # Design Decisions
//!
//! - **Fixed-size task metadata**: Enables O(1) random access by session-local task number
//! - **Append-only data files**: Maximizes write throughput for inputs/outputs
//! - **No file locks**: Relies on in-memory locks in the Session Manager
//! - **CRC32 checksums**: Detects corruption on read

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use bincode::{Decode, Encode};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::apis::{
    new_metadata_id, Application, ApplicationAttributes, ApplicationSchema, ApplicationState,
    ExecutorGID, ExecutorState, Node, NodeInfo, NodeState, ResourceRequirement, Session,
    SessionAttributes, SessionGID, SessionState, SessionStatus, Shim, Task, TaskInput, TaskName,
    TaskOptions, TaskResult, TaskState, Workspace, DEFAULT_WORKSPACE,
};
use crate::{FlameError, FLAME_HOME};

use crate::apis::{ApplicationFilter, Executor, SessionFilter, TaskFilter};
use crate::storage::engine::{Engine, EnginePtr};

/// Task metadata stored in tasks.bin with fixed-size records.
///
/// Uses `bincode` with `fixint` encoding to ensure constant serialized size.
/// The checksum is calculated using `crc32fast` for data integrity.
#[derive(Encode, Decode, Debug, Clone, Default)]
struct TaskMetadata {
    /// Numeric file slot and internal task name; RPC uses its decimal string.
    pub name: TaskName,
    /// Persisted UUID used only as debug metadata.
    pub id: [u8; 16],
    /// Optimistic locking version
    pub version: u32,
    /// CRC32 checksum of the record (excluding this field)
    pub checksum: u32,
    /// Task state (TaskState enum as u8)
    pub state: u8,
    /// Unix timestamp of creation
    pub creation_time: i64,
    /// Unix timestamp of completion (0 if not completed)
    pub completion_time: i64,
    /// Offset in inputs.bin where input data starts
    pub input_offset: u64,
    /// Length of input data in bytes
    pub input_len: u64,
    /// Offset in outputs.bin where output data starts
    pub output_offset: u64,
    /// Length of output data in bytes
    pub output_len: u64,
    /// Offset in affinity.bin where the encoded affinity vector starts
    pub affinity_offset: u64,
    /// Length of the encoded affinity vector in bytes
    pub affinity_len: u64,
}

/// Session metadata stored as JSON.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct SessionMetadata {
    pub id: String,
    pub name: String,
    pub workspace: String,
    pub application: String,
    pub version: u32,
    pub state: i32,
    pub creation_time: i64,
    pub completion_time: Option<i64>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    #[serde(default)]
    pub priority: u32,
    pub common_data_len: u64,
    #[serde(default)]
    pub tokens: HashMap<String, String>,
    #[serde(default)]
    pub resreq_cpu: Option<u64>,
    #[serde(default)]
    pub resreq_memory: Option<u64>,
    #[serde(default)]
    pub resreq_gpu: Option<i32>,
}

fn default_batch_size() -> u32 {
    1
}

/// Application metadata stored as JSON.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ApplicationMetadata {
    pub id: String,
    pub name: String,
    pub workspace: String,
    pub version: u32,
    pub state: i32,
    pub creation_time: i64,
    #[serde(default)]
    pub shim: i32, // 0 = Host (default), 1 = Wasm, 3 = Cri
    pub image: Option<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub environments: std::collections::HashMap<String, String>,
    pub working_directory: Option<String>,
    pub max_instances: u32,
    pub delay_release_seconds: i64,
    pub schema: Option<ApplicationSchemaMetadata>,
    pub url: Option<String>,
    #[serde(default)]
    pub installer: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ApplicationSchemaMetadata {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct NodeMetadata {
    pub id: String,
    pub name: String,
    pub state: i32,
    pub capacity_cpu: u64,
    pub capacity_memory: u64,
    #[serde(default)]
    pub capacity_gpu: i32,
    pub allocatable_cpu: u64,
    pub allocatable_memory: u64,
    #[serde(default)]
    pub allocatable_gpu: i32,
    pub info_arch: String,
    pub info_os: String,
    pub creation_time: i64,
    pub last_heartbeat: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ExecutorMetadata {
    pub id: String,
    pub name: String,
    pub node: String,
    pub application: String,
    pub workspace: String,
    pub resreq_cpu: u64,
    pub resreq_memory: u64,
    #[serde(default)]
    pub resreq_gpu: i32,
    pub shim: i32,
    pub task: Option<String>,
    pub session: Option<String>,
    pub creation_time: i64,
    pub state: i32,
}

/// Bincode configuration for fixed-size encoding.
fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_fixed_int_encoding()
        .with_little_endian()
}

/// Calculate the fixed record size for TaskMetadata.
fn task_record_size() -> usize {
    // Calculate the serialized size of a default TaskMetadata
    let meta = TaskMetadata::default();
    bincode::encode_to_vec(&meta, bincode_config())
        .expect("Failed to calculate record size")
        .len()
}

/// Calculate CRC32 checksum for task metadata (excluding the checksum field itself).
fn calculate_checksum(meta: &TaskMetadata) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&meta.name.to_le_bytes());
    hasher.update(&meta.id);
    hasher.update(&meta.version.to_le_bytes());
    hasher.update(&[meta.state]);
    hasher.update(&meta.creation_time.to_le_bytes());
    hasher.update(&meta.completion_time.to_le_bytes());
    hasher.update(&meta.input_offset.to_le_bytes());
    hasher.update(&meta.input_len.to_le_bytes());
    hasher.update(&meta.output_offset.to_le_bytes());
    hasher.update(&meta.output_len.to_le_bytes());
    hasher.finalize()
}

const WORKSPACES: &str = "workspaces";
const SESSIONS: &str = "sessions";
const APPLICATIONS: &str = "applications";
const EXECUTORS: &str = "executors";

type SessionLocks = RwLock<HashMap<SessionGID, Arc<Mutex<()>>>>;
type ExecutorLocks = RwLock<HashMap<ExecutorGID, Arc<Mutex<()>>>>;

pub struct FilesystemEngine {
    base_path: PathBuf,
    record_size: usize,
    ssn_locks: SessionLocks,
    node_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    executor_locks: ExecutorLocks,
}

macro_rules! lock_ssn {
    ($self:expr, $gid:expr) => {
        let __fs_local_ssn_lock = {
            let mut locks = $self
                .ssn_locks
                .write()
                .map_err(|e| FlameError::Storage(format!("Session lock poisoned: {}", e)))?;
            locks
                .entry((*$gid).clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let __fs_local_ssn_guard = __fs_local_ssn_lock
            .lock()
            .map_err(|e| FlameError::Storage(format!("Session lock poisoned: {}", e)))?;
    };
}

macro_rules! lock_app {
    ($self:expr) => {
        $self
            .ssn_locks
            .write()
            .map_err(|e| FlameError::Storage(format!("App lock poisoned: {}", e)))
    };
}

macro_rules! lock_node {
    ($self:expr, $node_name:expr) => {
        let __fs_local_node_lock = {
            let mut locks = $self
                .node_locks
                .write()
                .map_err(|e| FlameError::Storage(format!("Node lock poisoned: {}", e)))?;
            locks
                .entry($node_name.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let __fs_local_node_guard = __fs_local_node_lock
            .lock()
            .map_err(|e| FlameError::Storage(format!("Node lock poisoned: {}", e)))?;
    };
}

macro_rules! lock_executor {
    ($self:expr, $gid:expr) => {
        let __fs_local_exec_lock = {
            let mut locks = $self
                .executor_locks
                .write()
                .map_err(|e| FlameError::Storage(format!("Executor lock poisoned: {}", e)))?;
            locks
                .entry((*$gid).clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let __fs_local_exec_guard = __fs_local_exec_lock
            .lock()
            .map_err(|e| FlameError::Storage(format!("Executor lock poisoned: {}", e)))?;
    };
}

impl FilesystemEngine {
    fn require_workspace(&self, name: &str) -> Result<(), FlameError> {
        let path = self
            .base_path
            .join(WORKSPACES)
            .join(name)
            .join("workspace.json");
        if path.is_file() {
            Ok(())
        } else {
            Err(FlameError::NotFound(format!("workspace {name}")))
        }
    }

    fn validate_executor_references(&self, executor: &Executor) -> Result<(), FlameError> {
        self.require_workspace(&executor.workspace)?;
        self.read_node_metadata(&executor.node)?;
        self.read_application_metadata(&executor.workspace, &executor.application)?;
        if let Some(gid) = executor.session() {
            let parent = self.read_session_metadata(&gid)?;
            if parent.application != executor.application {
                return Err(FlameError::InvalidConfig(
                    "executor session belongs to another application".to_string(),
                ));
            }
        }
        if let Some(task) = executor.task.as_deref() {
            let gid = executor.session().ok_or_else(|| {
                FlameError::InvalidConfig("executor task requires session".to_string())
            })?;
            self.read_task_metadata(&gid, Self::parse_task_name(task)?)?;
        }
        Ok(())
    }

    /// Create a new filesystem engine from a URL.
    ///
    /// URL format: `filesystem://<path>` or `file://<path>`
    pub async fn new_ptr(url: &str) -> Result<EnginePtr, FlameError> {
        let path = Self::parse_url(url)?;

        fs::create_dir_all(path.join(WORKSPACES))?;
        fs::create_dir_all(path.join("nodes"))
            .map_err(|e| FlameError::Storage(format!("Failed to create nodes directory: {e}")))?;

        let record_size = task_record_size();
        tracing::info!(
            "Filesystem storage engine initialized at {:?} with record size {}",
            path,
            record_size
        );

        let engine = Arc::new(FilesystemEngine {
            base_path: path,
            record_size,
            ssn_locks: RwLock::new(HashMap::new()),
            node_locks: RwLock::new(HashMap::new()),
            executor_locks: RwLock::new(HashMap::new()),
        });
        if engine
            .base_path
            .join(WORKSPACES)
            .join(DEFAULT_WORKSPACE)
            .exists()
        {
            engine.read_workspace(DEFAULT_WORKSPACE)?;
        } else {
            engine
                .create_workspace(DEFAULT_WORKSPACE.to_string())
                .await?;
        }
        Ok(engine)
    }

    fn read_workspace(&self, name: &str) -> Result<Workspace, FlameError> {
        let path = self
            .base_path
            .join(WORKSPACES)
            .join(name)
            .join("workspace.json");
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
            .map_err(|e| FlameError::Storage(e.to_string()))?;
        let name = value["name"]
            .as_str()
            .ok_or_else(|| FlameError::Storage("missing workspace name".into()))?;
        let millis = value["create_at"]
            .as_i64()
            .ok_or_else(|| FlameError::Storage("missing workspace timestamp".into()))?;
        Ok(Workspace {
            name: name.to_string(),
            create_at: DateTime::<Utc>::from_timestamp_millis(millis)
                .ok_or_else(|| FlameError::Storage("invalid workspace timestamp".into()))?,
        })
    }

    /// Parse the storage URL to extract the base path.
    fn parse_url(url: &str) -> Result<PathBuf, FlameError> {
        let path = if let Some(p) = url.strip_prefix("filesystem://") {
            p
        } else if let Some(p) = url.strip_prefix("file://") {
            p
        } else if let Some(p) = url.strip_prefix("fs://") {
            p
        } else {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid filesystem URL: {url}. Expected filesystem://, file://, or fs:// prefix"
            )));
        };

        if path.starts_with('/') {
            Ok(PathBuf::from(path))
        } else {
            let flame_home =
                std::env::var(FLAME_HOME).unwrap_or_else(|_| "/usr/local/flame".to_string());
            Ok(PathBuf::from(flame_home).join(path))
        }
    }

    fn session_path(&self, gid: &SessionGID) -> PathBuf {
        self.base_path
            .join(WORKSPACES)
            .join(&gid.workspace)
            .join(SESSIONS)
            .join(&gid.session)
    }

    fn application_path(&self, workspace: &str, application: &str) -> PathBuf {
        self.base_path
            .join(WORKSPACES)
            .join(workspace)
            .join(APPLICATIONS)
            .join(application)
    }

    fn node_path(&self, node_name: &str) -> PathBuf {
        self.base_path.join("nodes").join(node_name)
    }

    fn executor_path(&self, gid: &ExecutorGID) -> PathBuf {
        self.base_path
            .join(WORKSPACES)
            .join(&gid.workspace)
            .join(EXECUTORS)
            .join(&gid.executor)
    }

    /// Read session metadata from disk.
    fn read_session_metadata(&self, gid: &SessionGID) -> Result<SessionMetadata, FlameError> {
        let path = self.session_path(gid).join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!("Session {gid} not found: {e}"))
            } else {
                FlameError::Storage(format!("Failed to read session metadata for {gid}: {e}"))
            }
        })?;
        let meta: SessionMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("Failed to parse session metadata: {e}")))?;
        if meta.workspace != gid.workspace || meta.name != gid.session {
            return Err(FlameError::Storage(
                "session metadata path mismatch".to_string(),
            ));
        }
        Ok(meta)
    }

    fn _list_sessions_metadata(
        &self,
        filter: Option<&SessionFilter>,
    ) -> Result<Vec<(String, SessionMetadata)>, FlameError> {
        if filter.and_then(|filter| filter.limit) == Some(0) {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        let workspaces = self.base_path.join(WORKSPACES);
        for workspace_entry in fs::read_dir(workspaces)? {
            let workspace_entry = workspace_entry?;
            if !workspace_entry.path().is_dir() {
                continue;
            }
            let workspace = workspace_entry.file_name().to_string_lossy().to_string();
            if filter.is_some_and(|filter| filter.workspace != workspace) {
                continue;
            }
            let sessions_dir = workspace_entry.path().join(SESSIONS);
            let entries = match fs::read_dir(sessions_dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(FlameError::Storage(format!(
                        "Failed to list sessions: {error}"
                    )))
                }
            };
            for entry in entries {
                let entry = entry.map_err(|error| {
                    FlameError::Storage(format!(
                        "Failed to read a session directory entry: {error}"
                    ))
                })?;
                let session_name = entry.file_name().to_string_lossy().to_string();
                let metadata =
                    self.read_session_metadata(&SessionGID::new(&workspace, &session_name))?;
                let matches = filter.is_none_or(|filter| {
                    filter
                        .application
                        .as_ref()
                        .is_none_or(|application| metadata.application == *application)
                        && filter
                            .state
                            .is_none_or(|state| metadata.state == state as i32)
                        && filter
                            .names
                            .as_ref()
                            .is_none_or(|names| names.contains(&session_name))
                });
                if matches {
                    sessions.push((session_name, metadata));
                    if filter
                        .and_then(|filter| filter.limit)
                        .is_some_and(|limit| sessions.len() >= limit)
                    {
                        break;
                    }
                }
            }
        }
        Ok(sessions)
    }

    /// Write session metadata to disk atomically.
    fn write_session_metadata(
        &self,
        gid: &SessionGID,
        meta: &SessionMetadata,
    ) -> Result<(), FlameError> {
        let session_dir = self.session_path(gid);
        let path = session_dir.join("metadata");
        let tmp_path = session_dir.join("metadata.tmp");

        let content = serde_json::to_string_pretty(meta).map_err(|e| {
            FlameError::Storage(format!("Failed to serialize session metadata: {e}"))
        })?;

        // Write to temp file first
        fs::write(&tmp_path, &content)
            .map_err(|e| FlameError::Storage(format!("Failed to write session metadata: {e}")))?;

        // Atomic rename
        fs::rename(&tmp_path, &path)
            .map_err(|e| FlameError::Storage(format!("Failed to rename session metadata: {e}")))?;

        Ok(())
    }

    /// Read application metadata from disk.
    fn read_application_metadata(
        &self,
        workspace: &str,
        application: &str,
    ) -> Result<ApplicationMetadata, FlameError> {
        let path = self
            .application_path(workspace, application)
            .join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!(
                    "Application {workspace}/{application} not found: {e}"
                ))
            } else {
                FlameError::Storage(format!(
                    "Failed to read application metadata for {workspace}/{application}: {e}"
                ))
            }
        })?;
        let meta: ApplicationMetadata = serde_json::from_str(&content).map_err(|e| {
            FlameError::Storage(format!("Failed to parse application metadata: {e}"))
        })?;
        if meta.workspace != workspace || meta.name != application {
            return Err(FlameError::Storage(
                "application metadata path mismatch".to_string(),
            ));
        }
        Ok(meta)
    }

    /// Write application metadata to disk atomically.
    fn write_application_metadata(
        &self,
        workspace: &str,
        application: &str,
        meta: &ApplicationMetadata,
    ) -> Result<(), FlameError> {
        let app_dir = self.application_path(workspace, application);
        fs::create_dir_all(&app_dir).map_err(|e| {
            FlameError::Storage(format!("Failed to create application directory: {e}"))
        })?;

        let path = app_dir.join("metadata");
        let tmp_path = app_dir.join("metadata.tmp");

        let content = serde_json::to_string_pretty(meta).map_err(|e| {
            FlameError::Storage(format!("Failed to serialize application metadata: {e}"))
        })?;

        // Write to temp file first
        fs::write(&tmp_path, &content).map_err(|e| {
            FlameError::Storage(format!("Failed to write application metadata: {e}"))
        })?;

        // Atomic rename
        fs::rename(&tmp_path, &path).map_err(|e| {
            FlameError::Storage(format!("Failed to rename application metadata: {e}"))
        })?;

        Ok(())
    }

    fn read_node_metadata(&self, node_name: &str) -> Result<NodeMetadata, FlameError> {
        let path = self.node_path(node_name).join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!("Node {node_name} not found"))
            } else {
                FlameError::Storage(format!("Failed to read node metadata for {node_name}: {e}"))
            }
        })?;
        serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("Failed to parse node metadata: {e}")))
    }

    fn write_node_metadata(&self, node_name: &str, meta: &NodeMetadata) -> Result<(), FlameError> {
        let node_dir = self.node_path(node_name);
        fs::create_dir_all(&node_dir)
            .map_err(|e| FlameError::Storage(format!("Failed to create node directory: {e}")))?;

        let path = node_dir.join("metadata");
        let tmp_path = node_dir.join("metadata.tmp");

        let content = serde_json::to_string_pretty(meta)
            .map_err(|e| FlameError::Storage(format!("Failed to serialize node metadata: {e}")))?;

        fs::write(&tmp_path, &content)
            .map_err(|e| FlameError::Storage(format!("Failed to write node metadata: {e}")))?;

        fs::rename(&tmp_path, &path)
            .map_err(|e| FlameError::Storage(format!("Failed to rename node metadata: {e}")))?;

        Ok(())
    }

    fn read_executor_metadata(&self, gid: &ExecutorGID) -> Result<ExecutorMetadata, FlameError> {
        let path = self.executor_path(gid).join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!(
                    "Executor {}/{} not found",
                    gid.workspace, gid.executor
                ))
            } else {
                FlameError::Storage(format!(
                    "Failed to read executor metadata for {}/{}: {e}",
                    gid.workspace, gid.executor
                ))
            }
        })?;
        let meta: ExecutorMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("Failed to parse executor metadata: {e}")))?;
        if meta.workspace != gid.workspace || meta.name != gid.executor {
            return Err(FlameError::Storage(
                "executor metadata path mismatch".to_string(),
            ));
        }
        Ok(meta)
    }

    fn write_executor_metadata(
        &self,
        gid: &ExecutorGID,
        meta: &ExecutorMetadata,
    ) -> Result<(), FlameError> {
        let exec_dir = self.executor_path(gid);
        fs::create_dir_all(&exec_dir).map_err(|e| {
            FlameError::Storage(format!("Failed to create executor directory: {e}"))
        })?;

        let path = exec_dir.join("metadata");
        let tmp_path = exec_dir.join("metadata.tmp");

        let content = serde_json::to_string_pretty(meta).map_err(|e| {
            FlameError::Storage(format!("Failed to serialize executor metadata: {e}"))
        })?;

        fs::write(&tmp_path, &content)
            .map_err(|e| FlameError::Storage(format!("Failed to write executor metadata: {e}")))?;

        fs::rename(&tmp_path, &path)
            .map_err(|e| FlameError::Storage(format!("Failed to rename executor metadata: {e}")))?;

        Ok(())
    }

    /// Read task metadata from tasks.bin.
    fn read_task_metadata(
        &self,
        gid: &SessionGID,
        task_number: TaskName,
    ) -> Result<TaskMetadata, FlameError> {
        if task_number == 0 {
            return Err(FlameError::NotFound(format!(
                "Invalid task name: {task_number} (must be >= 1)"
            )));
        }

        let path = self.session_path(gid).join("tasks.bin");

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|e| FlameError::NotFound(format!("Tasks file not found: {e}")))?;

        let offset = (task_number - 1)
            .checked_mul(self.record_size as u64)
            .ok_or_else(|| {
                FlameError::InvalidConfig(format!("invalid task name: {task_number}"))
            })?;
        file.seek(SeekFrom::Start(offset)).map_err(|e| {
            FlameError::Storage(format!("Failed to seek to task {task_number}: {e}"))
        })?;

        let mut buffer = vec![0u8; self.record_size];
        file.read_exact(&mut buffer)
            .map_err(|e| FlameError::NotFound(format!("Task {task_number} not found: {e}")))?;

        let (meta, _): (TaskMetadata, _) = bincode::decode_from_slice(&buffer, bincode_config())
            .map_err(|e| {
                FlameError::Storage(format!("Failed to deserialize task metadata: {e}"))
            })?;

        // Verify checksum
        let expected_checksum = calculate_checksum(&meta);
        if meta.checksum != expected_checksum {
            return Err(FlameError::Storage(format!(
                "Task {task_number} checksum mismatch: expected {expected_checksum}, got {}",
                meta.checksum
            )));
        }

        Ok(meta)
    }

    fn parse_task_name(task: &str) -> Result<TaskName, FlameError> {
        let number = task
            .parse::<TaskName>()
            .map_err(|_| FlameError::InvalidConfig(format!("invalid task name: {task}")))?;
        if number.to_string() != task {
            return Err(FlameError::InvalidConfig(format!(
                "non-canonical task name: {task}"
            )));
        }
        Ok(number)
    }

    /// Write task metadata to tasks.bin at the specified offset.
    fn write_task_metadata(&self, gid: &SessionGID, meta: &TaskMetadata) -> Result<(), FlameError> {
        let path = self.session_path(gid).join("tasks.bin");

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| FlameError::Storage(format!("Failed to open tasks file: {e}")))?;

        let offset = meta
            .name
            .checked_sub(1)
            .and_then(|number| number.checked_mul(self.record_size as u64))
            .ok_or_else(|| {
                FlameError::InvalidConfig(format!("invalid task name: {}", meta.name))
            })?;
        file.seek(SeekFrom::Start(offset)).map_err(|e| {
            FlameError::Storage(format!("Failed to seek to task {}: {e}", meta.name))
        })?;

        let buffer = bincode::encode_to_vec(meta, bincode_config())
            .map_err(|e| FlameError::Storage(format!("Failed to serialize task metadata: {e}")))?;

        file.write_all(&buffer)
            .map_err(|e| FlameError::Storage(format!("Failed to write task metadata: {e}")))?;

        file.sync_data()
            .map_err(|e| FlameError::Storage(format!("Failed to sync task metadata: {e}")))?;

        Ok(())
    }

    /// Append data to a file and return the offset where it was written.
    fn append_data(
        &self,
        gid: &SessionGID,
        filename: &str,
        data: &[u8],
    ) -> Result<u64, FlameError> {
        let path = self.session_path(gid).join(filename);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| FlameError::Storage(format!("Failed to open {filename}: {e}")))?;

        // Get current file size (this is where we'll write)
        let offset = file.seek(SeekFrom::End(0)).map_err(|e| {
            FlameError::Storage(format!("Failed to seek to end of {filename}: {e}"))
        })?;

        file.write_all(data)
            .map_err(|e| FlameError::Storage(format!("Failed to write to {filename}: {e}")))?;

        file.sync_data()
            .map_err(|e| FlameError::Storage(format!("Failed to sync {filename}: {e}")))?;

        Ok(offset)
    }

    /// Read data from a file at the specified offset and length.
    fn read_data(
        &self,
        gid: &SessionGID,
        filename: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FlameError> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let path = self.session_path(gid).join(filename);

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|e| FlameError::Storage(format!("Failed to open {filename}: {e}")))?;

        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FlameError::Storage(format!("Failed to seek in {filename}: {e}")))?;

        let mut buffer = vec![0u8; len as usize];
        file.read_exact(&mut buffer)
            .map_err(|e| FlameError::Storage(format!("Failed to read from {filename}: {e}")))?;

        Ok(buffer)
    }

    /// Count tasks matching the filter.
    fn _count_task(&self, filter: &TaskFilter) -> Result<u64, FlameError> {
        let gid = filter.session();
        let path = self.session_path(gid).join("tasks.bin");

        let task_count = match fs::metadata(&path) {
            Ok(metadata) => metadata.len() / self.record_size as u64,
            Err(_) => 0,
        };
        let Some(states) = filter.states.as_ref() else {
            return Ok(task_count);
        };

        let mut count = 0;
        for task_id in 1..=task_count {
            let metadata = self.read_task_metadata(gid, task_id)?;
            let state = TaskState::try_from(metadata.state as i32)?;
            if states.contains(&state) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Read common data for a session.
    fn read_common_data(&self, gid: &SessionGID, len: u64) -> Result<Option<Bytes>, FlameError> {
        if len == 0 {
            return Ok(None);
        }

        let path = self.session_path(gid).join("common_data.bin");
        match fs::read(&path) {
            Ok(data) => Ok(Some(Bytes::from(data))),
            Err(_) => Ok(None),
        }
    }

    /// Write common data for a session.
    fn write_common_data(&self, gid: &SessionGID, data: &[u8]) -> Result<(), FlameError> {
        let path = self.session_path(gid).join("common_data.bin");
        fs::write(&path, data)
            .map_err(|e| FlameError::Storage(format!("Failed to write common data: {e}")))?;
        Ok(())
    }

    /// Convert TaskMetadata to Task.
    fn task_from_metadata(
        &self,
        gid: &SessionGID,
        meta: &TaskMetadata,
    ) -> Result<Task, FlameError> {
        let input = if meta.input_len > 0 {
            let data = self.read_data(gid, "inputs.bin", meta.input_offset, meta.input_len)?;
            Some(Bytes::from(data))
        } else {
            None
        };

        let output = if meta.output_len > 0 {
            let data = self.read_data(gid, "outputs.bin", meta.output_offset, meta.output_len)?;
            Some(Bytes::from(data))
        } else {
            None
        };

        let affinity: std::collections::HashSet<Bytes> = if meta.affinity_len > 0 {
            let data =
                self.read_data(gid, "affinity.bin", meta.affinity_offset, meta.affinity_len)?;
            let (keys, _): (Vec<Vec<u8>>, _) = bincode::decode_from_slice(&data, bincode_config())
                .map_err(|e| FlameError::Storage(e.to_string()))?;
            keys.into_iter().map(Bytes::from).collect()
        } else {
            Default::default()
        };

        let state = TaskState::try_from(meta.state as i32)?;
        let completion_time = if meta.completion_time > 0 {
            DateTime::from_timestamp(meta.completion_time, 0)
        } else {
            None
        };

        Ok(Task {
            id: uuid::Uuid::from_bytes(meta.id).to_string(),
            name: meta.name,
            session: gid.session.clone(),
            workspace: gid.workspace.clone(),
            version: meta.version,
            input,
            output,
            affinity,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                .ok_or_else(|| FlameError::Storage("Invalid creation time".to_string()))?,
            completion_time,
            events: Vec::new(), // Events are handled by EventManager
            state,
        })
    }

    /// Convert SessionMetadata to Session.
    fn session_from_metadata(&self, meta: &SessionMetadata) -> Result<Session, FlameError> {
        let state = SessionState::try_from(meta.state)?;
        let common_data = self.read_common_data(
            &SessionGID::new(&meta.workspace, &meta.name),
            meta.common_data_len,
        )?;
        let completion_time = meta
            .completion_time
            .and_then(|t| DateTime::from_timestamp(t, 0));

        let resreq = match (meta.resreq_cpu, meta.resreq_memory, meta.resreq_gpu) {
            (Some(cpu), Some(memory), Some(gpu)) => Some(ResourceRequirement { cpu, memory, gpu }),
            _ => None,
        };

        Ok(Session {
            id: meta.id.clone(),
            name: meta.name.clone(),
            workspace: meta.workspace.clone(),
            application: meta.application.clone(),
            version: meta.version,
            common_data,
            tokens: meta.tokens.clone(),
            tasks: std::collections::HashMap::new(),
            tasks_index: std::collections::HashMap::new(),
            creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                .ok_or_else(|| FlameError::Storage("Invalid creation time".to_string()))?,
            completion_time,
            events: Vec::new(),
            status: SessionStatus { state },
            min_instances: meta.min_instances,
            max_instances: meta.max_instances,
            batch_size: meta.batch_size,
            priority: meta.priority,
            resreq,
            retry_count: 0,
        })
    }

    /// Convert ApplicationMetadata to Application.
    fn application_from_metadata(meta: &ApplicationMetadata) -> Result<Application, FlameError> {
        let state = ApplicationState::try_from(meta.state)?;
        let schema = meta.schema.as_ref().map(|s| ApplicationSchema {
            input: s.input.clone(),
            output: s.output.clone(),
            common_data: s.common_data.clone(),
        });

        Ok(Application {
            id: meta.id.clone(),
            name: meta.name.clone(),
            workspace: meta.workspace.clone(),
            version: meta.version,
            state,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                .ok_or_else(|| FlameError::Storage("Invalid creation time".to_string()))?,
            shim: Shim::try_from(meta.shim).unwrap_or_default(),
            image: meta.image.clone(),
            description: meta.description.clone(),
            labels: meta.labels.clone(),
            command: meta.command.clone(),
            arguments: meta.arguments.clone(),
            environments: meta.environments.clone(),
            working_directory: meta.working_directory.clone(),
            max_instances: meta.max_instances,
            delay_release: Duration::seconds(meta.delay_release_seconds),
            schema,
            url: meta.url.clone(),
            installer: meta.installer.clone(),
        })
    }

    fn _update_task_state(
        &self,
        gid: &SessionGID,
        task: TaskName,
        task_state: TaskState,
    ) -> Result<Task, FlameError> {
        let mut meta = self.read_task_metadata(gid, task)?;

        meta.state = task_state as u8;
        meta.version += 1;

        if task_state.is_terminal() {
            meta.completion_time = Utc::now().timestamp();
        }

        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(gid, &meta)?;
        self.task_from_metadata(gid, &meta)
    }

    fn executor_from_metadata(meta: ExecutorMetadata) -> Executor {
        Executor {
            id: meta.id,
            name: meta.name,
            node: meta.node,
            resreq: ResourceRequirement {
                cpu: meta.resreq_cpu,
                memory: meta.resreq_memory,
                gpu: meta.resreq_gpu,
            },
            shim: Shim::try_from(meta.shim).unwrap_or_default(),
            application: meta.application,
            workspace: meta.workspace,
            task: meta.task,
            session: meta.session,
            attributes: Default::default(),
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::from(meta.state),
        }
    }

    fn executor_to_metadata(executor: &Executor) -> ExecutorMetadata {
        ExecutorMetadata {
            id: executor.id.clone(),
            name: executor.name.clone(),
            node: executor.node.clone(),
            application: executor.application.clone(),
            workspace: executor.workspace.clone(),
            resreq_cpu: executor.resreq.cpu,
            resreq_memory: executor.resreq.memory,
            resreq_gpu: executor.resreq.gpu,
            shim: i32::from(executor.shim),
            task: executor.task.clone(),
            session: executor.session.clone(),
            creation_time: executor.creation_time.timestamp(),
            state: i32::from(executor.state),
        }
    }
}

#[async_trait]
impl Engine for FilesystemEngine {
    async fn create_workspace(&self, name: String) -> Result<Workspace, FlameError> {
        let path = self.base_path.join(WORKSPACES).join(&name);
        fs::create_dir(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                FlameError::AlreadyExist(format!("workspace {name}"))
            } else {
                FlameError::Storage(e.to_string())
            }
        })?;
        let workspace = Workspace {
            name: name.clone(),
            create_at: Utc::now(),
        };
        for directory in [SESSIONS, APPLICATIONS, EXECUTORS] {
            fs::create_dir(path.join(directory))?;
        }
        fs::write(
            path.join("workspace.json"),
            serde_json::json!({
                "name": name, "create_at": workspace.create_at.timestamp_millis(),
            })
            .to_string(),
        )?;
        Ok(workspace)
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        let mut result = Vec::new();
        for entry in fs::read_dir(self.base_path.join(WORKSPACES))? {
            let name = entry?.file_name().to_string_lossy().to_string();
            result.push(self.read_workspace(&name)?);
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    async fn register_application(
        &self,
        workspace: String,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        crate::apis::validate_application_url(&workspace, attr.url.as_deref())?;
        self.require_workspace(&workspace)?;
        if self.read_application_metadata(&workspace, &name).is_ok() {
            return Err(FlameError::AlreadyExist(format!(
                "Application '{}' already exists",
                name
            )));
        }

        let schema = attr.schema.map(|s| ApplicationSchemaMetadata {
            input: s.input,
            output: s.output,
            common_data: s.common_data,
        });

        let meta = ApplicationMetadata {
            id: new_metadata_id(),
            name: name.clone(),
            workspace: workspace.clone(),
            version: 1,
            state: ApplicationState::Enabled as i32,
            creation_time: Utc::now().timestamp(),
            shim: attr.shim as i32,
            image: attr.image,
            description: attr.description,
            labels: attr.labels,
            command: attr.command,
            arguments: attr.arguments,
            environments: attr.environments,
            working_directory: attr.working_directory,
            max_instances: attr.max_instances,
            delay_release_seconds: attr.delay_release.num_seconds(),
            schema,
            url: attr.url,
            installer: attr.installer,
        };

        self.write_application_metadata(&workspace, &name, &meta)?;
        Self::application_from_metadata(&meta)
    }

    async fn update_application_state(
        &self,
        workspace: &str,
        name: &str,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let _guard = lock_app!(self)?;
        let mut meta = self.read_application_metadata(workspace, name)?;
        if meta.state == state as i32 {
            return Self::application_from_metadata(&meta);
        }

        meta.state = state as i32;
        meta.version += 1;
        self.write_application_metadata(workspace, name, &meta)?;
        Self::application_from_metadata(&meta)
    }

    async fn delete_application(&self, workspace: &str, name: &str) -> Result<(), FlameError> {
        let _guard = lock_app!(self)?;
        let app = self.read_application_metadata(workspace, name)?;
        if app.state != ApplicationState::Disabled as i32 {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> is not disabled"
            )));
        }

        let filter = SessionFilter::new(workspace).by_application(name);
        let sessions = self._list_sessions_metadata(Some(&filter))?;
        if !sessions.is_empty() {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> still has sessions"
            )));
        }

        let app_dir = self.application_path(workspace, name);
        fs::remove_dir_all(&app_dir).map_err(|e| {
            FlameError::Storage(format!("Failed to delete application '{}': {e}", name))
        })?;

        Ok(())
    }

    async fn update_application(
        &self,
        workspace: &str,
        name: &str,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        crate::apis::validate_application_url(workspace, attr.url.as_deref())?;
        let _guard = lock_app!(self)?;

        let mut meta = self.read_application_metadata(workspace, name)?;
        if meta.state != ApplicationState::Enabled as i32 {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> is not enabled"
            )));
        }

        let filter = SessionFilter::new(workspace)
            .by_application(name)
            .by_state(SessionState::Open);
        if !self._list_sessions_metadata(Some(&filter))?.is_empty() {
            return Err(FlameError::Storage(format!(
                "Cannot update application '{}': has open sessions",
                name
            )));
        }

        let schema = attr.schema.map(|s| ApplicationSchemaMetadata {
            input: s.input,
            output: s.output,
            common_data: s.common_data,
        });

        meta.version += 1;
        meta.shim = attr.shim as i32;
        meta.image = attr.image;
        meta.description = attr.description;
        meta.labels = attr.labels;
        meta.command = attr.command;
        meta.arguments = attr.arguments;
        meta.environments = attr.environments;
        meta.working_directory = attr.working_directory;
        meta.max_instances = attr.max_instances;
        meta.delay_release_seconds = attr.delay_release.num_seconds();
        meta.schema = schema;
        meta.url = attr.url;
        meta.installer = attr.installer;

        self.write_application_metadata(workspace, name, &meta)?;
        Self::application_from_metadata(&meta)
    }

    async fn get_application(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Application, FlameError> {
        let meta = self.read_application_metadata(workspace, name)?;
        Self::application_from_metadata(&meta)
    }

    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        let mut apps = Vec::new();
        let workspaces_dir = self.base_path.join(WORKSPACES);
        for workspace_entry in fs::read_dir(workspaces_dir)? {
            let workspace_entry = workspace_entry?;
            if !workspace_entry.path().is_dir() {
                continue;
            }
            let workspace = workspace_entry.file_name().to_string_lossy().to_string();
            if filter.is_some_and(|filter| filter.workspace != workspace) {
                continue;
            }
            let apps_dir = workspace_entry.path().join(APPLICATIONS);
            let Ok(entries) = fs::read_dir(apps_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let app_name = entry.file_name().to_string_lossy().to_string();
                if let Ok(meta) = self.read_application_metadata(&workspace, &app_name) {
                    if let Ok(app) = Self::application_from_metadata(&meta) {
                        let matches = filter.is_none_or(|filter| {
                            filter.state.is_none_or(|state| app.state == state)
                        });
                        if matches {
                            apps.push(app);
                        }
                    }
                }
            }
        }

        Ok(apps)
    }

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        self.require_workspace(&attr.workspace)?;
        self.read_application_metadata(&attr.workspace, &attr.application)?;
        let gid = SessionGID::new(&attr.workspace, &attr.name);
        if self.read_session_metadata(&gid).is_ok() {
            return Err(FlameError::AlreadyExist(format!(
                "Session '{}/{}' already exists",
                attr.workspace, attr.name
            )));
        }

        {
            let mut locks = lock_app!(self)?;
            locks.insert(gid.clone(), Arc::new(Mutex::new(())));
        }

        let session_dir = self.session_path(&gid);
        fs::create_dir_all(&session_dir)
            .map_err(|e| FlameError::Storage(format!("Failed to create session directory: {e}")))?;

        let common_data_len = if let Some(ref data) = attr.common_data {
            self.write_common_data(&gid, data)?;
            data.len() as u64
        } else {
            0
        };

        let meta = SessionMetadata {
            id: new_metadata_id(),
            name: attr.name.clone(),
            workspace: attr.workspace.clone(),
            application: attr.application.clone(),
            version: 1,
            state: SessionState::Open as i32,
            creation_time: Utc::now().timestamp(),
            completion_time: None,
            min_instances: attr.min_instances,
            max_instances: attr.max_instances,
            batch_size: attr.batch_size,
            priority: attr.priority,
            common_data_len,
            tokens: attr.tokens,
            resreq_cpu: attr.resreq.as_ref().map(|r| r.cpu),
            resreq_memory: attr.resreq.as_ref().map(|r| r.memory),
            resreq_gpu: attr.resreq.as_ref().map(|r| r.gpu),
        };

        self.write_session_metadata(&gid, &meta)?;

        let tasks_path = session_dir.join("tasks.bin");
        let inputs_path = session_dir.join("inputs.bin");
        let outputs_path = session_dir.join("outputs.bin");

        fs::write(&tasks_path, [])
            .map_err(|e| FlameError::Storage(format!("Failed to create tasks.bin: {e}")))?;
        fs::write(&inputs_path, [])
            .map_err(|e| FlameError::Storage(format!("Failed to create inputs.bin: {e}")))?;
        fs::write(&outputs_path, [])
            .map_err(|e| FlameError::Storage(format!("Failed to create outputs.bin: {e}")))?;

        self.session_from_metadata(&meta)
    }

    async fn get_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let meta = self.read_session_metadata(gid)?;
        self.session_from_metadata(&meta)
    }

    async fn open_session(
        &self,
        gid: &SessionGID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        // Try to get existing session
        match self.read_session_metadata(gid) {
            Ok(meta) => {
                // Session exists - validate state
                if meta.state != SessionState::Open as i32 {
                    return Err(FlameError::InvalidState(format!(
                        "Session {gid} is not open"
                    )));
                }

                let ssn = self.session_from_metadata(&meta)?;

                // If spec provided, validate full session attributes (same as sqlite engine
                // and in-memory cache). Only checking application was insufficient:
                // a persisted session could have min_instances/max_instances that differ from
                // the client spec, leading to surprising scheduling behavior.
                if let Some(ref attr) = spec {
                    ssn.validate_spec(attr)?;
                }

                Ok(ssn)
            }
            Err(_) => {
                // Session doesn't exist
                match spec {
                    Some(attr) => self.create_session(attr).await,
                    None => Err(FlameError::NotFound(format!("Session {gid} not found"))),
                }
            }
        }
    }

    async fn close_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        lock_ssn!(self, gid);

        let mut meta = self.read_session_metadata(gid)?;

        let task_count = self._count_task(&TaskFilter::new(gid.clone()))?;
        let mut pending_tasks = Vec::new();

        // First pass: check for running tasks and collect pending tasks
        for task_number in 1..=task_count {
            if let Ok(task_meta) = self.read_task_metadata(gid, task_number) {
                let state = match TaskState::try_from(task_meta.state as i32) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "Task {}/{} has corrupted state ({}): {}, treating as incomplete",
                            gid.session,
                            task_number,
                            task_meta.state,
                            e
                        );
                        return Err(FlameError::Storage(
                            "Cannot close session with corrupted task state".to_string(),
                        ));
                    }
                };
                if state == TaskState::Running {
                    return Err(FlameError::Storage(
                        "Cannot close session with running tasks".to_string(),
                    ));
                }
                if state == TaskState::Pending {
                    pending_tasks.push(task_number);
                }
            }
        }

        // Second pass: cancel pending tasks
        for task in pending_tasks {
            self._update_task_state(gid, task, TaskState::Cancelled)?;
        }

        meta.state = SessionState::Closed as i32;
        meta.completion_time = Some(Utc::now().timestamp());
        meta.version += 1;

        self.write_session_metadata(gid, &meta)?;
        self.session_from_metadata(&meta)
    }

    async fn delete_session(&self, gid: &SessionGID) -> Result<Session, FlameError> {
        let meta = self.read_session_metadata(gid)?;

        if meta.state != SessionState::Closed as i32 {
            return Err(FlameError::Storage(
                "Cannot delete open session".to_string(),
            ));
        }

        if self._count_task(
            &TaskFilter::new(gid.clone()).by_states(vec![TaskState::Pending, TaskState::Running]),
        )? > 0
        {
            return Err(FlameError::Storage(
                "Cannot delete session with non-terminal tasks".to_string(),
            ));
        }

        let session = self.session_from_metadata(&meta)?;

        let session_dir = self.session_path(gid);
        fs::remove_dir_all(&session_dir)
            .map_err(|e| FlameError::Storage(format!("Failed to delete session: {e}")))?;

        {
            let mut locks = lock_app!(self)?;
            locks.remove(gid);
        }

        Ok(session)
    }

    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError> {
        let mut sessions = Vec::new();
        for (_, metadata) in self._list_sessions_metadata(None)? {
            let session = self.session_from_metadata(&metadata)?;
            {
                let mut locks = lock_app!(self)?;
                locks
                    .entry(SessionGID::new(&metadata.workspace, &metadata.name))
                    .or_insert_with(|| Arc::new(Mutex::new(())));
            }
            sessions.push(session);
        }

        Ok(sessions)
    }

    async fn create_task(
        &self,
        gid: &SessionGID,
        input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        let ssn_meta = self.read_session_metadata(gid)?;
        if ssn_meta.state != SessionState::Open as i32 {
            return Err(FlameError::InvalidState(
                "Cannot create task in closed session".to_string(),
            ));
        }

        lock_ssn!(self, gid);

        let task_count = self._count_task(&TaskFilter::new(gid.clone()))?;
        let task_number = task_count
            .checked_add(1)
            .ok_or_else(|| FlameError::Storage("task number overflow".into()))?;

        let (input_offset, input_len) = if let Some(ref data) = input {
            let offset = self.append_data(gid, "inputs.bin", data)?;
            (offset, data.len() as u64)
        } else {
            (0, 0)
        };

        let affinity_data = options
            .unwrap_or_default()
            .affinity
            .iter()
            .map(|key| key.to_vec())
            .collect::<Vec<_>>();
        let affinity_data = bincode::encode_to_vec(affinity_data, bincode_config())
            .map_err(|e| FlameError::Storage(e.to_string()))?;
        let affinity_offset = self.append_data(gid, "affinity.bin", &affinity_data)?;

        let mut meta = TaskMetadata {
            name: task_number,
            id: *uuid::Uuid::new_v4().as_bytes(),
            version: 1,
            checksum: 0,
            state: TaskState::Pending as u8,
            creation_time: Utc::now().timestamp(),
            completion_time: 0,
            input_offset,
            input_len,
            output_offset: 0,
            output_len: 0,
            affinity_offset,
            affinity_len: affinity_data.len() as u64,
        };

        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(gid, &meta)?;

        self.task_from_metadata(gid, &meta)
    }

    async fn get_task(&self, gid: &SessionGID, task: &str) -> Result<Task, FlameError> {
        lock_ssn!(self, gid);
        let meta = self.read_task_metadata(gid, Self::parse_task_name(task)?)?;
        self.task_from_metadata(gid, &meta)
    }

    async fn retry_task(&self, gid: &SessionGID, task: &str) -> Result<Task, FlameError> {
        lock_ssn!(self, gid);

        let mut meta = self.read_task_metadata(gid, Self::parse_task_name(task)?)?;

        meta.state = TaskState::Pending as u8;
        meta.version += 1;
        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(gid, &meta)?;
        self.task_from_metadata(gid, &meta)
    }

    async fn update_task_state(
        &self,
        gid: &SessionGID,
        task: &str,
        task_state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        lock_ssn!(self, gid);

        self._update_task_state(gid, Self::parse_task_name(task)?, task_state)
    }

    async fn update_task_result(
        &self,
        gid: &SessionGID,
        task: &str,
        task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        lock_ssn!(self, gid);

        let mut meta = self.read_task_metadata(gid, Self::parse_task_name(task)?)?;

        if let Some(ref output) = task_result.output {
            let offset = self.append_data(gid, "outputs.bin", output)?;
            meta.output_offset = offset;
            meta.output_len = output.len() as u64;
        }

        meta.state = task_result.state as u8;
        meta.version += 1;

        if task_result.state.is_terminal() {
            meta.completion_time = Utc::now().timestamp();
        }

        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(gid, &meta)?;
        self.task_from_metadata(gid, &meta)
    }

    async fn find_tasks(&self, gid: &SessionGID) -> Result<Vec<Task>, FlameError> {
        lock_ssn!(self, gid);

        let mut tasks = Vec::new();
        let task_count = self._count_task(&TaskFilter::new(gid.clone()))?;

        for task_number in 1..=task_count {
            if let Ok(meta) = self.read_task_metadata(gid, task_number) {
                if let Ok(task) = self.task_from_metadata(gid, &meta) {
                    tasks.push(task);
                }
            }
        }

        Ok(tasks)
    }

    async fn create_node(&self, node: &Node) -> Result<Node, FlameError> {
        lock_node!(self, &node.name);

        let now = Utc::now().timestamp();
        let meta = NodeMetadata {
            id: if node.id.is_empty() {
                new_metadata_id()
            } else {
                node.id.clone()
            },
            name: node.name.clone(),
            state: i32::from(node.state),
            capacity_cpu: node.capacity.cpu,
            capacity_memory: node.capacity.memory,
            capacity_gpu: node.capacity.gpu,
            allocatable_cpu: node.allocatable.cpu,
            allocatable_memory: node.allocatable.memory,
            allocatable_gpu: node.allocatable.gpu,
            info_arch: node.info.arch.clone(),
            info_os: node.info.os.clone(),
            creation_time: now,
            last_heartbeat: now,
        };

        self.write_node_metadata(&node.name, &meta)?;
        let mut result = node.clone();
        result.id = meta.id;
        Ok(result)
    }

    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        lock_node!(self, name);

        match self.read_node_metadata(name) {
            Ok(meta) => Ok(Some(Node {
                id: meta.id,
                name: meta.name,
                state: NodeState::from(meta.state),
                capacity: ResourceRequirement {
                    cpu: meta.capacity_cpu,
                    memory: meta.capacity_memory,
                    gpu: meta.capacity_gpu,
                },
                allocatable: ResourceRequirement {
                    cpu: meta.allocatable_cpu,
                    memory: meta.allocatable_memory,
                    gpu: meta.allocatable_gpu,
                },
                info: NodeInfo {
                    arch: meta.info_arch,
                    os: meta.info_os,
                },
            })),
            Err(FlameError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn update_node(&self, node: &Node) -> Result<Node, FlameError> {
        lock_node!(self, &node.name);

        let existing = self.read_node_metadata(&node.name).ok();
        let creation_time = existing
            .as_ref()
            .map(|m| m.creation_time)
            .unwrap_or_else(|| Utc::now().timestamp());

        let meta = NodeMetadata {
            id: existing
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_else(new_metadata_id),
            name: node.name.clone(),
            state: i32::from(node.state),
            capacity_cpu: node.capacity.cpu,
            capacity_memory: node.capacity.memory,
            capacity_gpu: node.capacity.gpu,
            allocatable_cpu: node.allocatable.cpu,
            allocatable_memory: node.allocatable.memory,
            allocatable_gpu: node.allocatable.gpu,
            info_arch: node.info.arch.clone(),
            info_os: node.info.os.clone(),
            creation_time,
            last_heartbeat: Utc::now().timestamp(),
        };

        self.write_node_metadata(&node.name, &meta)?;
        let mut result = node.clone();
        result.id = meta.id;
        Ok(result)
    }

    async fn delete_node(&self, name: &str) -> Result<(), FlameError> {
        for executor in self.find_executors(Some(name)).await? {
            self.delete_executor(&executor.gid()).await?;
        }
        lock_node!(self, name);

        let node_dir = self.node_path(name);
        if node_dir.exists() {
            fs::remove_dir_all(&node_dir)
                .map_err(|e| FlameError::Storage(format!("Failed to delete node {name}: {e}")))?;
        }

        {
            let mut locks = self
                .node_locks
                .write()
                .map_err(|e| FlameError::Storage(format!("Node lock poisoned: {}", e)))?;
            locks.remove(name);
        }

        Ok(())
    }

    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError> {
        let mut nodes = Vec::new();
        let nodes_dir = self.base_path.join("nodes");

        if let Ok(entries) = fs::read_dir(&nodes_dir) {
            for entry in entries.flatten() {
                let node_name = entry.file_name().to_string_lossy().to_string();
                if let Ok(meta) = self.read_node_metadata(&node_name) {
                    nodes.push(Node {
                        id: meta.id,
                        name: meta.name,
                        state: NodeState::from(meta.state),
                        capacity: ResourceRequirement {
                            cpu: meta.capacity_cpu,
                            memory: meta.capacity_memory,
                            gpu: meta.capacity_gpu,
                        },
                        allocatable: ResourceRequirement {
                            cpu: meta.allocatable_cpu,
                            memory: meta.allocatable_memory,
                            gpu: meta.allocatable_gpu,
                        },
                        info: NodeInfo {
                            arch: meta.info_arch,
                            os: meta.info_os,
                        },
                    });
                }
            }
        }

        Ok(nodes)
    }

    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        self.validate_executor_references(executor)?;
        let gid = executor.gid();
        lock_executor!(self, &gid);
        if self.read_executor_metadata(&gid).is_ok() {
            return Err(FlameError::AlreadyExist(format!(
                "executor {}/{}",
                executor.workspace, executor.name
            )));
        }
        self.write_executor_metadata(&gid, &Self::executor_to_metadata(executor))?;
        Ok(executor.clone())
    }

    async fn get_executor(&self, gid: &ExecutorGID) -> Result<Option<Executor>, FlameError> {
        lock_executor!(self, gid);
        match self.read_executor_metadata(gid) {
            Ok(meta) => Ok(Some(Self::executor_from_metadata(meta))),
            Err(FlameError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        self.validate_executor_references(executor)?;
        let gid = executor.gid();
        lock_executor!(self, &gid);
        let current = self.read_executor_metadata(&gid)?;
        if current.id != executor.id {
            return Err(FlameError::InvalidConfig(
                "executor metadata ID mismatch".to_string(),
            ));
        }
        self.write_executor_metadata(&gid, &Self::executor_to_metadata(executor))?;
        Ok(executor.clone())
    }

    async fn update_executor_state(
        &self,
        gid: &ExecutorGID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        lock_executor!(self, gid);
        let mut meta = self.read_executor_metadata(gid)?;
        meta.state = i32::from(state);
        self.write_executor_metadata(gid, &meta)?;
        Ok(Self::executor_from_metadata(meta))
    }

    async fn delete_executor(&self, gid: &ExecutorGID) -> Result<(), FlameError> {
        lock_executor!(self, gid);
        let path = self.executor_path(gid);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        self.executor_locks
            .write()
            .map_err(|error| FlameError::Storage(format!("Executor lock poisoned: {error}")))?
            .remove(gid);
        Ok(())
    }

    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        let mut executors = Vec::new();
        for workspace_entry in fs::read_dir(self.base_path.join(WORKSPACES))? {
            let workspace_entry = workspace_entry?;
            if !workspace_entry.path().is_dir() {
                continue;
            }
            let workspace = workspace_entry.file_name().to_string_lossy().to_string();
            let Ok(entries) = fs::read_dir(workspace_entry.path().join(EXECUTORS)) else {
                continue;
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let meta = self.read_executor_metadata(&ExecutorGID::new(&workspace, &name))?;
                if node.is_none_or(|expected| meta.node == expected) {
                    executors.push(Self::executor_from_metadata(meta));
                }
            }
        }
        Ok(executors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_test_engine() -> (FilesystemEngine, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let url = format!("filesystem://{}", temp_dir.path().display());
        let _engine_ptr = FilesystemEngine::new_ptr(&url).await.unwrap();

        let engine = FilesystemEngine {
            base_path: temp_dir.path().to_path_buf(),
            record_size: task_record_size(),
            ssn_locks: RwLock::new(HashMap::new()),
            node_locks: RwLock::new(HashMap::new()),
            executor_locks: RwLock::new(HashMap::new()),
        };

        (engine, temp_dir)
    }

    #[tokio::test]
    async fn default_workspace_is_read_without_reinitialization() {
        let dir = TempDir::new().unwrap();
        let url = format!("filesystem://{}", dir.path().display());
        let engine = FilesystemEngine::new_ptr(&url).await.unwrap();
        let workspace = engine.list_workspaces().await.unwrap().remove(0);
        let path = dir.path().join(WORKSPACES).join(DEFAULT_WORKSPACE);
        let metadata = fs::read(path.join("workspace.json")).unwrap();
        fs::remove_dir(path.join(EXECUTORS)).unwrap();
        let reopened = FilesystemEngine::new_ptr(&url).await.unwrap();
        assert_eq!(reopened.list_workspaces().await.unwrap(), vec![workspace]);
        assert_eq!(fs::read(path.join("workspace.json")).unwrap(), metadata);
        assert!(!path.join(EXECUTORS).exists());
        assert!(matches!(
            engine.create_workspace(DEFAULT_WORKSPACE.to_string()).await,
            Err(FlameError::AlreadyExist(_))
        ));
    }

    #[tokio::test]
    async fn existing_default_workspace_requires_readable_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(WORKSPACES).join(DEFAULT_WORKSPACE);
        fs::create_dir_all(&path).unwrap();
        let url = format!("filesystem://{}", dir.path().display());
        assert!(FilesystemEngine::new_ptr(&url).await.is_err());
        assert!(!path.join("workspace.json").exists());
        for directory in [SESSIONS, APPLICATIONS, EXECUTORS] {
            assert!(!path.join(directory).exists());
        }
    }

    #[tokio::test]
    async fn test_record_size_is_constant() {
        let size1 = task_record_size();
        let size2 = task_record_size();
        assert_eq!(size1, size2);

        // Verify different metadata values produce same size
        let meta1 = TaskMetadata::default();
        let meta2 = TaskMetadata {
            name: u64::MAX,
            id: [255; 16],
            version: u32::MAX,
            checksum: u32::MAX,
            state: 255,
            creation_time: i64::MAX,
            completion_time: i64::MAX,
            input_offset: u64::MAX,
            input_len: u64::MAX,
            output_offset: u64::MAX,
            output_len: u64::MAX,
            affinity_offset: u64::MAX,
            affinity_len: u64::MAX,
        };

        let buf1 = bincode::encode_to_vec(&meta1, bincode_config()).unwrap();
        let buf2 = bincode::encode_to_vec(&meta2, bincode_config()).unwrap();

        assert_eq!(buf1.len(), buf2.len());
    }

    #[tokio::test]
    async fn test_checksum_calculation() {
        let meta = TaskMetadata {
            name: 1,
            id: [1; 16],
            version: 1,
            checksum: 0,
            state: TaskState::Pending as u8,
            creation_time: 1234567890,
            completion_time: 0,
            input_offset: 0,
            input_len: 100,
            output_offset: 0,
            output_len: 0,
            affinity_offset: 0,
            affinity_len: 0,
        };

        let checksum1 = calculate_checksum(&meta);
        let checksum2 = calculate_checksum(&meta);

        assert_eq!(checksum1, checksum2);

        // Different metadata should produce different checksum
        let meta2 = TaskMetadata { name: 2, ..meta };

        let checksum3 = calculate_checksum(&meta2);
        assert_ne!(checksum1, checksum3);
    }

    #[tokio::test]
    async fn test_url_parsing() {
        std::env::set_var("FLAME_HOME", "/opt/flame");

        let path1 = FilesystemEngine::parse_url("filesystem:///var/lib/flame").unwrap();
        assert_eq!(path1, PathBuf::from("/var/lib/flame"));

        let path2 = FilesystemEngine::parse_url("file:///tmp/flame").unwrap();
        assert_eq!(path2, PathBuf::from("/tmp/flame"));

        let path3 = FilesystemEngine::parse_url("fs:///data").unwrap();
        assert_eq!(path3, PathBuf::from("/data"));

        let path4 = FilesystemEngine::parse_url("fs://data").unwrap();
        assert_eq!(path4, PathBuf::from("/opt/flame/data"));

        let path5 = FilesystemEngine::parse_url("filesystem://data/sessions").unwrap();
        assert_eq!(path5, PathBuf::from("/opt/flame/data/sessions"));

        let path6 = FilesystemEngine::parse_url("file://storage").unwrap();
        assert_eq!(path6, PathBuf::from("/opt/flame/storage"));

        std::env::remove_var("FLAME_HOME");

        let err = FilesystemEngine::parse_url("sqlite:///tmp/flame.db");
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_application_lifecycle() {
        let (engine, _temp_dir) = create_test_engine().await;

        // Register application
        let attr = ApplicationAttributes {
            shim: Shim::Host,
            image: Some("test-image".to_string()),
            description: Some("Test application".to_string()),
            labels: vec!["test".to_string()],
            command: Some("/bin/test".to_string()),
            arguments: vec!["--arg1".to_string()],
            environments: std::collections::HashMap::new(),
            working_directory: Some("/tmp".to_string()),
            max_instances: 10,
            delay_release: Duration::seconds(60),
            schema: None,
            url: None,
            installer: None,
        };

        let app = engine
            .register_application("default".into(), "test-app".to_string(), attr.clone())
            .await
            .unwrap();
        assert_eq!(app.name, "test-app");
        assert_eq!(app.state, ApplicationState::Enabled);

        // Get application
        let app2 = engine.get_application("default", "test-app").await.unwrap();
        assert_eq!(app2.name, "test-app");

        // Find applications
        let apps = engine
            .find_applications(Some(&ApplicationFilter::new("default")))
            .await
            .unwrap();
        assert_eq!(apps.len(), 1);

        // Update application
        let updated_attr = ApplicationAttributes {
            shim: Shim::Cri,
            image: Some("updated-image".to_string()),
            description: Some("Updated description".to_string()),
            ..attr
        };
        let app3 = engine
            .update_application("default", "test-app", updated_attr)
            .await
            .unwrap();
        assert_eq!(app3.description, Some("Updated description".to_string()));
        assert_eq!(app3.shim, Shim::Cri);
        assert_eq!(app3.image.as_deref(), Some("updated-image"));
        assert_eq!(app3.version, 2);

        let result = engine.delete_application("default", "test-app").await;
        assert!(matches!(result, Err(FlameError::InvalidState(_))));

        let disabled = engine
            .update_application_state("default", "test-app", ApplicationState::Disabled)
            .await
            .unwrap();
        assert_eq!(disabled.version, 3);
        let unchanged = engine
            .update_application_state("default", "test-app", ApplicationState::Disabled)
            .await
            .unwrap();
        assert_eq!(unchanged.version, disabled.version);

        let result = engine
            .update_application(
                "default",
                "test-app",
                ApplicationAttributes {
                    image: Some("must-not-be-written".to_string()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(result, Err(FlameError::InvalidState(_))));
        let unchanged = engine.get_application("default", "test-app").await.unwrap();
        assert_eq!(unchanged.version, disabled.version);
        assert_eq!(unchanged.image, disabled.image);

        let filter = ApplicationFilter::new("default").by_state(ApplicationState::Disabled);
        let disabled_apps = engine.find_applications(Some(&filter)).await.unwrap();
        assert_eq!(disabled_apps.len(), 1);
        assert_eq!(disabled_apps[0].name, "test-app");

        engine
            .delete_application("default", "test-app")
            .await
            .unwrap();

        // Verify it's gone
        let result = engine.get_application("default", "test-app").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_application_deletion_waits_for_open_sessions() {
        let (engine, _temp_dir) = create_test_engine().await;
        engine
            .register_application(
                "default".into(),
                "test-app".to_string(),
                ApplicationAttributes::default(),
            )
            .await
            .unwrap();
        engine
            .create_session(SessionAttributes {
                name: "test-session".to_string(),
                workspace: "default".into(),
                application: "test-app".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        engine
            .update_application_state("default", "test-app", ApplicationState::Disabled)
            .await
            .unwrap();

        let open_sessions = SessionFilter::new("default")
            .by_application("test-app")
            .by_state(SessionState::Open);
        assert_eq!(
            engine
                ._list_sessions_metadata(Some(&open_sessions))
                .unwrap()
                .len(),
            1
        );
        let result = engine.delete_application("default", "test-app").await;
        assert!(matches!(result, Err(FlameError::InvalidState(_))));

        engine
            .close_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        let result = engine.delete_application("default", "test-app").await;
        assert!(matches!(result, Err(FlameError::InvalidState(_))));
        assert!(engine
            .get_session(&SessionGID::new("default", "test-session"))
            .await
            .is_ok());

        engine
            .delete_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        engine
            .delete_application("default", "test-app")
            .await
            .unwrap();
        assert!(matches!(
            engine
                .get_session(&SessionGID::new("default", "test-session"))
                .await,
            Err(FlameError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn test_application_deletion_fails_closed_on_unreadable_session() {
        let (engine, _temp_dir) = create_test_engine().await;
        engine
            .register_application(
                "default".into(),
                "test-app".to_string(),
                ApplicationAttributes::default(),
            )
            .await
            .unwrap();
        engine
            .update_application_state("default", "test-app", ApplicationState::Disabled)
            .await
            .unwrap();
        fs::create_dir_all(engine.session_path(&SessionGID::new("default", "incomplete-session")))
            .unwrap();

        let sessions = SessionFilter::new("default").by_application("test-app");
        assert!(engine._list_sessions_metadata(Some(&sessions)).is_err());
        assert!(engine
            .delete_application("default", "test-app")
            .await
            .is_err());
        assert!(engine.get_application("default", "test-app").await.is_ok());
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let (engine, _temp_dir) = create_test_engine().await;

        // First register an application
        let app_attr = ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: None,
            labels: vec![],
            command: Some("/bin/test".to_string()),
            arguments: vec![],
            environments: std::collections::HashMap::new(),
            working_directory: None,
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        };
        engine
            .register_application("default".into(), "test-app".to_string(), app_attr)
            .await
            .unwrap();

        // Create session
        let ssn_attr = SessionAttributes {
            tokens: [("service".into(), "credential".into())].into(),
            name: "test-session".to_string(),
            workspace: "default".into(),
            application: "test-app".to_string(),
            common_data: Some(Bytes::from("test data")),
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
        };

        let session = engine.create_session(ssn_attr).await.unwrap();
        assert_eq!(session.name, "test-session");
        assert_eq!(session.status.state, SessionState::Open);

        // Get session
        let session2 = engine
            .get_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        assert_eq!(session2.name, "test-session");
        assert_eq!(
            session2.tokens.get("service").map(String::as_str),
            Some("credential")
        );

        // Find sessions
        let sessions = engine.find_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);

        // Close session (should work since no tasks)
        let closed = engine
            .close_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        assert_eq!(closed.status.state, SessionState::Closed);

        // Delete session
        let deleted = engine
            .delete_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        assert_eq!(deleted.name, "test-session");
    }

    #[tokio::test]
    async fn test_task_lifecycle() {
        let (engine, _temp_dir) = create_test_engine().await;

        // Setup: register app and create session
        let app_attr = ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: None,
            labels: vec![],
            command: Some("/bin/test".to_string()),
            arguments: vec![],
            environments: std::collections::HashMap::new(),
            working_directory: None,
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        };
        engine
            .register_application("default".into(), "test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            tokens: Default::default(),
            name: "test-session".to_string(),
            workspace: "default".into(),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
        };
        engine.create_session(ssn_attr).await.unwrap();

        // Create task with input
        let input = Bytes::from("test input data");
        let task = engine
            .create_task(
                &SessionGID::new("default", "test-session"),
                Some(input.clone()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(task.name, 1);
        assert_eq!(task.state, TaskState::Pending);
        assert_eq!(task.input, Some(input));

        for invalid_name in ["01", "+1", "not-a-number", "18446744073709551616"] {
            assert!(matches!(
                engine
                    .get_task(&SessionGID::new("default", "test-session"), invalid_name)
                    .await,
                Err(FlameError::InvalidConfig(_))
            ));
        }
        assert!(matches!(
            engine
                .get_task(&SessionGID::new("default", "test-session"), "0")
                .await,
            Err(FlameError::NotFound(_))
        ));

        // Get task
        let gid = "1";
        let task2 = engine
            .get_task(&SessionGID::new("default", "test-session"), gid)
            .await
            .unwrap();
        assert_eq!(task2.name, 1);

        // Update task state
        let task3 = engine
            .update_task_state(
                &SessionGID::new("default", "test-session"),
                gid,
                TaskState::Running,
                None,
            )
            .await
            .unwrap();
        assert_eq!(task3.state, TaskState::Running);

        // Update task result
        let output = Bytes::from("test output data");
        let result = TaskResult {
            state: TaskState::Succeed,
            output: Some(output.clone()),
            message: None,
        };
        let task4 = engine
            .update_task_result(&SessionGID::new("default", "test-session"), gid, result)
            .await
            .unwrap();
        assert_eq!(task4.state, TaskState::Succeed);
        assert_eq!(task4.output, Some(output));

        // Find tasks
        let tasks = engine
            .find_tasks(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1);

        // Create another task
        let task5 = engine
            .create_task(&SessionGID::new("default", "test-session"), None, None)
            .await
            .unwrap();
        assert_eq!(task5.name, 2);

        // Complete second task
        let gid2 = "2";
        engine
            .update_task_state(
                &SessionGID::new("default", "test-session"),
                gid2,
                TaskState::Succeed,
                None,
            )
            .await
            .unwrap();

        // Now we can close the session
        let closed = engine
            .close_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        assert_eq!(closed.status.state, SessionState::Closed);
    }

    #[tokio::test]
    async fn test_register_application_already_exists() {
        let (engine, _temp_dir) = create_test_engine().await;

        let attr = ApplicationAttributes {
            shim: Shim::Host,
            image: Some("test-image".to_string()),
            description: Some("Test application".to_string()),
            labels: vec![],
            command: Some("/bin/test".to_string()),
            arguments: vec![],
            environments: std::collections::HashMap::new(),
            working_directory: None,
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        };

        engine
            .register_application("default".into(), "test-app".to_string(), attr.clone())
            .await
            .unwrap();

        let result = engine
            .register_application("default".into(), "test-app".to_string(), attr)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FlameError::AlreadyExist(_)));
    }

    #[tokio::test]
    async fn test_create_session_already_exists() {
        let (engine, _temp_dir) = create_test_engine().await;

        let app_attr = ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: None,
            labels: vec![],
            command: Some("/bin/test".to_string()),
            arguments: vec![],
            environments: std::collections::HashMap::new(),
            working_directory: None,
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        };
        engine
            .register_application("default".into(), "test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            tokens: Default::default(),
            name: "test-session".to_string(),
            workspace: "default".into(),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
        };

        engine.create_session(ssn_attr.clone()).await.unwrap();

        let result = engine.create_session(ssn_attr).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FlameError::AlreadyExist(_)));
    }

    #[tokio::test]
    async fn test_close_session_with_pending_tasks() {
        let (engine, _temp_dir) = create_test_engine().await;

        let app_attr = ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: None,
            labels: vec![],
            command: Some("/bin/test".to_string()),
            arguments: vec![],
            environments: std::collections::HashMap::new(),
            working_directory: None,
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        };
        engine
            .register_application("default".into(), "test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            tokens: Default::default(),
            name: "test-session".to_string(),
            workspace: "default".into(),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
        };
        engine.create_session(ssn_attr).await.unwrap();

        let task1 = engine
            .create_task(&SessionGID::new("default", "test-session"), None, None)
            .await
            .unwrap();
        assert_eq!(task1.state, TaskState::Pending);

        let task2 = engine
            .create_task(&SessionGID::new("default", "test-session"), None, None)
            .await
            .unwrap();
        assert_eq!(task2.state, TaskState::Pending);

        let closed = engine
            .close_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
        assert_eq!(closed.status.state, SessionState::Closed);

        let task1_after = engine
            .get_task(
                &SessionGID::new("default", "test-session"),
                &task1.name.to_string(),
            )
            .await
            .unwrap();
        assert_eq!(task1_after.state, TaskState::Cancelled);

        let task2_after = engine
            .get_task(
                &SessionGID::new("default", "test-session"),
                &task2.name.to_string(),
            )
            .await
            .unwrap();
        assert_eq!(task2_after.state, TaskState::Cancelled);

        engine
            .delete_session(&SessionGID::new("default", "test-session"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_close_session_with_running_tasks() {
        let (engine, _temp_dir) = create_test_engine().await;

        let app_attr = ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: None,
            labels: vec![],
            command: Some("/bin/test".to_string()),
            arguments: vec![],
            environments: std::collections::HashMap::new(),
            working_directory: None,
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        };
        engine
            .register_application("default".into(), "test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            tokens: Default::default(),
            name: "test-session".to_string(),
            workspace: "default".into(),
            application: "test-app".to_string(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
            resreq: None,
        };
        engine.create_session(ssn_attr).await.unwrap();

        let task = engine
            .create_task(&SessionGID::new("default", "test-session"), None, None)
            .await
            .unwrap();

        engine
            .update_task_state(
                &SessionGID::new("default", "test-session"),
                &task.name.to_string(),
                TaskState::Running,
                None,
            )
            .await
            .unwrap();

        let result = engine
            .close_session(&SessionGID::new("default", "test-session"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_node_crud() {
        let (engine, _temp_dir) = create_test_engine().await;

        let node = Node {
            id: new_metadata_id(),
            name: "test-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
                gpu: 0,
            },
            allocatable: ResourceRequirement {
                cpu: 6,
                memory: 12288,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };

        let created = engine.create_node(&node).await.unwrap();
        assert_eq!(created.name, "test-node");

        let found = engine.get_node("test-node").await.unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.name, "test-node");
        assert_eq!(found.capacity.cpu, 8);

        let mut updated_node = node.clone();
        updated_node.state = NodeState::NotReady;
        let updated = engine.update_node(&updated_node).await.unwrap();
        assert_eq!(updated.state, NodeState::NotReady);

        let nodes = engine.find_nodes().await.unwrap();
        assert_eq!(nodes.len(), 1);

        engine.delete_node("test-node").await.unwrap();
        let deleted = engine.get_node("test-node").await.unwrap();
        assert!(deleted.is_none());
    }

    #[tokio::test]
    async fn test_executor_crud() {
        let (engine, _temp_dir) = create_test_engine().await;
        engine
            .register_application(
                "default".into(),
                "test-app".into(),
                ApplicationAttributes::default(),
            )
            .await
            .unwrap();

        let node = Node {
            id: new_metadata_id(),
            name: "exec-test-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };
        engine.create_node(&node).await.unwrap();

        let executor = crate::apis::Executor {
            id: new_metadata_id(),
            name: "exec-1".into(),
            workspace: "default".into(),
            node: "exec-test-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            },
            shim: Shim::Host,
            application: "test-app".to_string(),
            task: None,
            session: None,
            attributes: Default::default(),
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::Void,
        };

        let created = engine.create_executor(&executor).await.unwrap();
        assert_eq!(created.name, "exec-1");
        assert_eq!(created.application, "test-app");

        let found = engine
            .get_executor(&ExecutorGID::new("default", "exec-1"))
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().application, "test-app");

        let updated = engine
            .update_executor_state(&ExecutorGID::new("default", "exec-1"), ExecutorState::Idle)
            .await
            .unwrap();
        assert_eq!(updated.state, ExecutorState::Idle);
        assert_eq!(updated.application, "test-app");

        let executors = engine.find_executors(None).await.unwrap();
        assert_eq!(executors.len(), 1);
        assert_eq!(executors[0].application, "test-app");

        let by_node = engine.find_executors(Some("exec-test-node")).await.unwrap();
        assert_eq!(by_node.len(), 1);

        let by_other = engine.find_executors(Some("other-node")).await.unwrap();
        assert_eq!(by_other.len(), 0);

        engine
            .delete_executor(&ExecutorGID::new("default", "exec-1"))
            .await
            .unwrap();
        let deleted = engine
            .get_executor(&ExecutorGID::new("default", "exec-1"))
            .await
            .unwrap();
        assert!(deleted.is_none());
    }

    #[tokio::test]
    async fn test_node_delete_cascades_to_executors() {
        let (engine, _temp_dir) = create_test_engine().await;
        engine
            .register_application(
                "default".into(),
                "test-app".into(),
                ApplicationAttributes::default(),
            )
            .await
            .unwrap();

        let node = Node {
            id: new_metadata_id(),
            name: "cascade-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };
        engine.create_node(&node).await.unwrap();

        for i in 1..=3 {
            let executor = crate::apis::Executor {
                id: new_metadata_id(),
                name: format!("cascade-exec-{i}"),
                workspace: "default".into(),
                node: "cascade-node".to_string(),
                resreq: ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                    gpu: 0,
                },
                shim: Shim::Host,
                application: "test-app".to_string(),
                task: None,
                session: None,
                attributes: Default::default(),
                creation_time: Utc::now(),
                latest_updated_timestamp: Utc::now(),
                state: ExecutorState::Void,
            };
            engine.create_executor(&executor).await.unwrap();
        }

        let executors = engine.find_executors(Some("cascade-node")).await.unwrap();
        assert_eq!(executors.len(), 3);

        engine.delete_node("cascade-node").await.unwrap();

        let executors = engine.find_executors(None).await.unwrap();
        assert_eq!(executors.len(), 0);
    }
    #[tokio::test]
    async fn test_workspace_names_and_uuid_survive_restart() {
        let (engine, temp) = create_test_engine().await;
        engine.create_workspace("other".into()).await.unwrap();
        let mut ids = Vec::new();
        for workspace in ["default", "other"] {
            let app = engine
                .register_application(
                    workspace.into(),
                    "app".into(),
                    ApplicationAttributes::default(),
                )
                .await
                .unwrap();
            let session = engine
                .create_session(SessionAttributes {
                    workspace: workspace.into(),
                    name: "session".into(),
                    application: "app".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let task = engine
                .create_task(
                    &SessionGID::new(workspace, "session"),
                    Some(Bytes::from(workspace.to_owned())),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(task.name, 1);
            for id in [&app.id, &session.id, &task.id] {
                assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 4);
            }
            ids.push(task.id);
        }
        assert_ne!(ids[0], ids[1]);
        let reopened =
            FilesystemEngine::new_ptr(&format!("filesystem://{}", temp.path().display()))
                .await
                .unwrap();
        for (index, workspace) in ["default", "other"].into_iter().enumerate() {
            let session = reopened
                .get_session(&SessionGID::new(workspace, "session"))
                .await
                .unwrap();
            assert_eq!(session.workspace, workspace);
            let task = reopened
                .get_task(&SessionGID::new(workspace, "session"), "1")
                .await
                .unwrap();
            assert_eq!(task.id, ids[index]);
            assert_eq!(task.input, Some(Bytes::from(workspace.to_owned())));
            assert!(reopened
                .get_task(&SessionGID::new(workspace, "session"), &task.id)
                .await
                .is_err());
        }
    }
}

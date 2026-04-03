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
//! <work_dir>/data/
//! ├── sessions/<session_id>/
//! │   ├── metadata          # Session metadata (JSON)
//! │   ├── tasks.bin         # TaskMetadata records (fixed-size, indexed by Task ID)
//! │   ├── inputs.bin        # Concatenated input data (append-only)
//! │   └── outputs.bin       # Concatenated output data (append-only)
//! └── applications/<app_name>/
//!     └── metadata          # Application metadata (JSON)
//! ```
//!
//! # Design Decisions
//!
//! - **Fixed-size task metadata**: Enables O(1) random access by task ID
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

use common::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationSchema, ApplicationState,
    ExecutorID, ExecutorState, Node, NodeInfo, NodeState, ResourceRequirement, Role, Session,
    SessionAttributes, SessionID, SessionState, SessionStatus, Shim, Task, TaskGID, TaskID,
    TaskInput, TaskOutput, TaskResult, TaskState, User, Workspace,
};
use common::{FlameError, FLAME_HOME};

use crate::model::Executor;
use crate::storage::engine::{Engine, EnginePtr};

/// Task metadata stored in tasks.bin with fixed-size records.
///
/// Uses `bincode` with `fixint` encoding to ensure constant serialized size.
/// The checksum is calculated using `crc32fast` for data integrity.
#[derive(Encode, Decode, Debug, Clone, Default)]
struct TaskMetadata {
    /// Task ID (index in file)
    pub id: u64,
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct SessionMetadata {
    pub id: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
    pub application: String,
    pub slots: u32,
    pub version: u32,
    pub state: i32,
    pub creation_time: i64,
    pub completion_time: Option<i64>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub common_data_len: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ApplicationMetadata {
    pub name: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
    pub version: u32,
    pub state: i32,
    pub creation_time: i64,
    #[serde(default)]
    pub shim: i32,
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
}

fn default_workspace() -> String {
    common::apis::WORKSPACE_DEFAULT.to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ApplicationSchemaMetadata {
    pub input: Option<String>,
    pub output: Option<String>,
    pub common_data: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct NodeMetadata {
    pub name: String,
    pub state: i32,
    pub capacity_cpu: u64,
    pub capacity_memory: u64,
    pub allocatable_cpu: u64,
    pub allocatable_memory: u64,
    pub info_arch: String,
    pub info_os: String,
    pub creation_time: i64,
    pub last_heartbeat: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ExecutorMetadata {
    pub id: String,
    pub node: String,
    pub resreq_cpu: u64,
    pub resreq_memory: u64,
    pub slots: u32,
    pub shim: i32,
    pub task_id: Option<i64>,
    pub ssn_id: Option<String>,
    pub creation_time: i64,
    pub state: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct UserMetadata {
    pub name: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub certificate_cn: String,
    pub enabled: bool,
    pub creation_time: i64,
    pub last_login_time: Option<i64>,
    pub roles: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RoleMetadata {
    pub name: String,
    pub description: Option<String>,
    pub permissions: Vec<String>,
    pub workspaces: Vec<String>,
    pub creation_time: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct WorkspaceMetadata {
    pub name: String,
    pub description: Option<String>,
    pub labels: std::collections::HashMap<String, String>,
    pub creation_time: i64,
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
    hasher.update(&meta.id.to_le_bytes());
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

pub struct FilesystemEngine {
    base_path: PathBuf,
    record_size: usize,
    ssn_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    node_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    executor_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

macro_rules! lock_ssn {
    ($self:expr, $ssn_id:expr) => {
        let __fs_local_ssn_lock = {
            let locks = $self
                .ssn_locks
                .read()
                .map_err(|e| FlameError::Storage(format!("Session lock poisoned: {}", e)))?;
            locks.get($ssn_id).cloned().ok_or_else(|| {
                FlameError::NotFound(format!("Session lock not found: {}", $ssn_id))
            })?
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
    ($self:expr, $executor_id:expr) => {
        let __fs_local_exec_lock = {
            let mut locks = $self
                .executor_locks
                .write()
                .map_err(|e| FlameError::Storage(format!("Executor lock poisoned: {}", e)))?;
            locks
                .entry($executor_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let __fs_local_exec_guard = __fs_local_exec_lock
            .lock()
            .map_err(|e| FlameError::Storage(format!("Executor lock poisoned: {}", e)))?;
    };
}

impl FilesystemEngine {
    /// Create a new filesystem engine from a URL.
    ///
    /// URL format: `filesystem://<path>` or `file://<path>`
    pub async fn new_ptr(url: &str) -> Result<EnginePtr, FlameError> {
        let path = Self::parse_url(url)?;

        let sessions_path = path.join("sessions");
        let applications_path = path.join("applications");
        let nodes_path = path.join("nodes");
        let users_path = path.join("users");
        let roles_path = path.join("roles");
        let workspaces_path = path.join("workspaces");

        fs::create_dir_all(&sessions_path).map_err(|e| {
            FlameError::Storage(format!("Failed to create sessions directory: {e}"))
        })?;
        fs::create_dir_all(&applications_path).map_err(|e| {
            FlameError::Storage(format!("Failed to create applications directory: {e}"))
        })?;
        fs::create_dir_all(&nodes_path)
            .map_err(|e| FlameError::Storage(format!("Failed to create nodes directory: {e}")))?;
        fs::create_dir_all(&users_path)
            .map_err(|e| FlameError::Storage(format!("Failed to create users directory: {e}")))?;
        fs::create_dir_all(&roles_path)
            .map_err(|e| FlameError::Storage(format!("Failed to create roles directory: {e}")))?;
        fs::create_dir_all(&workspaces_path).map_err(|e| {
            FlameError::Storage(format!("Failed to create workspaces directory: {e}"))
        })?;

        let record_size = task_record_size();
        tracing::info!(
            "Filesystem storage engine initialized at {:?} with record size {}",
            path,
            record_size
        );

        Ok(Arc::new(FilesystemEngine {
            base_path: path,
            record_size,
            ssn_locks: RwLock::new(HashMap::new()),
            node_locks: RwLock::new(HashMap::new()),
            executor_locks: RwLock::new(HashMap::new()),
        }))
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

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.base_path.join("sessions").join(session_id)
    }

    fn application_path(&self, app_name: &str) -> PathBuf {
        self.base_path.join("applications").join(app_name)
    }

    fn node_path(&self, node_name: &str) -> PathBuf {
        self.base_path.join("nodes").join(node_name)
    }

    fn executor_path(&self, node_name: &str, executor_id: &str) -> PathBuf {
        self.node_path(node_name)
            .join("executors")
            .join(executor_id)
    }

    /// Read session metadata from disk.
    fn read_session_metadata(&self, session_id: &str) -> Result<SessionMetadata, FlameError> {
        let path = self.session_path(session_id).join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!("Session {session_id} not found: {e}"))
            } else {
                FlameError::Storage(format!(
                    "Failed to read session metadata for {session_id}: {e}"
                ))
            }
        })?;
        serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("Failed to parse session metadata: {e}")))
    }

    /// Write session metadata to disk atomically.
    fn write_session_metadata(
        &self,
        session_id: &str,
        meta: &SessionMetadata,
    ) -> Result<(), FlameError> {
        let session_dir = self.session_path(session_id);
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
    fn read_application_metadata(&self, app_name: &str) -> Result<ApplicationMetadata, FlameError> {
        let path = self.application_path(app_name).join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!("Application {app_name} not found: {e}"))
            } else {
                FlameError::Storage(format!(
                    "Failed to read application metadata for {app_name}: {e}"
                ))
            }
        })?;
        serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("Failed to parse application metadata: {e}")))
    }

    /// Write application metadata to disk atomically.
    fn write_application_metadata(
        &self,
        app_name: &str,
        meta: &ApplicationMetadata,
    ) -> Result<(), FlameError> {
        let app_dir = self.application_path(app_name);
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

    fn read_executor_metadata(
        &self,
        node_name: &str,
        executor_id: &str,
    ) -> Result<ExecutorMetadata, FlameError> {
        let path = self.executor_path(node_name, executor_id).join("metadata");
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FlameError::NotFound(format!("Executor {executor_id} not found"))
            } else {
                FlameError::Storage(format!(
                    "Failed to read executor metadata for {executor_id}: {e}"
                ))
            }
        })?;
        serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("Failed to parse executor metadata: {e}")))
    }

    fn write_executor_metadata(
        &self,
        node_name: &str,
        executor_id: &str,
        meta: &ExecutorMetadata,
    ) -> Result<(), FlameError> {
        let exec_dir = self.executor_path(node_name, executor_id);
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

    fn find_executor_node(&self, executor_id: &str) -> Result<String, FlameError> {
        let nodes_dir = self.base_path.join("nodes");
        if let Ok(entries) = fs::read_dir(&nodes_dir) {
            for entry in entries.flatten() {
                let node_name = entry.file_name().to_string_lossy().to_string();
                let exec_path = self.executor_path(&node_name, executor_id).join("metadata");
                if exec_path.exists() {
                    return Ok(node_name);
                }
            }
        }
        Err(FlameError::NotFound(format!(
            "Executor {executor_id} not found"
        )))
    }

    /// Read task metadata from tasks.bin.
    fn read_task_metadata(
        &self,
        session_id: &str,
        task_id: TaskID,
    ) -> Result<TaskMetadata, FlameError> {
        if task_id < 1 {
            return Err(FlameError::NotFound(format!(
                "Invalid task ID: {task_id} (must be >= 1)"
            )));
        }

        let path = self.session_path(session_id).join("tasks.bin");

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|e| FlameError::NotFound(format!("Tasks file not found: {e}")))?;

        let offset = (task_id as u64 - 1) * self.record_size as u64;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FlameError::Storage(format!("Failed to seek to task {task_id}: {e}")))?;

        let mut buffer = vec![0u8; self.record_size];
        file.read_exact(&mut buffer)
            .map_err(|e| FlameError::NotFound(format!("Task {task_id} not found: {e}")))?;

        let (meta, _): (TaskMetadata, _) = bincode::decode_from_slice(&buffer, bincode_config())
            .map_err(|e| {
                FlameError::Storage(format!("Failed to deserialize task metadata: {e}"))
            })?;

        // Verify checksum
        let expected_checksum = calculate_checksum(&meta);
        if meta.checksum != expected_checksum {
            return Err(FlameError::Storage(format!(
                "Task {task_id} checksum mismatch: expected {expected_checksum}, got {}",
                meta.checksum
            )));
        }

        Ok(meta)
    }

    /// Write task metadata to tasks.bin at the specified offset.
    fn write_task_metadata(&self, session_id: &str, meta: &TaskMetadata) -> Result<(), FlameError> {
        let path = self.session_path(session_id).join("tasks.bin");

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| FlameError::Storage(format!("Failed to open tasks file: {e}")))?;

        let offset = (meta.id - 1) * self.record_size as u64;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FlameError::Storage(format!("Failed to seek to task {}: {e}", meta.id)))?;

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
        session_id: &str,
        filename: &str,
        data: &[u8],
    ) -> Result<u64, FlameError> {
        let path = self.session_path(session_id).join(filename);

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
        session_id: &str,
        filename: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FlameError> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let path = self.session_path(session_id).join(filename);

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

    /// Get the number of tasks in a session by checking the tasks.bin file size.
    fn get_task_count(&self, session_id: &str) -> Result<u64, FlameError> {
        let path = self.session_path(session_id).join("tasks.bin");

        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.len() / self.record_size as u64),
            Err(_) => Ok(0),
        }
    }

    /// Read common data for a session.
    fn read_common_data(&self, session_id: &str, len: u64) -> Result<Option<Bytes>, FlameError> {
        if len == 0 {
            return Ok(None);
        }

        let path = self.session_path(session_id).join("common_data.bin");
        match fs::read(&path) {
            Ok(data) => Ok(Some(Bytes::from(data))),
            Err(_) => Ok(None),
        }
    }

    /// Write common data for a session.
    fn write_common_data(&self, session_id: &str, data: &[u8]) -> Result<(), FlameError> {
        let path = self.session_path(session_id).join("common_data.bin");
        fs::write(&path, data)
            .map_err(|e| FlameError::Storage(format!("Failed to write common data: {e}")))?;
        Ok(())
    }

    /// Convert TaskMetadata to Task.
    fn task_from_metadata(
        &self,
        session_id: &str,
        meta: &TaskMetadata,
    ) -> Result<Task, FlameError> {
        let input = if meta.input_len > 0 {
            let data =
                self.read_data(session_id, "inputs.bin", meta.input_offset, meta.input_len)?;
            Some(Bytes::from(data))
        } else {
            None
        };

        let output = if meta.output_len > 0 {
            let data = self.read_data(
                session_id,
                "outputs.bin",
                meta.output_offset,
                meta.output_len,
            )?;
            Some(Bytes::from(data))
        } else {
            None
        };

        let state = TaskState::try_from(meta.state as i32)?;
        let completion_time = if meta.completion_time > 0 {
            DateTime::from_timestamp(meta.completion_time, 0)
        } else {
            None
        };

        Ok(Task {
            id: meta.id as TaskID,
            ssn_id: session_id.to_string(),
            workspace: common::apis::WORKSPACE_DEFAULT.to_string(),
            version: meta.version,
            input,
            output,
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
        let common_data = self.read_common_data(&meta.id, meta.common_data_len)?;
        let completion_time = meta
            .completion_time
            .and_then(|t| DateTime::from_timestamp(t, 0));

        Ok(Session {
            id: meta.id.clone(),
            workspace: meta.workspace.clone(),
            application: meta.application.clone(),
            slots: meta.slots,
            version: meta.version,
            common_data,
            tasks: std::collections::HashMap::new(),
            tasks_index: std::collections::HashMap::new(),
            creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                .ok_or_else(|| FlameError::Storage("Invalid creation time".to_string()))?,
            completion_time,
            events: Vec::new(),
            status: SessionStatus { state },
            min_instances: meta.min_instances,
            max_instances: meta.max_instances,
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
        })
    }

    /// Check if an application exists and is enabled.
    fn check_application_enabled(&self, app_name: &str) -> Result<(), FlameError> {
        let meta = self.read_application_metadata(app_name)?;
        if meta.state != ApplicationState::Enabled as i32 {
            return Err(FlameError::InvalidState(format!(
                "Application {app_name} is not enabled"
            )));
        }
        Ok(())
    }

    fn _update_task_state(
        &self,
        ssn_id: &SessionID,
        task_id: &TaskID,
        task_state: TaskState,
    ) -> Result<Task, FlameError> {
        let mut meta = self.read_task_metadata(ssn_id, *task_id)?;

        meta.state = task_state as u8;
        meta.version += 1;

        if task_state.is_terminal() {
            meta.completion_time = Utc::now().timestamp();
        }

        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(ssn_id, &meta)?;
        self.task_from_metadata(ssn_id, &meta)
    }
}

#[async_trait]
impl Engine for FilesystemEngine {
    async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        if self.read_application_metadata(&name).is_ok() {
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
            name: name.clone(),
            workspace: common::apis::WORKSPACE_DEFAULT.to_string(),
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
        };

        self.write_application_metadata(&name, &meta)?;
        Self::application_from_metadata(&meta)
    }

    async fn unregister_application(&self, name: String) -> Result<(), FlameError> {
        let _guard = lock_app!(self)?;

        let sessions_dir = self.base_path.join("sessions");
        if let Ok(entries) = fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                let session_id = entry.file_name().to_string_lossy().to_string();
                if let Ok(meta) = self.read_session_metadata(&session_id) {
                    if meta.application == name && meta.state == SessionState::Open as i32 {
                        return Err(FlameError::Storage(format!(
                            "Cannot unregister application '{}': has open sessions",
                            name
                        )));
                    }
                }
            }
        }

        let app_dir = self.application_path(&name);
        fs::remove_dir_all(&app_dir).map_err(|e| {
            FlameError::Storage(format!("Failed to delete application '{}': {e}", name))
        })?;

        Ok(())
    }

    async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        let _guard = lock_app!(self)?;

        let mut meta = self.read_application_metadata(&name)?;

        let sessions_dir = self.base_path.join("sessions");
        if let Ok(entries) = fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                let session_id = entry.file_name().to_string_lossy().to_string();
                if let Ok(ssn_meta) = self.read_session_metadata(&session_id) {
                    if ssn_meta.application == name && ssn_meta.state == SessionState::Open as i32 {
                        return Err(FlameError::Storage(format!(
                            "Cannot update application '{}': has open sessions",
                            name
                        )));
                    }
                }
            }
        }

        let schema = attr.schema.map(|s| ApplicationSchemaMetadata {
            input: s.input,
            output: s.output,
            common_data: s.common_data,
        });

        meta.version += 1;
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

        self.write_application_metadata(&name, &meta)?;
        Self::application_from_metadata(&meta)
    }

    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        let meta = self.read_application_metadata(&id)?;
        Self::application_from_metadata(&meta)
    }

    async fn find_application(&self) -> Result<Vec<Application>, FlameError> {
        let mut apps = Vec::new();
        let apps_dir = self.base_path.join("applications");

        if let Ok(entries) = fs::read_dir(&apps_dir) {
            for entry in entries.flatten() {
                let app_name = entry.file_name().to_string_lossy().to_string();
                if let Ok(meta) = self.read_application_metadata(&app_name) {
                    if let Ok(app) = Self::application_from_metadata(&meta) {
                        apps.push(app);
                    }
                }
            }
        }

        Ok(apps)
    }

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        self.check_application_enabled(&attr.application)?;

        if self.read_session_metadata(&attr.id).is_ok() {
            return Err(FlameError::AlreadyExist(format!(
                "Session '{}' already exists",
                attr.id
            )));
        }

        {
            let mut locks = lock_app!(self)?;
            locks.insert(attr.id.clone(), Arc::new(Mutex::new(())));
        }

        let session_dir = self.session_path(&attr.id);
        fs::create_dir_all(&session_dir)
            .map_err(|e| FlameError::Storage(format!("Failed to create session directory: {e}")))?;

        let common_data_len = if let Some(ref data) = attr.common_data {
            self.write_common_data(&attr.id, data)?;
            data.len() as u64
        } else {
            0
        };

        let meta = SessionMetadata {
            id: attr.id.clone(),
            workspace: common::apis::WORKSPACE_DEFAULT.to_string(),
            application: attr.application.clone(),
            slots: attr.slots,
            version: 1,
            state: SessionState::Open as i32,
            creation_time: Utc::now().timestamp(),
            completion_time: None,
            min_instances: attr.min_instances,
            max_instances: attr.max_instances,
            common_data_len,
        };

        self.write_session_metadata(&attr.id, &meta)?;

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

    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let meta = self.read_session_metadata(&id)?;
        self.session_from_metadata(&meta)
    }

    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        // Try to get existing session
        match self.read_session_metadata(&id) {
            Ok(meta) => {
                // Session exists - validate state
                if meta.state != SessionState::Open as i32 {
                    return Err(FlameError::InvalidState(format!(
                        "Session {id} is not open"
                    )));
                }

                // If spec provided, validate it matches
                if let Some(ref attr) = spec {
                    if meta.application != attr.application {
                        return Err(FlameError::InvalidConfig(format!(
                            "Session {id} spec mismatch: application differs"
                        )));
                    }
                    if meta.slots != attr.slots {
                        return Err(FlameError::InvalidConfig(format!(
                            "Session {id} spec mismatch: slots differs"
                        )));
                    }
                }

                self.session_from_metadata(&meta)
            }
            Err(_) => {
                // Session doesn't exist
                match spec {
                    Some(attr) => self.create_session(attr).await,
                    None => Err(FlameError::NotFound(format!("Session {id} not found"))),
                }
            }
        }
    }

    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        lock_ssn!(self, &id);

        let mut meta = self.read_session_metadata(&id)?;

        let task_count = self.get_task_count(&id)?;
        let mut pending_tasks = Vec::new();

        // First pass: check for running tasks and collect pending tasks
        for task_id in 1..=task_count {
            if let Ok(task_meta) = self.read_task_metadata(&id, task_id as TaskID) {
                let state = match TaskState::try_from(task_meta.state as i32) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "Task {}/{} has corrupted state ({}): {}, treating as incomplete",
                            id,
                            task_id,
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
                    pending_tasks.push(task_id as TaskID);
                }
            }
        }

        // Second pass: cancel pending tasks
        for task_id in pending_tasks {
            self._update_task_state(&id, &task_id, TaskState::Cancelled)?;
        }

        meta.state = SessionState::Closed as i32;
        meta.completion_time = Some(Utc::now().timestamp());
        meta.version += 1;

        self.write_session_metadata(&id, &meta)?;
        self.session_from_metadata(&meta)
    }

    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let meta = self.read_session_metadata(&id)?;

        if meta.state != SessionState::Closed as i32 {
            return Err(FlameError::Storage(
                "Cannot delete open session".to_string(),
            ));
        }

        let task_count = self.get_task_count(&id)?;
        for task_id in 1..=task_count {
            if let Ok(task_meta) = self.read_task_metadata(&id, task_id as TaskID) {
                let state = match TaskState::try_from(task_meta.state as i32) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "Task {}/{} has corrupted state ({}): {}, treating as incomplete",
                            id,
                            task_id,
                            task_meta.state,
                            e
                        );
                        return Err(FlameError::Storage(
                            "Cannot delete session with corrupted task state".to_string(),
                        ));
                    }
                };
                if !state.is_terminal() {
                    return Err(FlameError::Storage(
                        "Cannot delete session with non-terminal tasks".to_string(),
                    ));
                }
            }
        }

        let session = self.session_from_metadata(&meta)?;

        let session_dir = self.session_path(&id);
        fs::remove_dir_all(&session_dir)
            .map_err(|e| FlameError::Storage(format!("Failed to delete session: {e}")))?;

        {
            let mut locks = lock_app!(self)?;
            locks.remove(&id);
        }

        Ok(session)
    }

    async fn find_session(&self) -> Result<Vec<Session>, FlameError> {
        let mut sessions = Vec::new();
        let sessions_dir = self.base_path.join("sessions");

        if let Ok(entries) = fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                let session_id = entry.file_name().to_string_lossy().to_string();
                if let Ok(meta) = self.read_session_metadata(&session_id) {
                    if let Ok(session) = self.session_from_metadata(&meta) {
                        {
                            let mut locks = lock_app!(self)?;
                            locks.insert(session_id.clone(), Arc::new(Mutex::new(())));
                        }
                        sessions.push(session);
                    }
                }
            }
        }

        Ok(sessions)
    }

    async fn create_task(
        &self,
        ssn_id: SessionID,
        input: Option<TaskInput>,
    ) -> Result<Task, FlameError> {
        let ssn_meta = self.read_session_metadata(&ssn_id)?;
        if ssn_meta.state != SessionState::Open as i32 {
            return Err(FlameError::InvalidState(
                "Cannot create task in closed session".to_string(),
            ));
        }

        lock_ssn!(self, &ssn_id);

        let task_count = self.get_task_count(&ssn_id)?;
        let task_id = task_count + 1;

        let (input_offset, input_len) = if let Some(ref data) = input {
            let offset = self.append_data(&ssn_id, "inputs.bin", data)?;
            (offset, data.len() as u64)
        } else {
            (0, 0)
        };

        let mut meta = TaskMetadata {
            id: task_id,
            version: 1,
            checksum: 0,
            state: TaskState::Pending as u8,
            creation_time: Utc::now().timestamp(),
            completion_time: 0,
            input_offset,
            input_len,
            output_offset: 0,
            output_len: 0,
        };

        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(&ssn_id, &meta)?;

        self.task_from_metadata(&ssn_id, &meta)
    }

    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        lock_ssn!(self, &gid.ssn_id);
        let meta = self.read_task_metadata(&gid.ssn_id, gid.task_id)?;
        self.task_from_metadata(&gid.ssn_id, &meta)
    }

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        lock_ssn!(self, &gid.ssn_id);

        let mut meta = self.read_task_metadata(&gid.ssn_id, gid.task_id)?;

        meta.state = TaskState::Pending as u8;
        meta.version += 1;
        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(&gid.ssn_id, &meta)?;
        self.task_from_metadata(&gid.ssn_id, &meta)
    }

    async fn delete_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        // In append-only filesystem architecture, physical deletion is not supported.
        // The task data remains in the append-only files (inputs.bin, outputs.bin).
        // Callers should use close_session + delete_session to clean up entire sessions.
        Err(FlameError::Storage(format!(
            "Task deletion not supported in filesystem storage engine (task {}/{}). \
             Use session deletion to clean up completed sessions.",
            gid.ssn_id, gid.task_id
        )))
    }

    async fn update_task_state(
        &self,
        gid: TaskGID,
        task_state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        lock_ssn!(self, &gid.ssn_id);

        self._update_task_state(&gid.ssn_id, &gid.task_id, task_state)
    }

    async fn update_task_result(
        &self,
        gid: TaskGID,
        task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        lock_ssn!(self, &gid.ssn_id);

        let mut meta = self.read_task_metadata(&gid.ssn_id, gid.task_id)?;

        if let Some(ref output) = task_result.output {
            let offset = self.append_data(&gid.ssn_id, "outputs.bin", output)?;
            meta.output_offset = offset;
            meta.output_len = output.len() as u64;
        }

        meta.state = task_result.state as u8;
        meta.version += 1;

        if task_result.state.is_terminal() {
            meta.completion_time = Utc::now().timestamp();
        }

        meta.checksum = calculate_checksum(&meta);

        self.write_task_metadata(&gid.ssn_id, &meta)?;
        self.task_from_metadata(&gid.ssn_id, &meta)
    }

    async fn find_tasks(&self, ssn_id: SessionID) -> Result<Vec<Task>, FlameError> {
        lock_ssn!(self, &ssn_id);

        let mut tasks = Vec::new();
        let task_count = self.get_task_count(&ssn_id)?;

        for task_id in 1..=task_count {
            if let Ok(meta) = self.read_task_metadata(&ssn_id, task_id as TaskID) {
                if let Ok(task) = self.task_from_metadata(&ssn_id, &meta) {
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
            name: node.name.clone(),
            state: i32::from(node.state),
            capacity_cpu: node.capacity.cpu,
            capacity_memory: node.capacity.memory,
            allocatable_cpu: node.allocatable.cpu,
            allocatable_memory: node.allocatable.memory,
            info_arch: node.info.arch.clone(),
            info_os: node.info.os.clone(),
            creation_time: now,
            last_heartbeat: now,
        };

        self.write_node_metadata(&node.name, &meta)?;
        Ok(node.clone())
    }

    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        lock_node!(self, name);

        match self.read_node_metadata(name) {
            Ok(meta) => Ok(Some(Node {
                name: meta.name,
                state: NodeState::from(meta.state),
                capacity: ResourceRequirement {
                    cpu: meta.capacity_cpu,
                    memory: meta.capacity_memory,
                },
                allocatable: ResourceRequirement {
                    cpu: meta.allocatable_cpu,
                    memory: meta.allocatable_memory,
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
            name: node.name.clone(),
            state: i32::from(node.state),
            capacity_cpu: node.capacity.cpu,
            capacity_memory: node.capacity.memory,
            allocatable_cpu: node.allocatable.cpu,
            allocatable_memory: node.allocatable.memory,
            info_arch: node.info.arch.clone(),
            info_os: node.info.os.clone(),
            creation_time,
            last_heartbeat: Utc::now().timestamp(),
        };

        self.write_node_metadata(&node.name, &meta)?;
        Ok(node.clone())
    }

    async fn delete_node(&self, name: &str) -> Result<(), FlameError> {
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
                        name: meta.name,
                        state: NodeState::from(meta.state),
                        capacity: ResourceRequirement {
                            cpu: meta.capacity_cpu,
                            memory: meta.capacity_memory,
                        },
                        allocatable: ResourceRequirement {
                            cpu: meta.allocatable_cpu,
                            memory: meta.allocatable_memory,
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
        lock_executor!(self, &executor.id);

        let meta = ExecutorMetadata {
            id: executor.id.clone(),
            node: executor.node.clone(),
            resreq_cpu: executor.resreq.cpu,
            resreq_memory: executor.resreq.memory,
            slots: executor.slots,
            shim: i32::from(executor.shim),
            task_id: executor.task_id,
            ssn_id: executor.ssn_id.clone(),
            creation_time: executor.creation_time.timestamp(),
            state: i32::from(executor.state),
        };

        self.write_executor_metadata(&executor.node, &executor.id, &meta)?;
        Ok(executor.clone())
    }

    async fn get_executor(&self, id: &ExecutorID) -> Result<Option<Executor>, FlameError> {
        lock_executor!(self, id);

        let node_name = match self.find_executor_node(id) {
            Ok(name) => name,
            Err(FlameError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };

        match self.read_executor_metadata(&node_name, id) {
            Ok(meta) => Ok(Some(Executor {
                id: meta.id,
                node: meta.node,
                resreq: ResourceRequirement {
                    cpu: meta.resreq_cpu,
                    memory: meta.resreq_memory,
                },
                slots: meta.slots,
                shim: Shim::try_from(meta.shim).unwrap_or_default(),
                task_id: meta.task_id.map(|t| t as TaskID),
                ssn_id: meta.ssn_id,
                creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
                state: ExecutorState::from(meta.state),
            })),
            Err(FlameError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        lock_executor!(self, &executor.id);

        let meta = ExecutorMetadata {
            id: executor.id.clone(),
            node: executor.node.clone(),
            resreq_cpu: executor.resreq.cpu,
            resreq_memory: executor.resreq.memory,
            slots: executor.slots,
            shim: i32::from(executor.shim),
            task_id: executor.task_id,
            ssn_id: executor.ssn_id.clone(),
            creation_time: executor.creation_time.timestamp(),
            state: i32::from(executor.state),
        };

        self.write_executor_metadata(&executor.node, &executor.id, &meta)?;
        Ok(executor.clone())
    }

    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        lock_executor!(self, id);

        let node_name = self.find_executor_node(id)?;
        let mut meta = self.read_executor_metadata(&node_name, id)?;
        meta.state = i32::from(state);
        self.write_executor_metadata(&node_name, id, &meta)?;

        Ok(Executor {
            id: meta.id,
            node: meta.node,
            resreq: ResourceRequirement {
                cpu: meta.resreq_cpu,
                memory: meta.resreq_memory,
            },
            slots: meta.slots,
            shim: Shim::try_from(meta.shim).unwrap_or_default(),
            task_id: meta.task_id.map(|t| t as TaskID),
            ssn_id: meta.ssn_id,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
            state,
        })
    }

    async fn delete_executor(&self, id: &ExecutorID) -> Result<(), FlameError> {
        lock_executor!(self, id);

        let node_name = match self.find_executor_node(id) {
            Ok(name) => name,
            Err(FlameError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };

        let exec_dir = self.executor_path(&node_name, id);
        if exec_dir.exists() {
            fs::remove_dir_all(&exec_dir)
                .map_err(|e| FlameError::Storage(format!("Failed to delete executor {id}: {e}")))?;
        }

        {
            let mut locks = self
                .executor_locks
                .write()
                .map_err(|e| FlameError::Storage(format!("Executor lock poisoned: {}", e)))?;
            locks.remove(id);
        }

        Ok(())
    }

    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        let mut executors = Vec::new();
        let nodes_dir = self.base_path.join("nodes");

        let node_names: Vec<String> = match node {
            Some(n) => vec![n.to_string()],
            None => {
                let mut names = Vec::new();
                if let Ok(entries) = fs::read_dir(&nodes_dir) {
                    for entry in entries.flatten() {
                        names.push(entry.file_name().to_string_lossy().to_string());
                    }
                }
                names
            }
        };

        for node_name in node_names {
            let executors_dir = self.node_path(&node_name).join("executors");
            if let Ok(entries) = fs::read_dir(&executors_dir) {
                for entry in entries.flatten() {
                    let executor_id = entry.file_name().to_string_lossy().to_string();
                    if let Ok(meta) = self.read_executor_metadata(&node_name, &executor_id) {
                        executors.push(Executor {
                            id: meta.id,
                            node: meta.node,
                            resreq: ResourceRequirement {
                                cpu: meta.resreq_cpu,
                                memory: meta.resreq_memory,
                            },
                            slots: meta.slots,
                            shim: Shim::try_from(meta.shim).unwrap_or_default(),
                            task_id: meta.task_id.map(|t| t as TaskID),
                            ssn_id: meta.ssn_id,
                            creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                                .unwrap_or_default(),
                            state: ExecutorState::from(meta.state),
                        });
                    }
                }
            }
        }

        Ok(executors)
    }

    async fn get_user(&self, name: &str) -> Result<Option<User>, FlameError> {
        let user_path = self.base_path.join("users").join(name).join("metadata");
        if !user_path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&user_path)
            .map_err(|e| FlameError::Storage(format!("failed to read user: {e}")))?;
        let meta: UserMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse user: {e}")))?;

        Ok(Some(User {
            name: meta.name,
            display_name: meta.display_name,
            email: meta.email,
            certificate_cn: meta.certificate_cn,
            enabled: meta.enabled,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
            last_login_time: meta
                .last_login_time
                .and_then(|t| DateTime::from_timestamp(t, 0)),
            roles: meta.roles,
        }))
    }

    async fn get_user_by_cn(&self, cn: &str) -> Result<Option<User>, FlameError> {
        let users_dir = self.base_path.join("users");
        if let Ok(entries) = fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("metadata");
                if path.exists() {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(meta) = serde_json::from_str::<UserMetadata>(&content) {
                            if meta.certificate_cn == cn {
                                return Ok(Some(User {
                                    name: meta.name,
                                    display_name: meta.display_name,
                                    email: meta.email,
                                    certificate_cn: meta.certificate_cn,
                                    enabled: meta.enabled,
                                    creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                                        .unwrap_or_default(),
                                    last_login_time: meta
                                        .last_login_time
                                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                                    roles: meta.roles,
                                }));
                            }
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    async fn get_user_roles(&self, user_name: &str) -> Result<Vec<Role>, FlameError> {
        let user_path = self
            .base_path
            .join("users")
            .join(user_name)
            .join("metadata");
        if !user_path.exists() {
            return Err(FlameError::NotFound(format!("user not found: {user_name}")));
        }

        let content = fs::read_to_string(&user_path)
            .map_err(|e| FlameError::Storage(format!("failed to read user: {e}")))?;
        let user_meta: UserMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse user: {e}")))?;

        let mut roles = Vec::new();
        for role_name in &user_meta.roles {
            if let Some(role) = self.get_role(role_name).await? {
                roles.push(role);
            }
        }
        Ok(roles)
    }

    async fn create_user(&self, user: &User) -> Result<User, FlameError> {
        let user_dir = self.base_path.join("users").join(&user.name);
        fs::create_dir_all(&user_dir)
            .map_err(|e| FlameError::Storage(format!("failed to create user directory: {e}")))?;

        let meta = UserMetadata {
            name: user.name.clone(),
            display_name: user.display_name.clone(),
            email: user.email.clone(),
            certificate_cn: user.certificate_cn.clone(),
            enabled: user.enabled,
            creation_time: Utc::now().timestamp(),
            last_login_time: None,
            roles: user.roles.clone(),
        };

        let path = user_dir.join("metadata");
        let content = serde_json::to_string_pretty(&meta)
            .map_err(|e| FlameError::Storage(format!("failed to serialize user: {e}")))?;
        fs::write(&path, content)
            .map_err(|e| FlameError::Storage(format!("failed to write user: {e}")))?;

        Ok(User {
            name: meta.name,
            display_name: meta.display_name,
            email: meta.email,
            certificate_cn: meta.certificate_cn,
            enabled: meta.enabled,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
            last_login_time: None,
            roles: meta.roles,
        })
    }

    async fn update_user(
        &self,
        user: &User,
        assign_roles: &[String],
        revoke_roles: &[String],
    ) -> Result<User, FlameError> {
        let user_path = self
            .base_path
            .join("users")
            .join(&user.name)
            .join("metadata");
        if !user_path.exists() {
            return Err(FlameError::NotFound(format!(
                "user not found: {}",
                user.name
            )));
        }

        let content = fs::read_to_string(&user_path)
            .map_err(|e| FlameError::Storage(format!("failed to read user: {e}")))?;
        let mut meta: UserMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse user: {e}")))?;

        meta.display_name = user.display_name.clone();
        meta.email = user.email.clone();
        meta.enabled = user.enabled;

        for role in revoke_roles {
            meta.roles.retain(|r| r != role);
        }
        for role in assign_roles {
            if !meta.roles.contains(role) {
                meta.roles.push(role.clone());
            }
        }

        let content = serde_json::to_string_pretty(&meta)
            .map_err(|e| FlameError::Storage(format!("failed to serialize user: {e}")))?;
        fs::write(&user_path, content)
            .map_err(|e| FlameError::Storage(format!("failed to write user: {e}")))?;

        Ok(User {
            name: meta.name,
            display_name: meta.display_name,
            email: meta.email,
            certificate_cn: meta.certificate_cn,
            enabled: meta.enabled,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
            last_login_time: meta
                .last_login_time
                .and_then(|t| DateTime::from_timestamp(t, 0)),
            roles: meta.roles,
        })
    }

    async fn delete_user(&self, name: &str) -> Result<(), FlameError> {
        let user_dir = self.base_path.join("users").join(name);
        if user_dir.exists() {
            fs::remove_dir_all(&user_dir)
                .map_err(|e| FlameError::Storage(format!("failed to delete user: {e}")))?;
        }
        Ok(())
    }

    async fn find_users(&self, role_filter: Option<&str>) -> Result<Vec<User>, FlameError> {
        let mut users = Vec::new();
        let users_dir = self.base_path.join("users");

        if let Ok(entries) = fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("metadata");
                if path.exists() {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(meta) = serde_json::from_str::<UserMetadata>(&content) {
                            let include = match role_filter {
                                Some(role) => meta.roles.contains(&role.to_string()),
                                None => true,
                            };
                            if include {
                                users.push(User {
                                    name: meta.name,
                                    display_name: meta.display_name,
                                    email: meta.email,
                                    certificate_cn: meta.certificate_cn,
                                    enabled: meta.enabled,
                                    creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                                        .unwrap_or_default(),
                                    last_login_time: meta
                                        .last_login_time
                                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                                    roles: meta.roles,
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(users)
    }

    async fn get_role(&self, name: &str) -> Result<Option<Role>, FlameError> {
        let path = self.base_path.join("roles").join(name).join("metadata");
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)
            .map_err(|e| FlameError::Storage(format!("failed to read role: {e}")))?;
        let meta: RoleMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse role: {e}")))?;

        Ok(Some(Role {
            name: meta.name,
            description: meta.description,
            permissions: meta.permissions,
            workspaces: meta.workspaces,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
        }))
    }

    async fn create_role(&self, role: &Role) -> Result<Role, FlameError> {
        let role_dir = self.base_path.join("roles").join(&role.name);
        fs::create_dir_all(&role_dir)
            .map_err(|e| FlameError::Storage(format!("failed to create role directory: {e}")))?;

        let meta = RoleMetadata {
            name: role.name.clone(),
            description: role.description.clone(),
            permissions: role.permissions.clone(),
            workspaces: role.workspaces.clone(),
            creation_time: Utc::now().timestamp(),
        };

        let path = role_dir.join("metadata");
        let content = serde_json::to_string_pretty(&meta)
            .map_err(|e| FlameError::Storage(format!("failed to serialize role: {e}")))?;
        fs::write(&path, content)
            .map_err(|e| FlameError::Storage(format!("failed to write role: {e}")))?;

        Ok(Role {
            name: meta.name,
            description: meta.description,
            permissions: meta.permissions,
            workspaces: meta.workspaces,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
        })
    }

    async fn update_role(&self, role: &Role) -> Result<Role, FlameError> {
        let role_path = self
            .base_path
            .join("roles")
            .join(&role.name)
            .join("metadata");
        if !role_path.exists() {
            return Err(FlameError::NotFound(format!(
                "role not found: {}",
                role.name
            )));
        }

        let content = fs::read_to_string(&role_path)
            .map_err(|e| FlameError::Storage(format!("failed to read role: {e}")))?;
        let mut meta: RoleMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse role: {e}")))?;

        meta.description = role.description.clone();
        meta.permissions = role.permissions.clone();
        meta.workspaces = role.workspaces.clone();

        let content = serde_json::to_string_pretty(&meta)
            .map_err(|e| FlameError::Storage(format!("failed to serialize role: {e}")))?;
        fs::write(&role_path, content)
            .map_err(|e| FlameError::Storage(format!("failed to write role: {e}")))?;

        Ok(Role {
            name: meta.name,
            description: meta.description,
            permissions: meta.permissions,
            workspaces: meta.workspaces,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
        })
    }

    async fn delete_role(&self, name: &str) -> Result<(), FlameError> {
        let role_dir = self.base_path.join("roles").join(name);
        if role_dir.exists() {
            fs::remove_dir_all(&role_dir)
                .map_err(|e| FlameError::Storage(format!("failed to delete role: {e}")))?;
        }
        Ok(())
    }

    async fn find_roles(&self, workspace_filter: Option<&str>) -> Result<Vec<Role>, FlameError> {
        let mut roles = Vec::new();
        let roles_dir = self.base_path.join("roles");

        if let Ok(entries) = fs::read_dir(&roles_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("metadata");
                if path.exists() {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(meta) = serde_json::from_str::<RoleMetadata>(&content) {
                            let include = match workspace_filter {
                                Some(ws) => meta.workspaces.contains(&ws.to_string()),
                                None => true,
                            };
                            if include {
                                roles.push(Role {
                                    name: meta.name,
                                    description: meta.description,
                                    permissions: meta.permissions,
                                    workspaces: meta.workspaces,
                                    creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                                        .unwrap_or_default(),
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(roles)
    }

    async fn get_workspace(&self, name: &str) -> Result<Option<Workspace>, FlameError> {
        let path = self
            .base_path
            .join("workspaces")
            .join(name)
            .join("metadata");
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)
            .map_err(|e| FlameError::Storage(format!("failed to read workspace: {e}")))?;
        let meta: WorkspaceMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse workspace: {e}")))?;

        Ok(Some(Workspace {
            name: meta.name,
            description: meta.description,
            labels: meta.labels,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
        }))
    }

    async fn create_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError> {
        let ws_dir = self.base_path.join("workspaces").join(&workspace.name);
        fs::create_dir_all(&ws_dir).map_err(|e| {
            FlameError::Storage(format!("failed to create workspace directory: {e}"))
        })?;

        let meta = WorkspaceMetadata {
            name: workspace.name.clone(),
            description: workspace.description.clone(),
            labels: workspace.labels.clone(),
            creation_time: Utc::now().timestamp(),
        };

        let path = ws_dir.join("metadata");
        let content = serde_json::to_string_pretty(&meta)
            .map_err(|e| FlameError::Storage(format!("failed to serialize workspace: {e}")))?;
        fs::write(&path, content)
            .map_err(|e| FlameError::Storage(format!("failed to write workspace: {e}")))?;

        Ok(Workspace {
            name: meta.name,
            description: meta.description,
            labels: meta.labels,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
        })
    }

    async fn update_workspace(&self, workspace: &Workspace) -> Result<Workspace, FlameError> {
        let ws_path = self
            .base_path
            .join("workspaces")
            .join(&workspace.name)
            .join("metadata");
        if !ws_path.exists() {
            return Err(FlameError::NotFound(format!(
                "workspace not found: {}",
                workspace.name
            )));
        }

        let content = fs::read_to_string(&ws_path)
            .map_err(|e| FlameError::Storage(format!("failed to read workspace: {e}")))?;
        let mut meta: WorkspaceMetadata = serde_json::from_str(&content)
            .map_err(|e| FlameError::Storage(format!("failed to parse workspace: {e}")))?;

        meta.description = workspace.description.clone();
        meta.labels = workspace.labels.clone();

        let content = serde_json::to_string_pretty(&meta)
            .map_err(|e| FlameError::Storage(format!("failed to serialize workspace: {e}")))?;
        fs::write(&ws_path, content)
            .map_err(|e| FlameError::Storage(format!("failed to write workspace: {e}")))?;

        Ok(Workspace {
            name: meta.name,
            description: meta.description,
            labels: meta.labels,
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
        })
    }

    async fn delete_workspace(&self, name: &str) -> Result<(), FlameError> {
        let ws_dir = self.base_path.join("workspaces").join(name);
        if ws_dir.exists() {
            fs::remove_dir_all(&ws_dir)
                .map_err(|e| FlameError::Storage(format!("failed to delete workspace: {e}")))?;
        }
        Ok(())
    }

    async fn find_workspaces(&self) -> Result<Vec<Workspace>, FlameError> {
        let mut workspaces = Vec::new();
        let ws_dir = self.base_path.join("workspaces");

        if let Ok(entries) = fs::read_dir(&ws_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("metadata");
                if path.exists() {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(meta) = serde_json::from_str::<WorkspaceMetadata>(&content) {
                            workspaces.push(Workspace {
                                name: meta.name,
                                description: meta.description,
                                labels: meta.labels,
                                creation_time: DateTime::from_timestamp(meta.creation_time, 0)
                                    .unwrap_or_default(),
                            });
                        }
                    }
                }
            }
        }
        Ok(workspaces)
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
    async fn test_record_size_is_constant() {
        let size1 = task_record_size();
        let size2 = task_record_size();
        assert_eq!(size1, size2);

        // Verify different metadata values produce same size
        let meta1 = TaskMetadata::default();
        let meta2 = TaskMetadata {
            id: u64::MAX,
            version: u32::MAX,
            checksum: u32::MAX,
            state: 255,
            creation_time: i64::MAX,
            completion_time: i64::MAX,
            input_offset: u64::MAX,
            input_len: u64::MAX,
            output_offset: u64::MAX,
            output_len: u64::MAX,
        };

        let buf1 = bincode::encode_to_vec(&meta1, bincode_config()).unwrap();
        let buf2 = bincode::encode_to_vec(&meta2, bincode_config()).unwrap();

        assert_eq!(buf1.len(), buf2.len());
    }

    #[tokio::test]
    async fn test_checksum_calculation() {
        let meta = TaskMetadata {
            id: 1,
            version: 1,
            checksum: 0,
            state: TaskState::Pending as u8,
            creation_time: 1234567890,
            completion_time: 0,
            input_offset: 0,
            input_len: 100,
            output_offset: 0,
            output_len: 0,
        };

        let checksum1 = calculate_checksum(&meta);
        let checksum2 = calculate_checksum(&meta);

        assert_eq!(checksum1, checksum2);

        // Different metadata should produce different checksum
        let meta2 = TaskMetadata { id: 2, ..meta };

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
        };

        let app = engine
            .register_application("test-app".to_string(), attr.clone())
            .await
            .unwrap();
        assert_eq!(app.name, "test-app");
        assert_eq!(app.state, ApplicationState::Enabled);

        // Get application
        let app2 = engine
            .get_application("test-app".to_string())
            .await
            .unwrap();
        assert_eq!(app2.name, "test-app");

        // Find applications
        let apps = engine.find_application().await.unwrap();
        assert_eq!(apps.len(), 1);

        // Update application
        let updated_attr = ApplicationAttributes {
            description: Some("Updated description".to_string()),
            ..attr
        };
        let app3 = engine
            .update_application("test-app".to_string(), updated_attr)
            .await
            .unwrap();
        assert_eq!(app3.description, Some("Updated description".to_string()));
        assert_eq!(app3.version, 2);

        // Unregister application
        engine
            .unregister_application("test-app".to_string())
            .await
            .unwrap();

        // Verify it's gone
        let result = engine.get_application("test-app".to_string()).await;
        assert!(result.is_err());
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
        };
        engine
            .register_application("test-app".to_string(), app_attr)
            .await
            .unwrap();

        // Create session
        let ssn_attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: Some(Bytes::from("test data")),
            min_instances: 0,
            max_instances: None,
        };

        let session = engine.create_session(ssn_attr).await.unwrap();
        assert_eq!(session.id, "test-session");
        assert_eq!(session.status.state, SessionState::Open);

        // Get session
        let session2 = engine
            .get_session("test-session".to_string())
            .await
            .unwrap();
        assert_eq!(session2.id, "test-session");

        // Find sessions
        let sessions = engine.find_session().await.unwrap();
        assert_eq!(sessions.len(), 1);

        // Close session (should work since no tasks)
        let closed = engine
            .close_session("test-session".to_string())
            .await
            .unwrap();
        assert_eq!(closed.status.state, SessionState::Closed);

        // Delete session
        let deleted = engine
            .delete_session("test-session".to_string())
            .await
            .unwrap();
        assert_eq!(deleted.id, "test-session");
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
        };
        engine
            .register_application("test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        };
        engine.create_session(ssn_attr).await.unwrap();

        // Create task with input
        let input = Bytes::from("test input data");
        let task = engine
            .create_task("test-session".to_string(), Some(input.clone()))
            .await
            .unwrap();
        assert_eq!(task.id, 1);
        assert_eq!(task.state, TaskState::Pending);
        assert_eq!(task.input, Some(input));

        // Get task
        let gid = TaskGID {
            ssn_id: "test-session".to_string(),
            task_id: 1,
        };
        let task2 = engine.get_task(gid.clone()).await.unwrap();
        assert_eq!(task2.id, 1);

        // Update task state
        let task3 = engine
            .update_task_state(gid.clone(), TaskState::Running, None)
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
            .update_task_result(gid.clone(), result)
            .await
            .unwrap();
        assert_eq!(task4.state, TaskState::Succeed);
        assert_eq!(task4.output, Some(output));

        // Find tasks
        let tasks = engine.find_tasks("test-session".to_string()).await.unwrap();
        assert_eq!(tasks.len(), 1);

        // Create another task
        let task5 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task5.id, 2);

        // Complete second task
        let gid2 = TaskGID {
            ssn_id: "test-session".to_string(),
            task_id: 2,
        };
        engine
            .update_task_state(gid2, TaskState::Succeed, None)
            .await
            .unwrap();

        // Now we can close the session
        let closed = engine
            .close_session("test-session".to_string())
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
        };

        engine
            .register_application("test-app".to_string(), attr.clone())
            .await
            .unwrap();

        let result = engine
            .register_application("test-app".to_string(), attr)
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
        };
        engine
            .register_application("test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
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
        };
        engine
            .register_application("test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        };
        engine.create_session(ssn_attr).await.unwrap();

        let task1 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task1.state, TaskState::Pending);

        let task2 = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();
        assert_eq!(task2.state, TaskState::Pending);

        let closed = engine
            .close_session("test-session".to_string())
            .await
            .unwrap();
        assert_eq!(closed.status.state, SessionState::Closed);

        let task1_after = engine.get_task(task1.gid()).await.unwrap();
        assert_eq!(task1_after.state, TaskState::Cancelled);

        let task2_after = engine.get_task(task2.gid()).await.unwrap();
        assert_eq!(task2_after.state, TaskState::Cancelled);
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
        };
        engine
            .register_application("test-app".to_string(), app_attr)
            .await
            .unwrap();

        let ssn_attr = SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        };
        engine.create_session(ssn_attr).await.unwrap();

        let task = engine
            .create_task("test-session".to_string(), None)
            .await
            .unwrap();

        engine
            .update_task_state(task.gid(), TaskState::Running, None)
            .await
            .unwrap();

        let result = engine.close_session("test-session".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_node_crud() {
        let (engine, _temp_dir) = create_test_engine().await;

        let node = Node {
            name: "test-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 8,
                memory: 16384,
            },
            allocatable: ResourceRequirement {
                cpu: 6,
                memory: 12288,
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

        let node = Node {
            name: "exec-test-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 4,
                memory: 8192,
            },
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };
        engine.create_node(&node).await.unwrap();

        let executor = crate::model::Executor {
            id: "exec-1".to_string(),
            node: "exec-test-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
            },
            slots: 1,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Void,
        };

        let created = engine.create_executor(&executor).await.unwrap();
        assert_eq!(created.id, "exec-1");

        let found = engine.get_executor(&"exec-1".to_string()).await.unwrap();
        assert!(found.is_some());

        let updated = engine
            .update_executor_state(&"exec-1".to_string(), ExecutorState::Idle)
            .await
            .unwrap();
        assert_eq!(updated.state, ExecutorState::Idle);

        let executors = engine.find_executors(None).await.unwrap();
        assert_eq!(executors.len(), 1);

        let by_node = engine.find_executors(Some("exec-test-node")).await.unwrap();
        assert_eq!(by_node.len(), 1);

        let by_other = engine.find_executors(Some("other-node")).await.unwrap();
        assert_eq!(by_other.len(), 0);

        engine.delete_executor(&"exec-1".to_string()).await.unwrap();
        let deleted = engine.get_executor(&"exec-1".to_string()).await.unwrap();
        assert!(deleted.is_none());
    }

    #[tokio::test]
    async fn test_node_delete_cascades_to_executors() {
        let (engine, _temp_dir) = create_test_engine().await;

        let node = Node {
            name: "cascade-node".to_string(),
            state: NodeState::Ready,
            capacity: ResourceRequirement {
                cpu: 4,
                memory: 8192,
            },
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
            },
            info: NodeInfo {
                arch: "x86_64".to_string(),
                os: "linux".to_string(),
            },
        };
        engine.create_node(&node).await.unwrap();

        for i in 1..=3 {
            let executor = crate::model::Executor {
                id: format!("cascade-exec-{i}"),
                node: "cascade-node".to_string(),
                resreq: ResourceRequirement {
                    cpu: 1,
                    memory: 1024,
                },
                slots: 1,
                shim: Shim::Host,
                task_id: None,
                ssn_id: None,
                creation_time: Utc::now(),
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
}

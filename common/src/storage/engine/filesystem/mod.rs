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
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use stdng::{async_rw_ptr, ensure, read_async, write_async, AsyncRWPtr};
use tokio::sync::OwnedRwLockWriteGuard;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};

use crate::apis::{
    Application, ApplicationAttributes, ApplicationID, ApplicationSchema, ApplicationState,
    ExecutorID, ExecutorState, Node, NodeInfo, NodeState, ResourceRequirement, Session,
    SessionAttributes, SessionID, SessionState, SessionStatus, Shim, Task, TaskCounts, TaskGID,
    TaskID, TaskInput, TaskOptions, TaskResult, TaskState,
};
use crate::{FlameError, FLAME_HOME};

use crate::apis::{ApplicationFilter, Executor, SessionFilter, TaskFilter};

use crate::storage::engine::{Engine, EnginePtr};

mod payload;
mod types;
use payload::Part;
use types::{
    calculate_checksum, task_record_size, ApplicationEntry, ApplicationMetadata,
    ApplicationSchemaMetadata, ExecutorEntry, ExecutorMetadata, NodeEntry, NodeMetadata,
    SessionEntry, SessionMetadata, TaskMetadata,
};

fn poisoned<T>(e: PoisonError<T>) -> FlameError {
    FlameError::Storage(format!("Storage lock poisoned: {e}"))
}

pub struct FilesystemEngine {
    base_path: PathBuf,
    record_size: usize,
    /// Short lookups only: never held across I/O or an await.
    sessions: RwLock<HashMap<String, AsyncRWPtr<SessionEntry>>>,
    /// Per-application lock; coordinates session creation with application
    /// deletion and update without blocking unrelated sessions.
    applications: RwLock<HashMap<String, AsyncRWPtr<ApplicationEntry>>>,
    nodes: RwLock<HashMap<String, AsyncRWPtr<NodeEntry>>>,
    executors: RwLock<HashMap<String, AsyncRWPtr<ExecutorEntry>>>,
}

/// Shared session entry: close and delete take the write side of this lock.
macro_rules! lock_ssn {
    ($self:expr, $ssn_id:expr, $entry:ident) => {
        let __fs_session = $self.session_entry($ssn_id)?;
        let $entry = read_async!(__fs_session);
        validate_entry(&$self.sessions, $ssn_id, &__fs_session)?;
    };
}

/// Registry guards never span entity waits or filesystem I/O.
/// Look up an existing entry; the registry guard is released before it returns.
fn lookup_entry<T>(
    registry: &RwLock<HashMap<String, AsyncRWPtr<T>>>,
    id: &str,
    kind: &str,
) -> Result<AsyncRWPtr<T>, FlameError> {
    registry
        .read()
        .map_err(poisoned)?
        .get(id)
        .cloned()
        .ok_or_else(|| FlameError::NotFound(format!("{kind} {id} not found")))
}

/// Create a new entry, or fail if one exists. It is published already
/// write-locked, so nobody sees it before its caller persists it; the guard's
/// `rwlock()` gives the entry back for `remove_entry` if that fails.
fn new_entry<T>(
    registry: &RwLock<HashMap<String, AsyncRWPtr<T>>>,
    id: &str,
    kind: &str,
    value: T,
) -> Result<OwnedRwLockWriteGuard<T>, FlameError> {
    let entry = async_rw_ptr(value);
    // A fresh lock is uncontended, so this never waits under the registry guard.
    let guard = entry
        .clone()
        .try_write_owned()
        .map_err(|e| FlameError::Internal(format!("Fresh entry lock was held: {e}")))?;
    let mut entries = registry.write().map_err(poisoned)?;
    if entries.contains_key(id) {
        return Err(FlameError::AlreadyExist(format!(
            "{kind} '{id}' already exists"
        )));
    }
    entries.insert(id.to_string(), entry);
    Ok(guard)
}

/// A waiter must still belong to the registered entry after taking its gate.
fn validate_entry<T>(
    registry: &RwLock<HashMap<String, AsyncRWPtr<T>>>,
    id: &str,
    shared: &AsyncRWPtr<T>,
) -> Result<(), FlameError> {
    if registry
        .read()
        .map_err(poisoned)?
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, shared))
    {
        Ok(())
    } else {
        Err(FlameError::NotFound(format!("Entry was deleted: {id}")))
    }
}

fn remove_entry<T>(
    registry: &RwLock<HashMap<String, AsyncRWPtr<T>>>,
    id: &str,
    shared: &AsyncRWPtr<T>,
) -> Result<(), FlameError> {
    let mut entries = registry.write().map_err(poisoned)?;
    if entries
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, shared))
    {
        entries.remove(id);
        Ok(())
    } else {
        Err(FlameError::NotFound(format!("Entry was deleted: {id}")))
    }
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

        fs::create_dir_all(&sessions_path).map_err(|e| {
            FlameError::Storage(format!("Failed to create sessions directory: {e}"))
        })?;
        fs::create_dir_all(&applications_path).map_err(|e| {
            FlameError::Storage(format!("Failed to create applications directory: {e}"))
        })?;
        fs::create_dir_all(&nodes_path)
            .map_err(|e| FlameError::Storage(format!("Failed to create nodes directory: {e}")))?;

        let record_size = task_record_size();
        tracing::info!(
            "Filesystem storage engine initialized at {:?} with record size {}",
            path,
            record_size
        );

        let engine = FilesystemEngine {
            base_path: path,
            record_size,
            sessions: RwLock::new(HashMap::new()),
            applications: RwLock::new(HashMap::new()),
            nodes: RwLock::new(HashMap::new()),
            executors: RwLock::new(HashMap::new()),
        };
        engine.load_entities()?;
        Ok(Arc::new(engine))
    }

    /// Load applications, nodes and executors once. Entries then live until the
    /// entity is deleted; unreadable records are logged and skipped.
    fn load_entities(&self) -> Result<(), FlameError> {
        for id in Self::child_names(&self.base_path.join("sessions")) {
            match self.read_session_metadata(&id) {
                Ok(metadata) => {
                    let entry = SessionEntry::new(metadata, self.session_path(&id));
                    self.sessions
                        .write()
                        .map_err(poisoned)?
                        .insert(id, async_rw_ptr(entry));
                }
                Err(e) => tracing::warn!("Skipping session <{id}>: {e}"),
            }
        }
        for name in Self::child_names(&self.base_path.join("applications")) {
            match self.read_application_metadata(&name) {
                Ok(metadata) => {
                    self.applications
                        .write()
                        .map_err(poisoned)?
                        .insert(name, async_rw_ptr(ApplicationEntry { metadata }));
                }
                Err(e) => tracing::warn!("Skipping application <{name}>: {e}"),
            }
        }
        for node in Self::child_names(&self.base_path.join("nodes")) {
            match self.read_node_metadata(&node) {
                Ok(metadata) => {
                    self.nodes
                        .write()
                        .map_err(poisoned)?
                        .insert(node.clone(), async_rw_ptr(NodeEntry { metadata }));
                }
                Err(e) => {
                    tracing::warn!("Skipping node <{node}>: {e}");
                    continue;
                }
            }
            for id in Self::child_names(&self.node_path(&node).join("executors")) {
                match self.read_executor_metadata(&node, &id) {
                    Ok(metadata) => {
                        self.executors
                            .write()
                            .map_err(poisoned)?
                            .insert(id, async_rw_ptr(ExecutorEntry { metadata }));
                    }
                    Err(e) => tracing::warn!("Skipping executor <{id}> of node <{node}>: {e}"),
                }
            }
        }
        Ok(())
    }

    fn child_names(dir: &std::path::Path) -> Vec<String> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|file| file.file_name().to_string_lossy().to_string())
            .collect()
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

    fn session_entry(&self, id: &str) -> Result<AsyncRWPtr<SessionEntry>, FlameError> {
        self.sessions
            .read()
            .map_err(|e| FlameError::Storage(format!("Session registry poisoned: {e}")))?
            .get(id)
            .cloned()
            .ok_or_else(|| FlameError::NotFound(format!("Session lock not found: {id}")))
    }

    fn application_entry(&self, name: &str) -> Result<AsyncRWPtr<ApplicationEntry>, FlameError> {
        lookup_entry(&self.applications, name, "Application")
    }

    fn node_entry(&self, name: &str) -> Result<AsyncRWPtr<NodeEntry>, FlameError> {
        lookup_entry(&self.nodes, name, "Node")
    }

    fn executor_entry(&self, id: &str) -> Result<AsyncRWPtr<ExecutorEntry>, FlameError> {
        lookup_entry(&self.executors, id, "Executor")
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

    fn _list_sessions_metadata(
        &self,
        filter: Option<&SessionFilter>,
    ) -> Result<Vec<(SessionID, SessionMetadata)>, FlameError> {
        if filter.and_then(|filter| filter.limit) == Some(0) {
            return Ok(Vec::new());
        }
        let sessions_dir = self.base_path.join("sessions");
        let entries = match fs::read_dir(&sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(FlameError::Storage(format!(
                    "Failed to list sessions: {error}"
                )))
            }
        };

        let mut sessions = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                FlameError::Storage(format!("Failed to read a session directory entry: {error}"))
            })?;
            let session_id = entry.file_name().to_string_lossy().to_string();
            let metadata = self.read_session_metadata(&session_id)?;
            let matches = filter.is_none_or(|filter| {
                filter
                    .application
                    .as_ref()
                    .is_none_or(|application| metadata.application == *application)
                    && filter
                        .state
                        .is_none_or(|state| metadata.state == state as i32)
                    && filter
                        .ids
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&session_id))
            });
            if matches {
                sessions.push((session_id, metadata));
                if filter
                    .and_then(|filter| filter.limit)
                    .is_some_and(|limit| sessions.len() >= limit)
                {
                    break;
                }
            }
        }
        Ok(sessions)
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

    /// Persist `metadata` and make it the entry's current one.
    fn commit_session_metadata(
        &self,
        entry: &mut SessionEntry,
        metadata: SessionMetadata,
    ) -> Result<(), FlameError> {
        self.write_session_metadata(&metadata.id, &metadata)?;
        entry.metadata = metadata;
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

    /// Read one task record under the task's read lock, so it never observes a
    /// writer mid-update.
    fn read_task_metadata(
        &self,
        entry: &SessionEntry,
        task_id: TaskID,
    ) -> Result<TaskMetadata, FlameError> {
        entry.with_task_read(task_id, || self.read_task_record(entry, task_id))
    }

    /// Read and verify one task record. The caller holds the task's lock.
    fn read_task_record(
        &self,
        entry: &SessionEntry,
        task_id: TaskID,
    ) -> Result<TaskMetadata, FlameError> {
        if task_id < 1 {
            return Err(FlameError::NotFound(format!(
                "Invalid task ID: {task_id} (must be >= 1)"
            )));
        }

        let tasks = entry.part(Part::Tasks)?;
        let offset = (task_id as u64 - 1)
            .checked_mul(self.record_size as u64)
            .ok_or_else(|| {
                FlameError::NotFound(format!("Task {task_id} is outside the record range"))
            })?;
        let mut buffer = vec![0u8; self.record_size];
        tasks.file.read_exact_at(&mut buffer, offset).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                FlameError::NotFound(format!("Task {task_id} not found: {e}"))
            } else {
                FlameError::Storage(format!("Cannot read task {task_id}: {e}"))
            }
        })?;

        let (meta, _): (TaskMetadata, _) = postcard::take_from_bytes(&buffer).map_err(|e| {
            FlameError::Storage(format!("Failed to deserialize task metadata: {e}"))
        })?;

        let expected_checksum = calculate_checksum(&meta);
        if meta.checksum != expected_checksum {
            return Err(FlameError::Storage(format!(
                "Task {task_id} checksum mismatch: expected {expected_checksum}, got {}",
                meta.checksum
            )));
        }
        Ok(meta)
    }

    /// Write an existing task record in place. It is not synced explicitly: the kernel writes it back from the page cache.
    fn write_task_metadata(
        &self,
        entry: &SessionEntry,
        meta: &TaskMetadata,
    ) -> Result<(), FlameError> {
        let tasks = entry.part(Part::Tasks)?;
        let offset = (meta.id - 1) * self.record_size as u64;
        let buffer = postcard::to_allocvec(meta)
            .map_err(|e| FlameError::Storage(format!("Failed to serialize task metadata: {e}")))?;

        tasks
            .file
            .write_all_at(&buffer, offset)
            .map_err(|e| FlameError::Storage(format!("Failed to write task metadata: {e}")))?;

        Ok(())
    }

    /// Select the next dense task ID and write its record. The caller holds the
    /// session's commit gate, so this is the only writer extending tasks.bin.
    ///
    /// An incomplete tail left by an earlier failed write is not a committed task:
    /// it is discarded so the committed prefix stays dense. Write and sync errors
    /// are returned to the caller, which can retry the operation.
    fn commit_task(
        &self,
        entry: &SessionEntry,
        mut meta: TaskMetadata,
    ) -> Result<TaskMetadata, FlameError> {
        let tasks = entry.part(Part::Tasks)?;
        let record_size = self.record_size as u64;

        let mut len = tasks
            .file
            .metadata()
            .map_err(|e| FlameError::Storage(format!("Failed to stat tasks.bin: {e}")))?
            .len();
        if len % record_size != 0 {
            len -= len % record_size;
            tasks.file.set_len(len).map_err(|e| {
                FlameError::Storage(format!("Failed to discard incomplete task record: {e}"))
            })?;
        }

        meta.id = len / record_size + 1;
        meta.checksum = calculate_checksum(&meta);
        let buffer = postcard::to_allocvec(&meta)
            .map_err(|e| FlameError::Storage(format!("Failed to serialize task metadata: {e}")))?;

        // Readers of the new ID (a scan up to the file length) wait for the write.
        let written = entry.with_task(meta.id as TaskID, || tasks.file.write_all_at(&buffer, len));
        if let Err(e) = written {
            return Err(FlameError::Storage(format!(
                "Failed to commit task record: {e}"
            )));
        }

        Ok(meta)
    }

    /// Append an immutable payload and return its offset.
    fn append_data(
        &self,
        entry: &SessionEntry,
        part: Part,
        data: &[u8],
    ) -> Result<u64, FlameError> {
        entry.part(part)?.append(part.file_name(), data)
    }

    /// Read data from a file at the specified offset and length.
    fn read_data(
        &self,
        entry: &SessionEntry,
        part: Part,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FlameError> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let file = entry.part(part)?;
        let mut buffer = vec![0u8; len as usize];
        file.file.read_exact_at(&mut buffer, offset).map_err(|e| {
            FlameError::Storage(format!("Failed to read from {}: {e}", part.file_name()))
        })?;

        Ok(buffer)
    }

    /// Count tasks matching the filter.
    fn _count_task(&self, entry: &SessionEntry, filter: &TaskFilter) -> Result<u64, FlameError> {
        let path = self.session_path(&filter.session).join("tasks.bin");

        let length = fs::metadata(&path)
            .map_err(|e| FlameError::Storage(format!("Cannot stat task records: {e}")))?
            .len();
        if length % self.record_size as u64 != 0 {
            return Err(FlameError::Storage(
                "Partial task record in tasks.bin".into(),
            ));
        }
        let task_count = length / self.record_size as u64;
        let Some(states) = filter.states.as_ref() else {
            return Ok(task_count);
        };

        let mut count = 0;
        for task_id in 1..=task_count {
            let metadata = self.read_task_metadata(entry, task_id as TaskID)?;
            let state = TaskState::try_from(metadata.state as i32)?;
            if states.contains(&state) {
                count += 1;
            }
        }
        Ok(count)
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

    /// Convert TaskMetadata to Task, reading its immutable payloads.
    fn task_from_metadata(
        &self,
        entry: &SessionEntry,
        meta: &TaskMetadata,
    ) -> Result<Task, FlameError> {
        let input = if meta.input_len > 0 {
            let data = self.read_data(entry, Part::Inputs, meta.input_offset, meta.input_len)?;
            Some(Bytes::from(data))
        } else {
            None
        };

        let output = if meta.output_len > 0 {
            let data = self.read_data(entry, Part::Outputs, meta.output_offset, meta.output_len)?;
            Some(Bytes::from(data))
        } else {
            None
        };

        let affinity: std::collections::HashSet<Bytes> = if meta.affinity_len > 0 {
            let data = self.read_data(
                entry,
                Part::Affinity,
                meta.affinity_offset,
                meta.affinity_len,
            )?;
            let (keys, _): (Vec<Vec<u8>>, _) =
                postcard::take_from_bytes(&data).map_err(|e| FlameError::Storage(e.to_string()))?;
            keys.into_iter().map(Bytes::from).collect()
        } else {
            Default::default()
        };

        Self::task_with_data(&entry.metadata.id, meta, input, output, affinity)
    }

    /// Build a Task from a record and payloads the caller already holds.
    fn task_with_data(
        session_id: &str,
        meta: &TaskMetadata,
        input: Option<Bytes>,
        output: Option<Bytes>,
        affinity: std::collections::HashSet<Bytes>,
    ) -> Result<Task, FlameError> {
        let state = TaskState::try_from(meta.state as i32)?;
        let completion_time = if meta.completion_time > 0 {
            DateTime::from_timestamp(meta.completion_time, 0)
        } else {
            None
        };

        Ok(Task {
            id: meta.id as TaskID,
            ssn_id: session_id.to_string(),
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

    fn session_from_metadata(&self, meta: &SessionMetadata) -> Result<Session, FlameError> {
        let state = SessionState::try_from(meta.state)?;
        let common_data = self.read_common_data(&meta.id, meta.common_data_len)?;
        let completion_time = meta
            .completion_time
            .and_then(|t| DateTime::from_timestamp(t, 0));

        let resreq = match (meta.resreq_cpu, meta.resreq_memory, meta.resreq_gpu) {
            (Some(cpu), Some(memory), Some(gpu)) => Some(ResourceRequirement { cpu, memory, gpu }),
            _ => None,
        };

        Ok(Session {
            id: meta.id.clone(),
            application: meta.application.clone(),
            version: meta.version,
            common_data,
            tokens: meta.tokens.clone(),
            tasks: Default::default(),
            task_counts: Default::default(),
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
            name: meta.name.clone(),
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

    fn node_from_metadata(meta: &NodeMetadata) -> Node {
        Node {
            name: meta.name.clone(),
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
                arch: meta.info_arch.clone(),
                os: meta.info_os.clone(),
                ..Default::default()
            },
            // Reported by the executor manager each time the node registers.
            shims: Vec::new(),
        }
    }

    fn executor_from_metadata(meta: &ExecutorMetadata) -> Executor {
        Executor {
            id: meta.id.clone(),
            node: meta.node.clone(),
            resreq: ResourceRequirement {
                cpu: meta.resreq_cpu,
                memory: meta.resreq_memory,
                gpu: meta.resreq_gpu,
            },
            shim: Shim::try_from(meta.shim).unwrap_or_default(),
            application: meta.application.clone(),
            task_id: meta.task_id,
            ssn_id: meta.ssn_id.clone(),
            signal: None,
            attributes: Default::default(),
            creation_time: DateTime::from_timestamp(meta.creation_time, 0).unwrap_or_default(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::from(meta.state),
        }
    }

    /// Save an executor's metadata and remove its previous node's copy when it
    /// moved. The caller holds the executor's gate and its node's read gate.
    fn persist_executor(
        &self,
        executor: &Executor,
        metadata: &ExecutorMetadata,
        previous: Option<&ExecutorMetadata>,
    ) -> Result<(), FlameError> {
        self.write_executor_metadata(&executor.node, &executor.id, metadata)?;
        if let Some(previous) = previous.filter(|old| old.node != executor.node) {
            match fs::remove_dir_all(self.executor_path(&previous.node, &executor.id)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(FlameError::Storage(format!(
                        "Failed to remove previous executor metadata: {e}"
                    )))
                }
            }
        }
        Ok(())
    }

    /// Read-modify-write one task record. The caller holds the task's write gate
    /// (or the exclusive session lifecycle gate).
    fn update_task(
        &self,
        entry: &SessionEntry,
        task_id: TaskID,
        op: impl FnOnce(&mut TaskMetadata),
    ) -> Result<TaskMetadata, FlameError> {
        let mut meta = self.read_task_record(entry, task_id)?;
        op(&mut meta);
        meta.version += 1;
        meta.checksum = calculate_checksum(&meta);
        self.write_task_metadata(entry, &meta)?;
        Ok(meta)
    }

    fn _update_task_state(
        &self,
        entry: &SessionEntry,
        task_id: TaskID,
        task_state: TaskState,
    ) -> Result<TaskMetadata, FlameError> {
        self.update_task(entry, task_id, |meta| {
            meta.state = task_state as u8;
            if task_state.is_terminal() {
                meta.completion_time = Utc::now().timestamp();
            }
        })
    }
}

#[async_trait]
impl Engine for FilesystemEngine {
    async fn register_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        let schema = attr.schema.map(|s| ApplicationSchemaMetadata {
            input: s.input,
            output: s.output,
            common_data: s.common_data,
        });

        let meta = ApplicationMetadata {
            name: name.clone(),
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

        let application = Self::application_from_metadata(&meta)?;
        let entry = new_entry(
            &self.applications,
            &name,
            "Application",
            ApplicationEntry {
                metadata: meta.clone(),
            },
        )?;
        if let Err(error) = self.write_application_metadata(&name, &meta) {
            remove_entry(
                &self.applications,
                &name,
                OwnedRwLockWriteGuard::rwlock(&entry),
            )?;
            return Err(error);
        }
        Ok(application)
    }

    async fn update_application_state(
        &self,
        name: ApplicationID,
        state: ApplicationState,
    ) -> Result<Application, FlameError> {
        let shared = self.application_entry(&name)?;
        let mut entry = write_async!(shared);
        validate_entry(&self.applications, &name, &shared)?;
        let mut meta = entry.metadata.clone();
        if meta.state == state as i32 {
            return Self::application_from_metadata(&meta);
        }

        meta.state = state as i32;
        meta.version += 1;
        let application = Self::application_from_metadata(&meta)?;
        self.write_application_metadata(&name, &meta)?;
        entry.metadata = meta;
        Ok(application)
    }

    async fn delete_application(&self, name: ApplicationID) -> Result<(), FlameError> {
        let shared = self.application_entry(&name)?;
        let entry = write_async!(shared);
        validate_entry(&self.applications, &name, &shared)?;
        let app = &entry.metadata;
        if app.state != ApplicationState::Disabled as i32 {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> is not disabled"
            )));
        }

        let filter = SessionFilter {
            application: Some(name.clone()),
            ..SessionFilter::default()
        };
        let sessions = self._list_sessions_metadata(Some(&filter))?;
        if !sessions.is_empty() {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> still has sessions"
            )));
        }

        let app_dir = self.application_path(&name);
        fs::remove_dir_all(&app_dir).map_err(|e| {
            FlameError::Storage(format!("Failed to delete application '{}': {e}", name))
        })?;

        remove_entry(&self.applications, &name, &shared)?;
        Ok(())
    }

    async fn update_application(
        &self,
        name: String,
        attr: ApplicationAttributes,
    ) -> Result<Application, FlameError> {
        let shared = self.application_entry(&name)?;
        let mut entry = write_async!(shared);
        validate_entry(&self.applications, &name, &shared)?;
        let mut meta = entry.metadata.clone();
        if meta.state != ApplicationState::Enabled as i32 {
            return Err(FlameError::InvalidState(format!(
                "application <{name}> is not enabled"
            )));
        }

        let filter = SessionFilter::by_application(name.clone());
        let live = self
            ._list_sessions_metadata(Some(&filter))?
            .iter()
            .any(|(_, session)| session.state != SessionState::Closed as i32);
        if live {
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

        let application = Self::application_from_metadata(&meta)?;
        self.write_application_metadata(&name, &meta)?;
        entry.metadata = meta;
        Ok(application)
    }

    async fn get_application(&self, id: ApplicationID) -> Result<Application, FlameError> {
        let shared = self.application_entry(&id)?;
        let entry = read_async!(shared);
        validate_entry(&self.applications, &id, &shared)?;
        let metadata = &entry.metadata;
        Self::application_from_metadata(metadata)
    }

    async fn find_applications(
        &self,
        filter: Option<&ApplicationFilter>,
    ) -> Result<Vec<Application>, FlameError> {
        let names = self
            .applications
            .read()
            .map_err(poisoned)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut apps = Vec::new();
        for name in names {
            if let Ok(app) = self.get_application(name).await {
                if filter.is_none_or(|filter| filter.state.is_none_or(|state| app.state == state)) {
                    apps.push(app);
                }
            }
        }
        Ok(apps)
    }

    async fn create_session(&self, attr: SessionAttributes) -> Result<Session, FlameError> {
        if attr.application.is_empty() {
            return Err(FlameError::InvalidConfig(
                "Session application must not be empty".to_string(),
            ));
        }
        // Storage does not require the application to be registered; only an
        // existing one needs coordination with its deletion and update.
        let application = match self.application_entry(&attr.application) {
            Ok(application) => Some(application),
            Err(FlameError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let _application = match application.as_ref() {
            Some(application) => {
                let guard = read_async!(application);
                validate_entry(&self.applications, &attr.application, application)?;
                Some(guard)
            }
            None => None,
        };

        if self.read_session_metadata(&attr.id).is_ok() {
            return Err(FlameError::AlreadyExist(format!(
                "Session '{}' already exists",
                attr.id
            )));
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

        let tasks_path = session_dir.join("tasks.bin");
        let inputs_path = session_dir.join("inputs.bin");
        let outputs_path = session_dir.join("outputs.bin");

        fs::write(&tasks_path, [])
            .map_err(|e| FlameError::Storage(format!("Failed to create tasks.bin: {e}")))?;
        fs::write(&inputs_path, [])
            .map_err(|e| FlameError::Storage(format!("Failed to create inputs.bin: {e}")))?;
        fs::write(&outputs_path, [])
            .map_err(|e| FlameError::Storage(format!("Failed to create outputs.bin: {e}")))?;

        self.write_session_metadata(&attr.id, &meta)?;
        let session = self.session_from_metadata(&meta)?;
        self.sessions
            .write()
            .map_err(|e| FlameError::Storage(format!("Session registry poisoned: {e}")))?
            .entry(attr.id.clone())
            .or_insert_with(|| async_rw_ptr(SessionEntry::new(meta, session_dir)));

        Ok(session)
    }

    async fn get_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let entry = self
            .session_entry(&id)
            .map_err(|_| FlameError::NotFound(format!("Session {id} not found")))?;
        let meta = read_async!(entry);
        validate_entry(&self.sessions, &id, &entry)?;
        self.session_from_metadata(&meta.metadata)
    }

    async fn open_session(
        &self,
        id: SessionID,
        spec: Option<SessionAttributes>,
    ) -> Result<Session, FlameError> {
        match self.get_session(id.clone()).await {
            Ok(ssn) => {
                if ssn.status.state != SessionState::Open {
                    return Err(FlameError::InvalidState(format!(
                        "Session {id} is not open"
                    )));
                }
                if let Some(ref attr) = spec {
                    ssn.validate_spec(attr)?;
                }
                Ok(ssn)
            }
            Err(FlameError::NotFound(_)) => match spec {
                Some(attr) => self.create_session(attr).await,
                None => Err(FlameError::NotFound(format!("Session {id} not found"))),
            },
            Err(e) => Err(e),
        }
    }

    async fn close_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let shared = self.session_entry(&id)?;
        let mut entry = write_async!(shared);
        validate_entry(&self.sessions, &id, &shared)?;
        let already_closed = entry.is_closed();
        let task_count = self._count_task(&entry, &TaskFilter::by_session(&id))?;
        let mut pending_tasks = Vec::new();
        let mut counts = TaskCounts::default();
        for task_id in 1..=task_count {
            // Do not close over an unreadable task whose state is unknown.
            let task_meta = self.read_task_metadata(&entry, task_id as TaskID)?;
            let state = TaskState::try_from(task_meta.state as i32)?;
            if !already_closed && state == TaskState::Running {
                return Err(FlameError::Storage(
                    "Cannot close session with running tasks".to_string(),
                ));
            }
            if !already_closed && state == TaskState::Pending {
                pending_tasks.push(task_id as TaskID);
                counts.increment(TaskState::Cancelled);
            } else {
                counts.increment(state);
            }
        }
        for task_id in pending_tasks {
            self._update_task_state(&entry, task_id, TaskState::Cancelled)?;
        }
        if !already_closed {
            let mut metadata = entry.metadata.clone();
            metadata.state = SessionState::Closed as i32;
            metadata.completion_time = Some(Utc::now().timestamp());
            metadata.version += 1;
            self.commit_session_metadata(&mut entry, metadata)?;
        }
        entry.release_files();
        let mut session = self.session_from_metadata(&entry.metadata)?;
        session.task_counts = counts;
        Ok(session)
    }

    async fn update_session_state(
        &self,
        id: SessionID,
        state: SessionState,
    ) -> Result<Session, FlameError> {
        let shared = self.session_entry(&id)?;
        let mut entry = write_async!(shared);
        validate_entry(&self.sessions, &id, &shared)?;
        ensure!(
            !entry.is_closed(),
            FlameError::InvalidState(format!("Session {id} is closed"))
        )?;

        let mut metadata = entry.metadata.clone();
        metadata.state = state as i32;
        self.commit_session_metadata(&mut entry, metadata)?;
        if state == SessionState::Suspended {
            entry.release_files();
        }
        self.session_from_metadata(&entry.metadata)
    }

    async fn delete_session(&self, id: SessionID) -> Result<Session, FlameError> {
        let shared = self.session_entry(&id)?;
        let entry = write_async!(shared);
        validate_entry(&self.sessions, &id, &shared)?;
        if !entry.is_closed() {
            return Err(FlameError::Storage(
                "Cannot delete open session".to_string(),
            ));
        }
        if self._count_task(&entry, &TaskFilter::non_terminal(&id))? > 0 {
            return Err(FlameError::Storage(
                "Cannot delete session with non-terminal tasks".to_string(),
            ));
        }
        let session = self.session_from_metadata(&entry.metadata)?;
        fs::remove_dir_all(&entry.dir)
            .map_err(|e| FlameError::Storage(format!("Failed to delete session: {e}")))?;
        entry.release_files();
        self.sessions.write().map_err(poisoned)?.remove(&id);
        Ok(session)
    }

    async fn find_sessions(&self) -> Result<Vec<Session>, FlameError> {
        let entries = self
            .sessions
            .read()
            .map_err(poisoned)?
            .iter()
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect::<Vec<_>>();
        let mut sessions = Vec::new();
        for (id, shared) in entries {
            let entry = read_async!(shared);
            // A session deleted while we waited is simply no longer listed.
            if validate_entry(&self.sessions, &id, &shared).is_ok() {
                sessions.push(self.session_from_metadata(&entry.metadata)?);
            }
        }
        Ok(sessions)
    }

    async fn create_task(
        &self,
        ssn_id: SessionID,
        input: Option<TaskInput>,
        options: Option<TaskOptions>,
    ) -> Result<Task, FlameError> {
        lock_ssn!(self, &ssn_id, entry);

        // A suspending or suspended session still accepts tasks and task updates.
        ensure!(
            !entry.is_closed(),
            FlameError::InvalidState(format!("Session {} is closed", entry.metadata.id))
        )?;

        // Stage the immutable payloads first. Their ranges are reserved
        // atomically, so neither the writes nor their syncs hold a gate, and an
        // abandoned creation only leaves an unreferenced range.
        let input = input.filter(|data| !data.is_empty());
        let (input_offset, input_len) = if let Some(ref data) = input {
            let offset = self.append_data(&entry, Part::Inputs, data)?;
            (offset, data.len() as u64)
        } else {
            (0, 0)
        };

        let affinity = options.unwrap_or_default().affinity;
        let (affinity_offset, affinity_len) = if affinity.is_empty() {
            (0, 0)
        } else {
            let keys = affinity.iter().map(|key| key.to_vec()).collect::<Vec<_>>();
            let encoded =
                postcard::to_allocvec(&keys).map_err(|e| FlameError::Storage(e.to_string()))?;
            let offset = self.append_data(&entry, Part::Affinity, &encoded)?;
            (offset, encoded.len() as u64)
        };

        let meta = TaskMetadata {
            id: 0,
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
            affinity_len,
        };

        // The commit gate covers task ID selection and the record write only. It
        // never waits for a task gate: the ID is not visible to anyone until this
        // returns, so no other operation can reference the new task.
        let meta = {
            let _commit = entry.commit.lock().await;
            self.commit_task(&entry, meta)?
        };

        // The request already owns these immutable fields; reading them back would
        // add I/O without validating anything new.
        Self::task_with_data(&ssn_id, &meta, input, None, affinity)
    }

    async fn get_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        lock_ssn!(self, &gid.ssn_id, entry);
        let meta = self.read_task_metadata(&entry, gid.task_id)?;
        self.task_from_metadata(&entry, &meta)
    }

    async fn retry_task(&self, gid: TaskGID) -> Result<Task, FlameError> {
        lock_ssn!(self, &gid.ssn_id, entry);
        ensure!(
            entry.is_open(),
            FlameError::InvalidState(format!("Session {} is not open", entry.metadata.id))
        )?;

        let meta = entry.with_task(gid.task_id, || {
            self.update_task(&entry, gid.task_id, |meta| {
                meta.state = TaskState::Pending as u8;
            })
        })?;
        self.task_from_metadata(&entry, &meta)
    }

    async fn update_task_state(
        &self,
        current: Task,
        task_state: TaskState,
        _message: Option<String>,
    ) -> Result<Task, FlameError> {
        let gid = current.gid();
        lock_ssn!(self, &gid.ssn_id, entry);
        // A suspending or suspended session still accepts tasks and task updates.
        ensure!(
            !entry.is_closed(),
            FlameError::InvalidState(format!("Session {} is closed", entry.metadata.id))
        )?;

        let meta = entry.with_task(gid.task_id, || {
            self._update_task_state(&entry, gid.task_id, task_state)
        })?;
        self.task_from_metadata(&entry, &meta)
    }

    async fn update_task_result(
        &self,
        current: Task,
        task_result: TaskResult,
    ) -> Result<Task, FlameError> {
        let gid = current.gid();
        lock_ssn!(self, &gid.ssn_id, entry);
        // A suspending or suspended session still accepts tasks and task updates.
        ensure!(
            !entry.is_closed(),
            FlameError::InvalidState(format!("Session {} is closed", entry.metadata.id))
        )?;

        // Stage the output before taking the task gate. If the update is later
        // rejected or fails, the range stays unreferenced.
        let output = match task_result.output.as_ref() {
            Some(output) if !output.is_empty() => Some((
                self.append_data(&entry, Part::Outputs, output)?,
                output.len() as u64,
            )),
            Some(_) => Some((0, 0)),
            None => None,
        };

        let meta = entry.with_task(gid.task_id, || {
            self.update_task(&entry, gid.task_id, |meta| {
                if let Some((offset, len)) = output {
                    meta.output_offset = offset;
                    meta.output_len = len;
                }
                meta.state = task_result.state as u8;
                if task_result.state.is_terminal() {
                    meta.completion_time = Utc::now().timestamp();
                }
            })
        })?;
        self.task_from_metadata(&entry, &meta)
    }

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>, FlameError> {
        lock_ssn!(self, &filter.session, entry);
        if filter.states.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let task_count = {
            let _commit = entry.commit.lock().await;
            self._count_task(&entry, &TaskFilter::by_session(&filter.session))?
        };
        let mut tasks = Vec::new();
        for task_id in 1..=task_count {
            let meta = self.read_task_metadata(&entry, task_id as TaskID)?;
            let state = TaskState::try_from(meta.state as i32)?;
            if filter
                .states
                .as_ref()
                .is_none_or(|states| states.contains(&state))
            {
                tasks.push(self.task_from_metadata(&entry, &meta)?);
            }
        }
        Ok(tasks)
    }

    async fn create_node(&self, node: &Node) -> Result<Node, FlameError> {
        let metadata = NodeMetadata::from_node(node, Utc::now().timestamp());
        let mut entry = new_entry(
            &self.nodes,
            &node.name,
            "Node",
            NodeEntry {
                metadata: metadata.clone(),
            },
        )?;
        if let Err(error) = self.write_node_metadata(&node.name, &metadata) {
            remove_entry(
                &self.nodes,
                &node.name,
                OwnedRwLockWriteGuard::rwlock(&entry),
            )?;
            return Err(error);
        }
        entry.metadata = metadata;
        Ok(node.clone())
    }

    async fn get_node(&self, name: &str) -> Result<Option<Node>, FlameError> {
        let shared = match self.node_entry(name) {
            Ok(shared) => shared,
            Err(FlameError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let entry = read_async!(shared);
        validate_entry(&self.nodes, name, &shared)?;
        Ok(Some(Self::node_from_metadata(&entry.metadata)))
    }

    async fn update_node(&self, node: &Node) -> Result<Node, FlameError> {
        let shared = self.node_entry(&node.name)?;
        let mut entry = write_async!(shared);
        validate_entry(&self.nodes, &node.name, &shared)?;
        let metadata = NodeMetadata::from_node(node, entry.metadata.creation_time);
        self.write_node_metadata(&node.name, &metadata)?;
        entry.metadata = metadata;
        Ok(node.clone())
    }

    async fn delete_node(&self, name: &str) -> Result<(), FlameError> {
        let shared = match self.node_entry(name) {
            Ok(shared) => shared,
            Err(FlameError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let _entry = write_async!(shared);
        validate_entry(&self.nodes, name, &shared)?;
        match fs::remove_dir_all(self.node_path(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(FlameError::Storage(format!(
                    "Failed to delete node {name}: {e}"
                )))
            }
        }
        let executors = self
            .executors
            .read()
            .map_err(poisoned)?
            .iter()
            .map(|(id, shared)| (id.clone(), shared.clone()))
            .collect::<Vec<_>>();
        // Node -> executor: active operations on this node have finished; a
        // concurrent move to another node is protected by the executor gate.
        for (id, executor) in executors {
            let entry = write_async!(executor);
            // An executor deleted while we waited for its gate is already gone.
            if entry.metadata.node == name
                && validate_entry(&self.executors, &id, &executor).is_ok()
            {
                remove_entry(&self.executors, &id, &executor)?;
            }
        }
        remove_entry(&self.nodes, name, &shared)?;
        Ok(())
    }

    async fn find_nodes(&self) -> Result<Vec<Node>, FlameError> {
        let names = self
            .nodes
            .read()
            .map_err(poisoned)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for name in names {
            if let Some(node) = self.get_node(&name).await? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    async fn create_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        let parent = self.node_entry(&executor.node)?;
        let _parent = read_async!(parent);
        validate_entry(&self.nodes, &executor.node, &parent)?;
        let metadata = ExecutorMetadata::from_executor(executor);
        let mut entry = new_entry(
            &self.executors,
            &executor.id,
            "Executor",
            ExecutorEntry {
                metadata: metadata.clone(),
            },
        )?;
        if let Err(error) = self.persist_executor(executor, &metadata, None) {
            remove_entry(
                &self.executors,
                &executor.id,
                OwnedRwLockWriteGuard::rwlock(&entry),
            )?;
            return Err(error);
        }
        entry.metadata = metadata;
        Ok(executor.clone())
    }

    async fn get_executor(&self, id: &ExecutorID) -> Result<Option<Executor>, FlameError> {
        loop {
            let shared = match self.executor_entry(id) {
                Ok(shared) => shared,
                Err(FlameError::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            let node = {
                let entry = read_async!(shared);
                validate_entry(&self.executors, id, &shared)?;
                entry.metadata.node.clone()
            };
            let parent = match self.node_entry(&node) {
                Ok(parent) => parent,
                Err(FlameError::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            let _parent = read_async!(parent);
            validate_entry(&self.nodes, &node, &parent)?;
            let entry = read_async!(shared);
            validate_entry(&self.executors, id, &shared)?;
            if entry.metadata.node != node {
                continue;
            }
            return Ok(Some(Self::executor_from_metadata(&entry.metadata)));
        }
    }

    async fn update_executor(&self, executor: &Executor) -> Result<Executor, FlameError> {
        let parent = self.node_entry(&executor.node)?;
        let _parent = read_async!(parent);
        validate_entry(&self.nodes, &executor.node, &parent)?;
        let shared = self.executor_entry(&executor.id)?;
        let mut entry = write_async!(shared);
        validate_entry(&self.executors, &executor.id, &shared)?;
        let metadata = ExecutorMetadata::from_executor(executor);
        self.persist_executor(executor, &metadata, Some(&entry.metadata))?;
        entry.metadata = metadata;
        Ok(executor.clone())
    }

    async fn update_executor_state(
        &self,
        id: &ExecutorID,
        state: ExecutorState,
    ) -> Result<Executor, FlameError> {
        loop {
            let shared = self.executor_entry(id)?;
            let node = {
                let entry = read_async!(shared);
                validate_entry(&self.executors, id, &shared)?;
                entry.metadata.node.clone()
            };
            let parent = self.node_entry(&node)?;
            let _parent = read_async!(parent);
            validate_entry(&self.nodes, &node, &parent)?;
            let mut entry = write_async!(shared);
            validate_entry(&self.executors, id, &shared)?;
            if entry.metadata.node != node {
                continue;
            }
            let mut metadata = entry.metadata.clone();
            metadata.state = i32::from(state);
            self.write_executor_metadata(&node, id, &metadata)?;
            entry.metadata = metadata;
            return Ok(Self::executor_from_metadata(&entry.metadata));
        }
    }

    async fn delete_executor(&self, id: &ExecutorID) -> Result<(), FlameError> {
        loop {
            let shared = match self.executor_entry(id) {
                Ok(shared) => shared,
                Err(FlameError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            let node = {
                let entry = read_async!(shared);
                validate_entry(&self.executors, id, &shared)?;
                entry.metadata.node.clone()
            };
            let parent = match self.node_entry(&node) {
                Ok(parent) => parent,
                Err(FlameError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            let _parent = read_async!(parent);
            validate_entry(&self.nodes, &node, &parent)?;
            let entry = write_async!(shared);
            validate_entry(&self.executors, id, &shared)?;
            if entry.metadata.node != node {
                continue;
            }
            match fs::remove_dir_all(self.executor_path(&node, id)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(FlameError::Storage(format!(
                        "Failed to delete executor {id}: {e}"
                    )))
                }
            }
            remove_entry(&self.executors, id, &shared)?;
            return Ok(());
        }
    }

    async fn find_executors(&self, node: Option<&str>) -> Result<Vec<Executor>, FlameError> {
        let ids = self
            .executors
            .read()
            .map_err(poisoned)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut executors = Vec::new();
        for id in ids {
            if let Some(executor) = self.get_executor(&id).await? {
                if node.is_none_or(|name| executor.node == name) {
                    executors.push(executor);
                }
            }
        }
        Ok(executors)
    }
}

#[cfg(test)]
mod tests;

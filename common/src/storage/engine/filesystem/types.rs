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

use super::payload::{Part, PayloadFile};
use super::poisoned;
use crate::apis::{Executor, Node, SessionState, TaskID};
use crate::FlameError;
use bincode::{Decode, Encode};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

/// Task metadata stored in tasks.bin with fixed-size records.
///
/// Uses `bincode` with `fixint` encoding to ensure constant serialized size.
/// The checksum is calculated using `crc32fast` for data integrity.
#[derive(Encode, Decode, Debug, Clone, Default)]
pub(super) struct TaskMetadata {
    /// Task ID (index in file)
    pub(super) id: u64,
    /// Optimistic locking version
    pub(super) version: u32,
    /// CRC32 checksum of the record (excluding this field)
    pub(super) checksum: u32,
    /// Task state (TaskState enum as u8)
    pub(super) state: u8,
    /// Unix timestamp of creation
    pub(super) creation_time: i64,
    /// Unix timestamp of completion (0 if not completed)
    pub(super) completion_time: i64,
    /// Offset in inputs.bin where input data starts
    pub(super) input_offset: u64,
    /// Length of input data in bytes
    pub(super) input_len: u64,
    /// Offset in outputs.bin where output data starts
    pub(super) output_offset: u64,
    /// Length of output data in bytes
    pub(super) output_len: u64,
    /// Offset in affinity.bin where the encoded affinity vector starts
    pub(super) affinity_offset: u64,
    /// Length of the encoded affinity vector in bytes
    pub(super) affinity_len: u64,
}

/// Application metadata stored as JSON.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct ApplicationMetadata {
    pub(super) name: String,
    pub(super) version: u32,
    pub(super) state: i32,
    pub(super) creation_time: i64,
    #[serde(default)]
    pub(super) shim: i32, // 0 = Host (default), 1 = Wasm, 3 = Cri
    pub(super) image: Option<String>,
    pub(super) description: Option<String>,
    pub(super) labels: Vec<String>,
    pub(super) command: Option<String>,
    pub(super) arguments: Vec<String>,
    pub(super) environments: std::collections::HashMap<String, String>,
    pub(super) working_directory: Option<String>,
    pub(super) max_instances: u32,
    pub(super) delay_release_seconds: i64,
    pub(super) schema: Option<ApplicationSchemaMetadata>,
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) installer: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct ApplicationSchemaMetadata {
    pub(super) input: Option<String>,
    pub(super) output: Option<String>,
    pub(super) common_data: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct NodeMetadata {
    pub(super) name: String,
    pub(super) state: i32,
    pub(super) capacity_cpu: u64,
    pub(super) capacity_memory: u64,
    #[serde(default)]
    pub(super) capacity_gpu: i32,
    pub(super) allocatable_cpu: u64,
    pub(super) allocatable_memory: u64,
    #[serde(default)]
    pub(super) allocatable_gpu: i32,
    pub(super) info_arch: String,
    pub(super) info_os: String,
    pub(super) creation_time: i64,
    pub(super) last_heartbeat: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct ExecutorMetadata {
    pub(super) id: String,
    pub(super) node: String,
    pub(super) application: String,
    pub(super) resreq_cpu: u64,
    pub(super) resreq_memory: u64,
    #[serde(default)]
    pub(super) resreq_gpu: i32,
    pub(super) shim: i32,
    pub(super) task_id: Option<i64>,
    pub(super) ssn_id: Option<String>,
    pub(super) creation_time: i64,
    pub(super) state: i32,
}

impl NodeMetadata {
    pub(super) fn from_node(node: &Node, creation_time: i64) -> Self {
        Self {
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
        }
    }
}

impl ExecutorMetadata {
    pub(super) fn from_executor(executor: &Executor) -> Self {
        Self {
            id: executor.id.clone(),
            node: executor.node.clone(),
            application: executor.application.clone(),
            resreq_cpu: executor.resreq.cpu,
            resreq_memory: executor.resreq.memory,
            resreq_gpu: executor.resreq.gpu,
            shim: i32::from(executor.shim),
            task_id: executor.task_id,
            ssn_id: executor.ssn_id.clone(),
            creation_time: executor.creation_time.timestamp(),
            state: i32::from(executor.state),
        }
    }
}

/// Bincode configuration for fixed-size encoding.
pub(super) fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_fixed_int_encoding()
        .with_little_endian()
}

/// Calculate the fixed record size for TaskMetadata.
pub(super) fn task_record_size() -> usize {
    // Calculate the serialized size of a default TaskMetadata
    let meta = TaskMetadata::default();
    bincode::encode_to_vec(&meta, bincode_config())
        .expect("Failed to calculate record size")
        .len()
}

/// Calculate CRC32 checksum for task metadata (excluding the checksum field itself).
pub(super) fn calculate_checksum(meta: &TaskMetadata) -> u32 {
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

/// Application data and its admission/update gate share one registry entry.
pub(super) struct ApplicationEntry {
    pub(super) metadata: ApplicationMetadata,
}

/// Persisted node metadata coordinated with its executor operations.
pub(super) struct NodeEntry {
    pub(super) metadata: NodeMetadata,
}

pub(super) struct ExecutorEntry {
    pub(super) metadata: ExecutorMetadata,
}

/// Session metadata stored as JSON.
#[derive(Serialize, Deserialize, Clone, Default)]
pub(super) struct SessionMetadata {
    pub(super) id: String,
    pub(super) application: String,
    pub(super) version: u32,
    pub(super) state: i32,
    pub(super) creation_time: i64,
    pub(super) completion_time: Option<i64>,
    pub(super) min_instances: u32,
    pub(super) max_instances: Option<u32>,
    #[serde(default = "default_batch_size")]
    pub(super) batch_size: u32,
    #[serde(default)]
    pub(super) priority: u32,
    pub(super) common_data_len: u64,
    #[serde(default)]
    pub(super) tokens: HashMap<String, String>,
    #[serde(default)]
    pub(super) resreq_cpu: Option<u64>,
    #[serde(default)]
    pub(super) resreq_memory: Option<u64>,
    #[serde(default)]
    pub(super) resreq_gpu: Option<i32>,
}

/// Session metadata and the runtime resources shared by its operations.
#[derive(Default)]
pub(super) struct SessionEntry {
    pub(super) metadata: SessionMetadata,
    pub(super) dir: PathBuf,
    pub(super) commit: Mutex<()>,
    pub(super) tasks: parking_lot::Mutex<HashMap<TaskID, Arc<parking_lot::RwLock<()>>>>,
    pub(super) files: [StdMutex<Option<Arc<PayloadFile>>>; Part::COUNT],
}

fn default_batch_size() -> u32 {
    1
}

impl SessionEntry {
    pub(super) fn new(metadata: SessionMetadata, dir: PathBuf) -> Self {
        Self {
            metadata,
            dir,
            ..Default::default()
        }
    }

    pub(super) fn ensure_open(&self) -> Result<(), FlameError> {
        if self.metadata.state != SessionState::Open as i32 {
            return Err(FlameError::InvalidState(format!(
                "Session {} is closed",
                self.metadata.id
            )));
        }
        Ok(())
    }

    /// The lock of one task, created on first use and kept for the life of the
    /// session entry.
    pub(super) fn task_lock(&self, task_id: TaskID) -> Arc<parking_lot::RwLock<()>> {
        self.tasks.lock().entry(task_id).or_default().clone()
    }

    /// Run `f` holding the task's write lock. `f` is always synchronous, so a
    /// lock is never held across an await and a waiter only waits for one
    /// record read-modify-write.
    pub(super) fn with_task<R>(&self, task_id: TaskID, f: impl FnOnce() -> R) -> R {
        let lock = self.task_lock(task_id);
        let _guard = lock.write();
        f()
    }

    /// Run `f` holding the task's read lock: no writer is mid-update.
    pub(super) fn with_task_read<R>(&self, task_id: TaskID, f: impl FnOnce() -> R) -> R {
        let lock = self.task_lock(task_id);
        let _guard = lock.read();
        f()
    }

    /// The shared handle for one file, opened on first use.
    pub(super) fn part(&self, part: Part) -> Result<Arc<PayloadFile>, FlameError> {
        let path = self.dir.join(part.file_name());
        if self.metadata.state == SessionState::Closed as i32 {
            // Closed sessions stay readable but must not retain descriptors.
            return Ok(Arc::new(PayloadFile::open(&path, false)?));
        }

        let mut slot = self.files[part as usize].lock().map_err(poisoned)?;
        if let Some(file) = slot.as_ref() {
            return Ok(file.clone());
        }
        let file = Arc::new(PayloadFile::open(&path, true)?);
        *slot = Some(file.clone());
        Ok(file)
    }

    pub(super) fn release_files(&self) {
        for slot in &self.files {
            if let Ok(mut slot) = slot.lock() {
                *slot = None;
            }
        }
    }
}

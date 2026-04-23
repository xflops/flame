/*
Copyright 2025 The Flame Authors.
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
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use arrow::array::{BinaryArray, RecordBatch, UInt64Array};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::{
    CompressionContext, DictionaryTracker, IpcDataGenerator, IpcWriteOptions,
};
use arrow::ipc::{reader::FileReader, writer::FileWriter, CompressionType};
use arrow_flight::{
    flight_service_server::{FlightService, FlightServiceServer},
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, Location, PutResult, Result as FlightResult, SchemaResult,
    Ticket,
};
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use bytesize::ByteSize;
use futures::Stream;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use regex::Regex;
use stdng::{lock_ptr, new_ptr, MutexPtr};
use tonic::{Request, Response, Status, Streaming};
use url::Url;

use common::apis::SessionID;
use common::ctx::FlameCache;
use common::FlameError;

use crate::eviction::{new_policy, EvictionConfig, EvictionPolicyPtr};

/// Maximum number of deltas allowed per object before requiring compaction.
/// This prevents unbounded growth of delta files.
/// When this limit is reached, patch operations will return an error suggesting
/// the client should use update_object to compact the deltas.
const MAX_DELTAS_PER_OBJECT: u64 = 1000;

/// Default batch size for eviction operations
const EVICTION_BATCH_SIZE: usize = 10;

/// Validate that a key (session_id/object_id format) does not contain path traversal sequences.
/// Security: Prevents directory traversal attacks via user-controlled input.
fn validate_key(key: &str) -> Result<(), FlameError> {
    if key.contains("..") || key.starts_with('/') || key.contains("//") {
        return Err(FlameError::InvalidConfig(format!(
            "Invalid key '{}': contains path traversal sequences",
            key
        )));
    }
    Ok(())
}

/// Validate a session ID does not contain path traversal sequences.
/// Security: Prevents directory traversal attacks via user-controlled session IDs.
fn validate_session_id(session_id: &str) -> Result<(), FlameError> {
    if session_id.contains("..") || session_id.contains('/') || session_id.contains('\\') {
        return Err(FlameError::InvalidConfig(format!(
            "Invalid session_id '{}': contains path traversal sequences",
            session_id
        )));
    }
    Ok(())
}

/// Validate an object ID does not contain path traversal sequences.
/// Security: Prevents directory traversal attacks via user-controlled object IDs.
fn validate_object_id(object_id: &str) -> Result<(), FlameError> {
    if object_id.contains("..") || object_id.contains('/') || object_id.contains('\\') {
        return Err(FlameError::InvalidConfig(format!(
            "Invalid object_id '{}': contains path traversal sequences",
            object_id
        )));
    }
    Ok(())
}

/// Object with optional delta support
/// Per HLD: deltas field is empty for delta objects themselves
/// Note: This struct is immutable after construction - use with_deltas() to create
/// a new Object with deltas populated rather than mutating an existing one.
#[derive(Debug, Clone)]
pub struct Object {
    pub version: u64,
    pub data: Vec<u8>,
    pub deltas: Vec<Object>,
}

impl Object {
    /// Create a new Object with no deltas
    pub fn new(version: u64, data: Vec<u8>) -> Self {
        Self {
            version,
            data,
            deltas: Vec::new(),
        }
    }

    /// Create a new Object with deltas
    /// This is the preferred way to create an Object with deltas rather than
    /// mutating an existing Object's deltas field.
    pub fn with_deltas(version: u64, data: Vec<u8>, deltas: Vec<Object>) -> Self {
        Self {
            version,
            data,
            deltas,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,
    pub size: u64,
    pub delta_count: u64,
}

#[derive(Debug, Clone)]
pub struct CacheEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl CacheEndpoint {
    /// Convert to URI string for clients.
    /// Converts internal scheme names to client-compatible formats:
    /// - grpcs -> grpc+tls (for PyArrow Flight compatibility)
    fn to_uri(&self) -> String {
        let client_scheme = match self.scheme.as_str() {
            "grpcs" => "grpc+tls",
            other => other,
        };
        format!("{}://{}:{}", client_scheme, self.host, self.port)
    }

    fn get_host(cache_config: &FlameCache) -> Result<String, FlameError> {
        let network_interfaces =
            NetworkInterface::show().map_err(|e| FlameError::Network(e.to_string()))?;

        let reg = Regex::new(cache_config.network_interface.as_str())
            .map_err(|e| FlameError::InvalidConfig(e.to_string()))?;
        let host = network_interfaces
            .iter()
            .find(|iface| reg.is_match(iface.name.as_str()))
            .ok_or(FlameError::InvalidConfig(format!(
                "network interface <{}> not found",
                cache_config.network_interface
            )))?
            .clone();

        Ok(host
            .addr
            .iter()
            .find(|ip| ip.ip().is_ipv4())
            .ok_or(FlameError::InvalidConfig(format!(
                "network interface <{}> has no IPv4 addresses",
                cache_config.network_interface
            )))?
            .ip()
            .to_string())
    }
}

impl TryFrom<&FlameCache> for CacheEndpoint {
    type Error = FlameError;

    fn try_from(cache_config: &FlameCache) -> Result<Self, Self::Error> {
        let endpoint = CacheEndpoint::try_from(&cache_config.endpoint)?;
        let host = Self::get_host(cache_config)?;

        Ok(Self {
            scheme: endpoint.scheme,
            host,
            port: endpoint.port,
        })
    }
}

impl TryFrom<&String> for CacheEndpoint {
    type Error = FlameError;

    fn try_from(endpoint: &String) -> Result<Self, Self::Error> {
        let url = Url::parse(endpoint)
            .map_err(|_| FlameError::InvalidConfig(format!("invalid endpoint <{}>", endpoint)))?;

        Ok(Self {
            scheme: url.scheme().to_string(),
            host: url
                .host_str()
                .ok_or(FlameError::InvalidConfig(format!(
                    "no host in endpoint <{}>",
                    endpoint
                )))?
                .to_string(),
            port: url.port().unwrap_or(9090),
        })
    }
}

pub struct ObjectCache {
    endpoint: CacheEndpoint,
    storage_path: Option<PathBuf>,
    /// In-memory object storage (key present = in memory)
    objects: MutexPtr<HashMap<String, Object>>,
    /// Object metadata index (always in memory, tracks all objects)
    metadata: MutexPtr<HashMap<String, ObjectMetadata>>,
    /// Eviction policy
    eviction_policy: EvictionPolicyPtr,
}

impl ObjectCache {
    fn new(
        endpoint: CacheEndpoint,
        storage_path: Option<PathBuf>,
        eviction_config: Option<&EvictionConfig>,
    ) -> Result<Self, FlameError> {
        let eviction_policy = new_policy(eviction_config);

        let cache = Self {
            endpoint,
            storage_path: storage_path.clone(),
            objects: new_ptr(HashMap::new()),
            metadata: new_ptr(HashMap::new()),
            eviction_policy,
        };

        // Load existing objects from disk
        if let Some(storage_path) = &storage_path {
            cache.load_from_disk(storage_path)?;
        }

        Ok(cache)
    }

    fn create_metadata(&self, key: String, size: u64, delta_count: u64) -> ObjectMetadata {
        ObjectMetadata {
            endpoint: self.endpoint.to_uri(),
            key,
            version: 0,
            size,
            delta_count,
        }
    }

    /// Get the delta directory path for an object
    fn get_delta_dir(&self, key: &str) -> Option<PathBuf> {
        self.storage_path
            .as_ref()
            .map(|p| p.join(format!("{}.deltas", key)))
    }

    /// Count the number of delta files for an object.
    /// Returns 0 if the delta directory doesn't exist or is empty.
    fn count_deltas(&self, key: &str) -> u64 {
        let delta_dir = match self.get_delta_dir(key) {
            Some(dir) => dir,
            None => return 0,
        };

        if !delta_dir.exists() {
            return 0;
        }

        match fs::read_dir(&delta_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("arrow"))
                .count() as u64,
            Err(e) => {
                tracing::warn!("Failed to read delta directory {:?}: {}", delta_dir, e);
                0
            }
        }
    }

    /// Delete all deltas for an object
    fn clear_deltas(&self, key: &str) -> Result<(), FlameError> {
        if let Some(delta_dir) = self.get_delta_dir(key) {
            if delta_dir.exists() {
                fs::remove_dir_all(&delta_dir)?;
                tracing::debug!("Cleared deltas for object: {}", key);
            }
        }
        Ok(())
    }

    fn load_session_objects(
        &self,
        session_path: &Path,
        objects: &mut HashMap<String, Object>,
        metadata_map: &mut HashMap<String, ObjectMetadata>,
    ) -> Result<(), FlameError> {
        let session_id = session_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| FlameError::Internal("Invalid session directory name".to_string()))?;

        for object_entry in fs::read_dir(session_path)? {
            let object_entry = object_entry?;
            let object_path = object_entry.path();

            // Skip delta directories
            if object_path.is_dir() {
                continue;
            }

            if object_path.extension().and_then(|e| e.to_str()) != Some("arrow") {
                continue;
            }

            let object_id = object_path
                .file_stem()
                .and_then(|n| n.to_str())
                .ok_or_else(|| FlameError::Internal("Invalid object file name".to_string()))?;

            let key = format!("{}/{}", session_id, object_id);
            let size = fs::metadata(&object_path)?.len();
            let delta_count = self.count_deltas(&key);

            // Load object into memory
            let object = self.load_object_from_disk_internal(&object_path)?;
            let meta = self.create_metadata(key.clone(), size, delta_count);

            tracing::debug!("Loaded object: {} (deltas: {})", key, delta_count);
            objects.insert(key.clone(), object);
            metadata_map.insert(key.clone(), meta);

            // Track in eviction policy
            self.eviction_policy.on_add(&key, size);
        }

        Ok(())
    }

    fn load_from_disk(&self, storage_path: &Path) -> Result<(), FlameError> {
        if !storage_path.exists() {
            tracing::info!("Creating storage directory: {:?}", storage_path);
            fs::create_dir_all(storage_path)?;
            return Ok(());
        }

        tracing::info!("Loading objects from disk: {:?}", storage_path);
        let mut objects = lock_ptr!(self.objects)?;
        let mut metadata = lock_ptr!(self.metadata)?;

        for session_entry in fs::read_dir(storage_path)? {
            let session_entry = session_entry?;
            let session_path = session_entry.path();

            if !session_path.is_dir() {
                continue;
            }

            self.load_session_objects(&session_path, &mut objects, &mut metadata)?;
        }

        tracing::info!("Loaded {} objects from disk", objects.len());

        // Run eviction if needed after loading
        drop(objects);
        drop(metadata);
        self.run_eviction()?;

        Ok(())
    }

    /// Run eviction if needed, removing least recently used objects from memory.
    fn run_eviction(&self) -> Result<(), FlameError> {
        loop {
            let keys_to_evict = self.eviction_policy.victims(EVICTION_BATCH_SIZE);
            if keys_to_evict.is_empty() {
                break;
            }

            let mut objects = lock_ptr!(self.objects)?;
            for key in keys_to_evict {
                if objects.contains_key(&key) {
                    // Remove from in-memory storage (metadata is kept)
                    objects.remove(&key);
                    self.eviction_policy.on_evict(&key);
                    tracing::debug!("Evicted object from memory: {}", key);
                }
            }
        }
        Ok(())
    }

    fn load_object_from_disk_internal(&self, object_path: &Path) -> Result<Object, FlameError> {
        let file = fs::File::open(object_path)
            .map_err(|e| FlameError::NotFound(format!("Object file not found: {}", e)))?;
        let reader = FileReader::try_new(file, None)
            .map_err(|e| FlameError::Internal(format!("Failed to create reader: {}", e)))?;

        // SAFETY: Skip validation for trusted cache data (3-9x faster reads).
        // This is safe because all data in the cache was written by this service.
        let reader = unsafe { reader.with_skip_validation(true) };

        let batch = reader
            .into_iter()
            .next()
            .ok_or_else(|| FlameError::Internal("No batches in file".to_string()))?
            .map_err(|e| FlameError::Internal(format!("Failed to read batch: {}", e)))?;

        let object = batch_to_object(&batch)
            .map_err(|e| FlameError::Internal(format!("Failed to parse batch: {}", e)))?;

        Ok(object)
    }

    async fn put(
        &self,
        session_id: SessionID,
        object: Object,
    ) -> Result<ObjectMetadata, FlameError> {
        self.put_with_id(session_id, None, object).await
    }

    async fn put_with_id(
        &self,
        session_id: SessionID,
        object_id: Option<String>,
        object: Object,
    ) -> Result<ObjectMetadata, FlameError> {
        validate_session_id(&session_id)?;
        let object_id = object_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        validate_object_id(&object_id)?;

        let key = format!("{}/{}", session_id, object_id);
        let size = object.data.len() as u64;

        // Write to disk if storage is configured
        if let Some(storage_path) = &self.storage_path {
            // Create session directory
            let session_dir = storage_path.join(&session_id);
            fs::create_dir_all(&session_dir)?;

            // Write object to Arrow IPC file
            let object_path = session_dir.join(format!("{}.arrow", object_id));
            let batch = object_to_batch(&object)
                .map_err(|e| FlameError::Internal(format!("Failed to create batch: {}", e)))?;

            write_batch_to_file(&object_path, &batch)?;
            tracing::debug!("Wrote object to disk: {:?}", object_path);

            // Clear any existing deltas (clean slate per HLD)
            self.clear_deltas(&key)?;
        }

        let meta = self.create_metadata(key.clone(), size, 0);

        // Update in-memory storage
        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key.clone(), object);
            metadata.insert(key.clone(), meta.clone());
        }

        // Track in eviction policy and run eviction if needed
        self.eviction_policy.on_add(&key, size);
        self.run_eviction()?;

        tracing::debug!("Object put: {}", key);

        Ok(meta)
    }

    fn load_object_from_disk(&self, key: &str) -> Result<Object, FlameError> {
        validate_key(key)?;

        let storage_path = self
            .storage_path
            .as_ref()
            .ok_or_else(|| FlameError::InvalidConfig("Storage path not configured".to_string()))?;

        let object_path = storage_path.join(format!("{}.arrow", key));
        self.load_object_from_disk_internal(&object_path)
    }

    /// Load all deltas for an object from disk.
    /// Returns an empty Vec if no deltas exist (not an error).
    /// Deltas are sorted by their numeric index to ensure correct ordering.
    fn load_deltas_from_disk(&self, key: &str) -> Result<Vec<Object>, FlameError> {
        let delta_dir = match self.get_delta_dir(key) {
            Some(dir) if dir.exists() => dir,
            _ => return Ok(Vec::new()),
        };

        let entries = match fs::read_dir(&delta_dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("Failed to read delta directory {:?}: {}", delta_dir, e);
                return Ok(Vec::new());
            }
        };

        // Collect and filter delta files
        let mut delta_files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("arrow"))
            .collect();

        // Sort by index (filename is {index}.arrow) to ensure correct ordering
        // Files that don't parse as u64 are sorted to the end
        delta_files.sort_by_key(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });

        let mut deltas = Vec::with_capacity(delta_files.len());
        for entry in delta_files {
            let path = entry.path();
            let file = fs::File::open(&path).map_err(|e| {
                FlameError::Internal(format!("Failed to open delta file {:?}: {}", path, e))
            })?;
            let reader = FileReader::try_new(file, None).map_err(|e| {
                FlameError::Internal(format!("Failed to create reader for {:?}: {}", path, e))
            })?;
            // SAFETY: Skip validation for trusted cache data written by this service.
            let reader = unsafe { reader.with_skip_validation(true) };

            let batch = reader
                .into_iter()
                .next()
                .ok_or_else(|| {
                    FlameError::Internal(format!("No batches in delta file {:?}", path))
                })?
                .map_err(|e| {
                    FlameError::Internal(format!(
                        "Failed to read delta batch from {:?}: {}",
                        path, e
                    ))
                })?;

            let delta = batch_to_object(&batch).map_err(|e| {
                FlameError::Internal(format!("Failed to parse delta from {:?}: {}", path, e))
            })?;
            deltas.push(delta);
        }

        Ok(deltas)
    }

    fn try_load_and_index(&self, key: &str) -> Result<Option<Object>, FlameError> {
        validate_key(key)?;

        let storage_path = match &self.storage_path {
            Some(path) => path,
            None => return Ok(None),
        };

        let object_path = storage_path.join(format!("{}.arrow", key));
        if !object_path.exists() {
            return Ok(None);
        }

        let object = self.load_object_from_disk_internal(&object_path)?;
        let size = object.data.len() as u64;
        let delta_count = self.count_deltas(key);

        // Add to in-memory storage
        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key.to_string(), object.clone());

            let meta = self.create_metadata(key.to_string(), size, delta_count);
            metadata.insert(key.to_string(), meta);
        }

        // Track in eviction policy
        self.eviction_policy.on_add(key, size);
        self.run_eviction()?;

        tracing::debug!("Loaded object from disk: {} (deltas: {})", key, delta_count);
        Ok(Some(object))
    }

    /// Get an object with all its deltas populated in the deltas field.
    /// Creates a new Object with deltas rather than mutating an existing one.
    async fn get(&self, key: String) -> Result<Object, FlameError> {
        validate_key(&key)?;

        self.eviction_policy.on_access(&key);

        // Check if object is in memory
        {
            let objects = lock_ptr!(self.objects)?;
            if let Some(object) = objects.get(&key) {
                tracing::debug!("Object get from memory: {}", key);
                // Load deltas and return object with deltas
                let deltas = self.load_deltas_from_disk(&key)?;
                return Ok(Object::with_deltas(
                    object.version,
                    object.data.clone(),
                    deltas,
                ));
            }
        }

        // Check if object exists in metadata (on disk but not in memory)
        let exists_in_metadata = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata.contains_key(&key)
        };

        if exists_in_metadata {
            // Object is on disk, reload into memory
            let object = self.load_object_from_disk(&key)?;
            let size = object.data.len() as u64;

            // Add back to memory
            {
                let mut objects = lock_ptr!(self.objects)?;
                objects.insert(key.clone(), object.clone());
            }

            // Track in eviction policy and run eviction
            self.eviction_policy.on_add(&key, size);
            self.run_eviction()?;

            // Load deltas and return
            let deltas = self.load_deltas_from_disk(&key)?;
            tracing::debug!(
                "Object reloaded from disk: {} (deltas: {})",
                key,
                deltas.len()
            );
            return Ok(Object::with_deltas(object.version, object.data, deltas));
        }

        // Try to load from disk (not in index)
        if let Some(base) = self.try_load_and_index(&key)? {
            let deltas = self.load_deltas_from_disk(&key)?;
            return Ok(Object::with_deltas(base.version, base.data, deltas));
        }

        Err(FlameError::NotFound(format!("object <{}> not found", key)))
    }

    async fn update(&self, key: String, new_object: Object) -> Result<ObjectMetadata, FlameError> {
        validate_key(&key)?;

        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 2 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid key format: {}",
                key
            )));
        }

        let size = new_object.data.len() as u64;

        // Write to disk if storage is configured
        if let Some(storage_path) = &self.storage_path {
            let object_path = storage_path.join(format!("{}.arrow", key));
            let batch = object_to_batch(&new_object)
                .map_err(|e| FlameError::Internal(format!("Failed to create batch: {}", e)))?;

            write_batch_to_file(&object_path, &batch)?;
            tracing::debug!("Updated object on disk: {:?}", object_path);

            // Clear all deltas per HLD
            self.clear_deltas(&key)?;
        }

        let meta = self.create_metadata(key.clone(), size, 0);

        // Update in-memory storage
        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key.clone(), new_object);
            metadata.insert(key.clone(), meta.clone());
        }

        // Track in eviction policy
        self.eviction_policy.on_add(&key, size);
        self.run_eviction()?;

        tracing::debug!("Object update: {}", key);

        Ok(meta)
    }

    /// Append a delta to an existing object (PATCH operation).
    ///
    /// # Errors
    /// - Returns NotFound if the base object doesn't exist
    /// - Returns InvalidConfig if storage path is not configured
    /// - Returns InvalidState if delta count exceeds MAX_DELTAS_PER_OBJECT
    async fn patch(&self, key: String, delta: Object) -> Result<ObjectMetadata, FlameError> {
        validate_key(&key)?;

        self.eviction_policy.on_access(&key);

        let storage_path = self
            .storage_path
            .as_ref()
            .ok_or_else(|| FlameError::InvalidConfig("Storage path not configured".to_string()))?;

        // Reserve the next delta index atomically under the lock
        let next_index = {
            let mut metadata = lock_ptr!(self.metadata)?;
            let current_delta_count = match metadata.get(&key) {
                Some(meta) => meta.delta_count,
                None => {
                    drop(metadata);
                    if self.try_load_and_index(&key)?.is_none() {
                        return Err(FlameError::NotFound(format!(
                            "object <{}> not found, must put first",
                            key
                        )));
                    }
                    metadata = lock_ptr!(self.metadata)?;
                    metadata.get(&key).map(|m| m.delta_count).unwrap_or(0)
                }
            };

            if current_delta_count >= MAX_DELTAS_PER_OBJECT {
                return Err(FlameError::InvalidState(format!(
                    "object <{}> has reached maximum delta count ({}). Use update_object to compact deltas.",
                    key, MAX_DELTAS_PER_OBJECT
                )));
            }

            // Reserve the index by incrementing delta_count now
            if let Some(meta) = metadata.get_mut(&key) {
                meta.delta_count = current_delta_count + 1;
            }
            current_delta_count
        };

        // Write delta file outside the lock (I/O can be slow)
        let delta_dir = storage_path.join(format!("{}.deltas", key));
        fs::create_dir_all(&delta_dir)?;

        let delta_path = delta_dir.join(format!("{}.arrow", next_index));
        let batch = object_to_batch(&delta)
            .map_err(|e| FlameError::Internal(format!("Failed to create delta batch: {}", e)))?;

        if let Err(e) = write_batch_to_file(&delta_path, &batch) {
            // Rollback: decrement delta_count on write failure
            let mut metadata = lock_ptr!(self.metadata)?;
            if let Some(meta) = metadata.get_mut(&key) {
                meta.delta_count = meta.delta_count.saturating_sub(1);
            }
            return Err(e);
        }

        tracing::debug!("Wrote delta {} to disk: {:?}", next_index, delta_path);

        let metadata = lock_ptr!(self.metadata)?;
        let updated = metadata
            .get(&key)
            .cloned()
            .ok_or_else(|| FlameError::Internal("Failed to get metadata".to_string()))?;

        tracing::debug!(
            "Object patch: {} (delta_count: {})",
            key,
            updated.delta_count
        );
        Ok(updated)
    }

    async fn delete(&self, session_id: SessionID) -> Result<(), FlameError> {
        validate_session_id(&session_id)?;

        let keys_to_remove: Vec<String> = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata
                .keys()
                .filter(|k| k.starts_with(&format!("{}/", session_id)))
                .cloned()
                .collect()
        };

        for key in &keys_to_remove {
            self.eviction_policy.on_remove(key);
            // Also clear deltas for each object
            self.clear_deltas(key)?;
        }

        // Delete session directory and all objects (including deltas)
        if let Some(storage_path) = &self.storage_path {
            let session_dir = storage_path.join(&session_id);
            if session_dir.exists() {
                fs::remove_dir_all(&session_dir)?;
                tracing::debug!("Deleted session directory: {:?}", session_dir);
            }
        }

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.retain(|key, _| !key.starts_with(&format!("{}/", session_id)));
            metadata.retain(|key, _| !key.starts_with(&format!("{}/", session_id)));
        }

        tracing::debug!("Session deleted: <{}>", session_id);

        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError> {
        let metadata = lock_ptr!(self.metadata)?;
        Ok(metadata.values().cloned().collect())
    }
}

pub struct FlightCacheServer {
    cache: Arc<ObjectCache>,
}

impl FlightCacheServer {
    pub fn new(cache: Arc<ObjectCache>) -> Self {
        Self { cache }
    }

    fn extract_session_and_object_id(
        flight_data: &FlightData,
        session_id: &mut Option<String>,
        object_id: &mut Option<String>,
    ) {
        if session_id.is_some() {
            return;
        }

        if let Some(ref desc) = flight_data.flight_descriptor {
            if !desc.path.is_empty() {
                let path_str = &desc.path[0];
                if path_str.contains('/') {
                    let parts: Vec<&str> = path_str.split('/').collect();
                    if parts.len() == 2 {
                        *session_id = Some(parts[0].to_string());
                        *object_id = Some(parts[1].to_string());
                    }
                } else {
                    *session_id = Some(path_str.clone());
                }
            }
        }
    }

    fn extract_schema_from_flight_data(
        flight_data: &FlightData,
    ) -> Result<Arc<Schema>, FlameError> {
        use arrow::ipc::root_as_message;

        let message = root_as_message(&flight_data.data_header)
            .map_err(|e| FlameError::Internal(format!("Failed to parse IPC message: {}", e)))?;

        let ipc_schema = message
            .header_as_schema()
            .ok_or_else(|| FlameError::Internal("Message is not a schema".to_string()))?;

        let decoded_schema = arrow::ipc::convert::fb_to_schema(ipc_schema);
        Ok(Arc::new(decoded_schema))
    }

    fn decode_batch_from_flight_data(
        flight_data: &FlightData,
        schema: &Arc<Schema>,
    ) -> Result<RecordBatch, FlameError> {
        arrow_flight::utils::flight_data_to_arrow_batch(
            flight_data,
            schema.clone(),
            &Default::default(),
        )
        .map_err(|e| FlameError::Internal(format!("Failed to decode batch: {}", e)))
    }

    async fn collect_batches_from_stream(
        mut stream: Streaming<FlightData>,
    ) -> Result<(String, Option<String>, Vec<RecordBatch>), FlameError> {
        let mut batches = Vec::new();
        let mut session_id: Option<String> = None;
        let mut object_id: Option<String> = None;
        let mut schema: Option<Arc<Schema>> = None;

        while let Some(flight_data) = stream
            .message()
            .await
            .map_err(|e| FlameError::Internal(format!("Stream error: {}", e)))?
        {
            Self::extract_session_and_object_id(&flight_data, &mut session_id, &mut object_id);

            // Extract schema from data_header in first message
            if schema.is_none() && !flight_data.data_header.is_empty() {
                schema = Some(Self::extract_schema_from_flight_data(&flight_data)?);
            }

            // Decode batch if we have schema and data_body
            if let Some(ref schema_ref) = schema {
                if !flight_data.data_body.is_empty() {
                    let batch = Self::decode_batch_from_flight_data(&flight_data, schema_ref)?;
                    batches.push(batch);
                }
            }
        }

        if batches.is_empty() {
            return Err(FlameError::InvalidState("No data received".to_string()));
        }

        let session_id = session_id.ok_or_else(|| {
            FlameError::InvalidState(
                "session_id must be provided in app_metadata as 'session_id:{id}'".to_string(),
            )
        })?;

        Ok((session_id, object_id, batches))
    }

    fn combine_batches(batches: Vec<RecordBatch>) -> Result<RecordBatch, FlameError> {
        if batches.len() == 1 {
            Ok(batches.into_iter().next().unwrap())
        } else {
            let schema = batches[0].schema();
            concat_batches(&schema, &batches)
                .map_err(|e| FlameError::Internal(format!("Failed to concatenate batches: {}", e)))
        }
    }

    fn create_put_result(metadata: &ObjectMetadata) -> Result<PutResult, FlameError> {
        let object_ref = bson::doc! {
            "endpoint": &metadata.endpoint,
            "key": &metadata.key,
            "version": metadata.version as i64,
        };

        let mut bson_bytes = Vec::new();
        object_ref.to_writer(&mut bson_bytes).map_err(|e| {
            FlameError::Internal(format!("Failed to serialize ObjectRef to BSON: {}", e))
        })?;

        Ok(PutResult {
            app_metadata: Bytes::from(bson_bytes),
        })
    }

    async fn handle_put_action(&self, action_body: &str) -> Result<String, FlameError> {
        let (session_id_str, data_b64) = action_body
            .split_once(':')
            .ok_or_else(|| FlameError::InvalidState("Invalid PUT action format".to_string()))?;

        let session_id = session_id_str.to_string();
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| FlameError::InvalidState(format!("Invalid base64: {}", e)))?;

        let object = Object::new(0, data);
        let metadata = self.cache.put(session_id, object).await?;

        serde_json::to_string(&metadata)
            .map_err(|e| FlameError::Internal(format!("Failed to serialize: {}", e)))
    }

    async fn handle_update_action(&self, action_body: &str) -> Result<String, FlameError> {
        let (key_str, data_b64) = action_body
            .split_once(':')
            .ok_or_else(|| FlameError::InvalidState("Invalid UPDATE action format".to_string()))?;

        let key = key_str.to_string();
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| FlameError::InvalidState(format!("Invalid base64: {}", e)))?;

        let object = Object::new(0, data);
        let metadata = self.cache.update(key, object).await?;

        serde_json::to_string(&metadata)
            .map_err(|e| FlameError::Internal(format!("Failed to serialize: {}", e)))
    }

    async fn handle_delete_action(&self, session_id: String) -> Result<String, FlameError> {
        self.cache.delete(session_id).await?;
        Ok("OK".to_string())
    }

    /// Handle PATCH action: append delta to an existing object
    async fn handle_patch_action(&self, action_body: &str) -> Result<String, FlameError> {
        let (key_str, data_b64) = action_body
            .split_once(':')
            .ok_or_else(|| FlameError::InvalidState("Invalid PATCH action format".to_string()))?;

        let key = key_str.to_string();
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| FlameError::InvalidState(format!("Invalid base64: {}", e)))?;

        let delta = Object::new(0, data);
        let metadata = self.cache.patch(key, delta).await?;

        serde_json::to_string(&metadata)
            .map_err(|e| FlameError::Internal(format!("Failed to serialize: {}", e)))
    }
}

// Helper function to encode schema to IPC format for FlightInfo
fn encode_schema(schema: &Schema) -> Result<Vec<u8>, FlameError> {
    // Encode schema as IPC message using IpcDataGenerator
    use arrow::ipc::writer::{DictionaryTracker, IpcDataGenerator, IpcWriteOptions};

    let options = IpcWriteOptions::default();
    let data_gen = IpcDataGenerator::default();
    let mut dict_tracker = DictionaryTracker::new(false);

    // Encode the schema
    let encoded =
        data_gen.schema_to_bytes_with_dictionary_tracker(schema, &mut dict_tracker, &options);

    Ok(encoded.ipc_message)
}

fn get_object_schema() -> Schema {
    Schema::new(vec![
        Field::new("version", DataType::UInt64, false),
        Field::new("data", DataType::Binary, false),
    ])
}

// Helper function to write a batch to an Arrow IPC file
fn write_batch_to_file(path: &Path, batch: &RecordBatch) -> Result<(), FlameError> {
    let file = fs::File::create(path)?;
    let mut writer = FileWriter::try_new(file, &batch.schema())
        .map_err(|e| FlameError::Internal(format!("Failed to create writer: {}", e)))?;
    writer
        .write(batch)
        .map_err(|e| FlameError::Internal(format!("Failed to write batch: {}", e)))?;
    writer
        .finish()
        .map_err(|e| FlameError::Internal(format!("Failed to finish writer: {}", e)))?;
    Ok(())
}

// Helper function to create a RecordBatch from object data
// Note: Only serializes version and data; deltas are stored separately
fn object_to_batch(object: &Object) -> Result<RecordBatch, FlameError> {
    let schema = get_object_schema();

    let version_array = UInt64Array::from(vec![object.version]);
    let data_array = BinaryArray::from(vec![object.data.as_slice()]);

    RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(version_array), Arc::new(data_array)],
    )
    .map_err(|e| FlameError::Internal(format!("Failed to create RecordBatch: {}", e)))
}

// Helper function to extract data from RecordBatch
// Note: Returns Object with empty deltas; caller populates deltas separately
fn batch_to_object(batch: &RecordBatch) -> Result<Object, FlameError> {
    if batch.num_rows() != 1 {
        return Err(FlameError::InvalidState(
            "Expected exactly one row".to_string(),
        ));
    }

    let version_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| FlameError::Internal("Invalid version column".to_string()))?;
    let data_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .ok_or_else(|| FlameError::Internal("Invalid data column".to_string()))?;

    let version = version_col.value(0);
    let data = data_col.value(0).to_vec();

    Ok(Object::new(version, data))
}

/// Convert Object (with deltas) to FlightData stream
/// Sends schema once, followed by base batch, then delta batches
/// Uses ZSTD compression for ~54% faster encoding (Arrow 58+)
fn object_to_flight_data_vec(obj: &Object) -> Result<Vec<FlightData>, FlameError> {
    let options = IpcWriteOptions::default()
        .try_with_compression(Some(CompressionType::ZSTD))
        .map_err(|e| FlameError::Internal(format!("Failed to set compression: {}", e)))?;

    let data_gen = IpcDataGenerator::default();
    let mut dict_tracker = DictionaryTracker::new(false);
    let mut compression_ctx = CompressionContext::default();

    let base_batch = object_to_batch(obj)?;
    let schema = base_batch.schema();

    let mut all_flight_data = Vec::new();

    let encoded_schema = data_gen.schema_to_bytes_with_dictionary_tracker(
        schema.as_ref(),
        &mut dict_tracker,
        &options,
    );
    all_flight_data.push(FlightData {
        flight_descriptor: None,
        app_metadata: vec![].into(),
        data_header: encoded_schema.ipc_message.into(),
        data_body: vec![].into(),
    });

    let (encoded_dicts, encoded_batch) = data_gen
        .encode(
            &base_batch,
            &mut dict_tracker,
            &options,
            &mut compression_ctx,
        )
        .map_err(|e| FlameError::Internal(format!("Failed to encode base batch: {}", e)))?;
    for dict_batch in encoded_dicts {
        all_flight_data.push(dict_batch.into());
    }
    all_flight_data.push(encoded_batch.into());

    for delta in &obj.deltas {
        let delta_batch = object_to_batch(delta)?;
        let (encoded_dicts, encoded_batch) = data_gen
            .encode(
                &delta_batch,
                &mut dict_tracker,
                &options,
                &mut compression_ctx,
            )
            .map_err(|e| FlameError::Internal(format!("Failed to encode delta batch: {}", e)))?;
        for dict_batch in encoded_dicts {
            all_flight_data.push(dict_batch.into());
        }
        all_flight_data.push(encoded_batch.into());
    }

    Ok(all_flight_data)
}

#[async_trait]
impl FlightService for FlightCacheServer {
    type HandshakeStream = Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>;
    type ListFlightsStream = Pin<Box<dyn Stream<Item = Result<FlightInfo, Status>> + Send>>;
    type DoGetStream = Pin<Box<dyn Stream<Item = Result<FlightData, Status>> + Send>>;
    type DoPutStream = Pin<Box<dyn Stream<Item = Result<PutResult, Status>> + Send>>;
    type DoActionStream = Pin<Box<dyn Stream<Item = Result<FlightResult, Status>> + Send>>;
    type ListActionsStream = Pin<Box<dyn Stream<Item = Result<ActionType, Status>> + Send>>;
    type DoExchangeStream = Pin<Box<dyn Stream<Item = Result<FlightData, Status>> + Send>>;

    async fn get_flight_info(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let descriptor = request.into_inner();

        // Extract key from descriptor path
        let key = if !descriptor.path.is_empty() {
            descriptor.path.join("/")
        } else {
            return Err(Status::invalid_argument("Empty descriptor path"));
        };

        // Key format: "session_id/object_id"

        // Create endpoint with cache server's public endpoint
        let endpoint_uri = self.cache.endpoint.to_uri();

        let ticket = Ticket {
            ticket: Bytes::from(key.as_bytes().to_vec()),
        };

        let endpoint = FlightEndpoint {
            ticket: Some(ticket),
            location: vec![Location { uri: endpoint_uri }],
            expiration_time: None,
            app_metadata: Bytes::new(),
        };

        // Return empty schema - schema will be discovered from FlightData
        // This avoids compatibility issues with schema encoding between Rust and Python
        let flight_info = FlightInfo {
            schema: Bytes::new(),
            flight_descriptor: Some(FlightDescriptor {
                r#type: descriptor.r#type,
                cmd: descriptor.cmd,
                path: vec![key.clone()],
            }),
            endpoint: vec![endpoint],
            total_records: -1,
            total_bytes: -1,
            ordered: false,
            app_metadata: Bytes::new(),
        };

        Ok(Response::new(flight_info))
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        let ticket = request.into_inner();
        let key = String::from_utf8(ticket.ticket.to_vec())
            .map_err(|e| Status::invalid_argument(format!("Invalid ticket: {}", e)))?;

        // Key format: "session_id/object_id"
        // Returns Object with base data and all deltas populated per HLD
        let object = self.cache.get(key.clone()).await?;

        tracing::debug!(
            "do_get: key={}, base_size={}, delta_count={}",
            key,
            object.data.len(),
            object.deltas.len()
        );

        let flight_data_vec = object_to_flight_data_vec(&object)?;
        tracing::debug!(
            "do_get: generated {} FlightData messages",
            flight_data_vec.len()
        );

        let stream = futures::stream::iter(flight_data_vec.into_iter().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn do_put(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        let stream = request.into_inner();

        let (session_id, object_id, batches) = Self::collect_batches_from_stream(stream).await?;
        let combined_batch = Self::combine_batches(batches)?;
        let object = batch_to_object(&combined_batch)?;

        let metadata = self
            .cache
            .put_with_id(session_id, object_id, object)
            .await?;

        let result = Self::create_put_result(&metadata)?;

        tracing::debug!("do_put: sending PutResult with key: {}", metadata.key);
        let stream = futures::stream::iter(vec![Ok(result)]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        let action = request.into_inner();
        let action_type = action.r#type;
        let action_body = String::from_utf8(action.body.to_vec())
            .map_err(|e| Status::invalid_argument(format!("Invalid action body: {}", e)))?;

        let result = match action_type.as_str() {
            "PUT" => self.handle_put_action(&action_body).await?,
            "UPDATE" => self.handle_update_action(&action_body).await?,
            "DELETE" => self.handle_delete_action(action_body).await?,
            "PATCH" => self.handle_patch_action(&action_body).await?,
            _ => {
                return Err(Status::invalid_argument(format!(
                    "Unknown action type: {}",
                    action_type
                )))
            }
        };

        let flight_result = FlightResult {
            body: Bytes::from(result.into_bytes()),
        };

        let stream = futures::stream::iter(vec![Ok(flight_result)]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_actions(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        let actions = vec![
            ActionType {
                r#type: "PUT".to_string(),
                description: "Put an object into cache".to_string(),
            },
            ActionType {
                r#type: "UPDATE".to_string(),
                description: "Update an existing object (replaces base and clears deltas)"
                    .to_string(),
            },
            ActionType {
                r#type: "DELETE".to_string(),
                description: "Delete a session and all its objects".to_string(),
            },
            ActionType {
                r#type: "PATCH".to_string(),
                description: "Append delta data to an existing object".to_string(),
            },
        ];

        let stream = futures::stream::iter(actions.into_iter().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_schema(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        let schema = get_object_schema();

        let schema_result = SchemaResult {
            schema: Bytes::from(encode_schema(&schema)?),
        };

        Ok(Response::new(schema_result))
    }

    async fn handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        Err(Status::unimplemented("Handshake not implemented"))
    }

    #[allow(clippy::result_large_err)]
    async fn list_flights(
        &self,
        _request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        let all_objects = self
            .cache
            .list_all()
            .await
            .map_err(|e| Status::internal(format!("Failed to list objects: {}", e)))?;

        let flight_infos: Vec<Result<FlightInfo, Status>> = all_objects
            .into_iter()
            .map(|metadata| {
                let ticket = Ticket {
                    ticket: Bytes::from(metadata.key.as_bytes().to_vec()),
                };

                let endpoint = FlightEndpoint {
                    ticket: Some(ticket),
                    location: vec![Location {
                        uri: metadata.endpoint.clone(),
                    }],
                    expiration_time: None,
                    app_metadata: Bytes::new(),
                };

                // Return empty schema - schema will be discovered from FlightData
                let flight_info = FlightInfo {
                    schema: Bytes::new(),
                    flight_descriptor: None,
                    endpoint: vec![endpoint],
                    total_records: -1,
                    total_bytes: metadata.size as i64,
                    ordered: false,
                    app_metadata: Bytes::new(),
                };

                Ok(flight_info)
            })
            .collect();

        let stream = futures::stream::iter(flight_infos);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("Do exchange not implemented"))
    }

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<arrow_flight::PollInfo>, Status> {
        Err(Status::unimplemented("Poll flight info not implemented"))
    }
}

/// Run the object cache server.
///
/// # Arguments
/// * `cache_config` - Cache configuration (includes optional TLS config)
pub async fn run(cache_config: &FlameCache) -> Result<(), FlameError> {
    let endpoint = CacheEndpoint::try_from(cache_config)?;
    let address_str = format!("{}:{}", endpoint.host, endpoint.port);

    // Get storage path from config or environment variable
    let storage_path = if let Some(ref path) = cache_config.storage {
        Some(PathBuf::from(path))
    } else if let Ok(path) = std::env::var("FLAME_CACHE_STORAGE") {
        Some(PathBuf::from(path))
    } else {
        None
    };

    if let Some(ref path) = storage_path {
        tracing::info!("Using storage path: {:?}", path);
    } else {
        tracing::warn!("No storage path configured - cache will not persist");
    }

    // Create eviction config from FlameCache.eviction
    // Environment variables can still override config values (handled in new_policy)
    let eviction_config = EvictionConfig {
        policy: Some(cache_config.eviction.policy.clone()),
        max_memory: Some(ByteSize::b(cache_config.eviction.max_memory).to_string()),
        max_objects: cache_config.eviction.max_objects,
    };

    tracing::info!(
        "Eviction config: policy={}, max_memory={}, max_objects={:?}",
        cache_config.eviction.policy,
        ByteSize::b(cache_config.eviction.max_memory),
        cache_config.eviction.max_objects
    );

    let cache = Arc::new(ObjectCache::new(
        endpoint.clone(),
        storage_path,
        Some(&eviction_config),
    )?);
    let server = FlightCacheServer::new(Arc::clone(&cache));

    tracing::info!("Starting Arrow Flight cache server at {}", address_str);

    let addr = address_str
        .parse()
        .map_err(|e| FlameError::InvalidConfig(format!("Invalid address: {}", e)))?;

    let mut builder = tonic::transport::Server::builder();

    // Apply TLS if cache endpoint requires it (grpcs:// scheme) and TLS is configured
    if cache_config.requires_tls() {
        let tls_config = cache_config.tls.as_ref().ok_or_else(|| {
            FlameError::InvalidConfig(
                "cache endpoint uses grpcs:// but cache.tls is not configured".to_string(),
            )
        })?;

        let tls = tls_config.server_tls_config()?;
        builder = builder
            .tls_config(tls)
            .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;

        tracing::info!("TLS enabled for object cache");
    }

    builder
        .add_service(FlightServiceServer::new(server))
        .serve(addr)
        .await
        .map_err(|e| FlameError::Internal(format!("Server error: {}", e)))?;

    Ok(())
}

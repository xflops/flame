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
use std::pin::Pin;
use std::sync::Arc;

use arrow::array::{BinaryArray, RecordBatch, UInt64Array};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::{
    CompressionContext, DictionaryTracker, IpcDataGenerator, IpcWriteOptions,
};
use arrow::ipc::CompressionType;
use arrow_flight::{
    flight_service_server::{FlightService, FlightServiceServer},
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, Location, PutResult, Result as FlightResult, SchemaResult,
    Ticket,
};
use async_trait::async_trait;
use bytes::Bytes;
use bytesize::ByteSize;
use futures::Stream;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use regex::Regex;
use stdng::{lock_ptr, new_ptr, MutexPtr};
use tonic::{Request, Response, Status, Streaming};
use url::Url;

use common::ctx::FlameCache;
use common::FlameError;

use crate::eviction::{new_policy, EvictionConfig, EvictionPolicyPtr};

/// Default batch size for eviction operations
const EVICTION_BATCH_SIZE: usize = 10;

/// Wildcard session identifier for matching all sessions of an application
pub const WILDCARD_SESSION: &str = "*";

/// Parsed object key: `<app_name>/<session_id>/<object_id>`
/// session_id can be "*" for wildcard (all sessions), requires object_id to be None
#[derive(Debug, Clone)]
pub struct ObjectKey {
    pub app_name: String,
    pub session_id: String,
    pub object_id: Option<String>,
}

impl ObjectKey {
    /// Parse from path string.
    ///
    /// Wildcard '*' handling:
    /// - Only allowed for session_id (e.g., "app/*" for delete all sessions)
    /// - Not allowed for app_name or object_id
    /// - Wildcard session cannot have object_id (e.g., "app/*/obj" is invalid)
    pub fn from_path(path_str: &str) -> Result<Self, FlameError> {
        let parts: Vec<&str> = path_str.split('/').collect();

        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid key component: '{}'",
                    part
                )));
            }
            // Wildcard only allowed at index 1 (session_id position)
            if *part == WILDCARD_SESSION && i != 1 {
                return Err(FlameError::InvalidConfig(
                    "Wildcard '*' only allowed for session_id".to_string(),
                ));
            }
        }

        match parts.len() {
            2 => Ok(ObjectKey {
                app_name: parts[0].to_string(),
                session_id: parts[1].to_string(),
                object_id: None,
            }),
            3 => {
                // Wildcard session cannot reference specific objects
                if parts[1] == WILDCARD_SESSION {
                    return Err(FlameError::InvalidConfig(
                        "Wildcard session '*' cannot have object_id".to_string(),
                    ));
                }
                // Object ID cannot be wildcard
                if parts[2] == WILDCARD_SESSION {
                    return Err(FlameError::InvalidConfig(
                        "Wildcard '*' not allowed for object_id".to_string(),
                    ));
                }
                Ok(ObjectKey {
                    app_name: parts[0].to_string(),
                    session_id: parts[1].to_string(),
                    object_id: Some(parts[2].to_string()),
                })
            }
            _ => Err(FlameError::InvalidConfig(format!(
                "Invalid path '{}': expected '<app>/<ssn>' or '<app>/<ssn>/<uuid>'",
                path_str
            ))),
        }
    }

    pub fn is_all_sessions(&self) -> bool {
        self.session_id == WILDCARD_SESSION
    }

    pub fn to_key(&self) -> Option<String> {
        if self.is_all_sessions() {
            return None;
        }
        self.object_id
            .as_ref()
            .map(|oid| format!("{}/{}/{}", self.app_name, self.session_id, oid))
    }

    pub fn to_prefix(&self) -> String {
        if self.is_all_sessions() {
            self.app_name.clone()
        } else {
            format!("{}/{}", self.app_name, self.session_id)
        }
    }

    pub fn matches(&self, key_str: &str) -> bool {
        if self.is_all_sessions() {
            key_str.starts_with(&format!("{}/", self.app_name))
        } else {
            key_str.starts_with(&format!("{}/", self.to_prefix()))
        }
    }

    pub fn with_generated_id(self) -> Self {
        Self {
            object_id: Some(uuid::Uuid::new_v4().to_string()),
            ..self
        }
    }

    pub fn with_object_id(self, id: String) -> Result<Self, FlameError> {
        if self.is_all_sessions() {
            return Err(FlameError::InvalidConfig(
                "Wildcard session '*' cannot have object_id".to_string(),
            ));
        }
        if id.is_empty() || id.contains("..") || id.contains('\\') || id.contains('/') {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid object_id: '{}'",
                id
            )));
        }
        Ok(Self {
            object_id: Some(id),
            ..self
        })
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = FlameError;

    fn try_from(key: &str) -> Result<Self, Self::Error> {
        let parts: Vec<&str> = key.split('/').collect();

        if parts.len() != 3 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid key '{}': expected '<app>/<ssn>/<uuid>'",
                key
            )));
        }

        for part in &parts {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid key component: '{}'",
                    part
                )));
            }
        }

        Ok(ObjectKey {
            app_name: parts[0].to_string(),
            session_id: parts[1].to_string(),
            object_id: Some(parts[2].to_string()),
        })
    }
}

impl From<&ObjectKey> for String {
    fn from(key: &ObjectKey) -> Self {
        key.to_key().unwrap_or_else(|| key.to_prefix())
    }
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
    storage: crate::storage::StorageEnginePtr,
    objects: MutexPtr<HashMap<String, Object>>,
    metadata: MutexPtr<HashMap<String, ObjectMetadata>>,
    eviction_policy: EvictionPolicyPtr,
    /// Per-key write locks to prevent concurrent PUT/PATCH race conditions
    write_locks: MutexPtr<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ObjectCache {
    fn new(
        endpoint: CacheEndpoint,
        storage: crate::storage::StorageEnginePtr,
        eviction_config: Option<&EvictionConfig>,
    ) -> Result<Self, FlameError> {
        let eviction_policy = new_policy(eviction_config);

        Ok(Self {
            endpoint,
            storage,
            objects: new_ptr(HashMap::new()),
            metadata: new_ptr(HashMap::new()),
            eviction_policy,
            write_locks: new_ptr(HashMap::new()),
        })
    }

    async fn load_from_storage(&self) -> Result<(), FlameError> {
        let items = self.storage.load_objects().await?;

        let mut objects = lock_ptr!(self.objects)?;
        let mut metadata = lock_ptr!(self.metadata)?;

        for (key, object) in items {
            let key_str = key.to_key().expect("loaded key must have object_id");
            let size = object.data.len() as u64;
            let version = object.version;
            let delta_count = object.deltas.len() as u64;
            let meta = self.create_metadata(key_str.clone(), version, size, delta_count);

            objects.insert(key_str.clone(), object);
            metadata.insert(key_str.clone(), meta);

            self.eviction_policy.on_add(&key_str, size);
        }

        drop(objects);
        drop(metadata);
        self.run_eviction()?;

        Ok(())
    }

    fn get_write_lock(&self, key: &str) -> Result<Arc<tokio::sync::Mutex<()>>, FlameError> {
        let mut locks = lock_ptr!(self.write_locks)?;
        Ok(locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone())
    }

    fn create_metadata(
        &self,
        key: String,
        version: u64,
        size: u64,
        delta_count: u64,
    ) -> ObjectMetadata {
        ObjectMetadata {
            endpoint: self.endpoint.to_uri(),
            key,
            version,
            size,
            delta_count,
        }
    }

    fn run_eviction(&self) -> Result<(), FlameError> {
        loop {
            let keys_to_evict = self.eviction_policy.victims(EVICTION_BATCH_SIZE);
            if keys_to_evict.is_empty() {
                break;
            }

            let mut objects = lock_ptr!(self.objects)?;
            for key in keys_to_evict {
                if objects.contains_key(&key) {
                    objects.remove(&key);
                    self.eviction_policy.on_evict(&key);
                    tracing::debug!("Evicted object from memory: {}", key);
                }
            }
        }
        Ok(())
    }

    async fn put(&self, key: ObjectKey, object: Object) -> Result<ObjectMetadata, FlameError> {
        let key = match key.object_id {
            Some(_) => key,
            None => key.with_generated_id(),
        };

        let key_str = key.to_key().ok_or_else(|| {
            FlameError::Internal("ObjectKey missing object_id in put".to_string())
        })?;
        let size = object.data.len() as u64;

        // Acquire per-key lock to prevent concurrent version increments
        let write_lock = self.get_write_lock(&key_str)?;
        let _guard = write_lock.lock().await;

        let version_from_memory = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata.get(&key_str).map(|m| m.version)
        };

        let current_version = match version_from_memory {
            Some(v) => v,
            None => self
                .storage
                .read_object(&key)
                .await?
                .map(|obj| obj.version)
                .unwrap_or(0),
        };
        let new_version = current_version + 1;

        let versioned_object = Object::new(new_version, object.data);

        self.storage.write_object(&key, &versioned_object).await?;

        let meta = self.create_metadata(key_str.clone(), new_version, size, 0);

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key_str.clone(), versioned_object);
            metadata.insert(key_str.clone(), meta.clone());
        }

        self.eviction_policy.on_add(&key_str, size);
        self.run_eviction()?;

        tracing::debug!("Object put: {} (version={})", key_str, new_version);

        Ok(meta)
    }

    async fn get(&self, key: &ObjectKey) -> Result<Object, FlameError> {
        let key_str = key.to_key().ok_or_else(|| {
            FlameError::InvalidConfig("ObjectKey requires object_id for get".to_string())
        })?;

        self.eviction_policy.on_access(&key_str);

        {
            let objects = lock_ptr!(self.objects)?;
            if let Some(object) = objects.get(&key_str) {
                tracing::debug!("Object get from memory: {}", key_str);
                return Ok(object.clone());
            }
        }

        if let Some(object) = self.storage.read_object(key).await? {
            let size = object.data.len() as u64;
            let delta_count = object.deltas.len() as u64;
            let version = object.version;

            {
                let mut objects = lock_ptr!(self.objects)?;
                let mut metadata = lock_ptr!(self.metadata)?;

                objects.insert(key_str.clone(), object.clone());

                let meta = self.create_metadata(key_str.clone(), version, size, delta_count);
                metadata.insert(key_str.clone(), meta);
            }

            self.eviction_policy.on_add(&key_str, size);
            self.run_eviction()?;

            tracing::debug!("Object loaded from storage: {}", key_str);
            return Ok(object);
        }

        Err(FlameError::NotFound(format!(
            "object <{}> not found",
            key_str
        )))
    }

    async fn patch(&self, key: &ObjectKey, delta: Object) -> Result<ObjectMetadata, FlameError> {
        let key_str = key.to_key().ok_or_else(|| {
            FlameError::InvalidConfig("ObjectKey requires object_id for patch".to_string())
        })?;

        // Acquire per-key lock to prevent concurrent version increments
        let write_lock = self.get_write_lock(&key_str)?;
        let _guard = write_lock.lock().await;

        self.eviction_policy.on_access(&key_str);

        let version_from_memory = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata.get(&key_str).map(|m| m.version)
        };

        let current_version = match version_from_memory {
            Some(v) => v,
            None => self
                .storage
                .read_object(key)
                .await?
                .map(|obj| obj.version)
                .ok_or_else(|| {
                    FlameError::NotFound(format!("object <{}> not found for patch", key_str))
                })?,
        };
        let new_version = current_version + 1;

        let mut meta = self.storage.patch_object(key, &delta).await?;
        meta.endpoint = self.endpoint.to_uri();
        meta.version = new_version;

        {
            let mut metadata = lock_ptr!(self.metadata)?;
            metadata.insert(key_str.clone(), meta.clone());
        }

        {
            let mut objects = lock_ptr!(self.objects)?;
            if objects.remove(&key_str).is_some() {
                self.eviction_policy.on_remove(&key_str);
            }
        }

        tracing::debug!(
            "Object patch: {} (version={}, delta_count={})",
            key_str,
            new_version,
            meta.delta_count
        );
        Ok(meta)
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let keys_to_remove: Vec<String> = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata
                .keys()
                .filter(|k| key.matches(k))
                .cloned()
                .collect()
        };

        for k in &keys_to_remove {
            self.eviction_policy.on_remove(k);
        }

        self.storage.delete_objects(key).await?;

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.retain(|k, _| !key.matches(k));
            metadata.retain(|k, _| !key.matches(k));
        }

        tracing::debug!("Deleted: <{}>", key.to_prefix());

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
                match ObjectKey::from_path(path_str) {
                    Ok(key) => {
                        *session_id = Some(key.to_prefix());
                        *object_id = key.object_id;
                    }
                    Err(_) => {
                        *session_id = Some(path_str.clone());
                    }
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
    ) -> Result<(String, Option<String>, Option<String>, Vec<RecordBatch>), FlameError> {
        let mut batches = Vec::new();
        let mut session_id: Option<String> = None;
        let mut object_id: Option<String> = None;
        let mut command: Option<String> = None;
        let mut schema: Option<Arc<Schema>> = None;

        while let Some(flight_data) = stream
            .message()
            .await
            .map_err(|e| FlameError::Internal(format!("Stream error: {}", e)))?
        {
            Self::extract_session_and_object_id(&flight_data, &mut session_id, &mut object_id);

            if command.is_none() {
                if let Some(ref desc) = flight_data.flight_descriptor {
                    if !desc.cmd.is_empty() {
                        if let Ok(cmd_str) = String::from_utf8(desc.cmd.to_vec()) {
                            if let Some(key) = cmd_str.strip_prefix("PATCH:") {
                                command = Some("PATCH".to_string());
                                if session_id.is_none() {
                                    match ObjectKey::from_path(key) {
                                        Ok(parsed) => {
                                            session_id = Some(parsed.to_prefix());
                                            object_id = parsed.object_id;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to parse PATCH key '{}': {}",
                                                key,
                                                e
                                            );
                                            return Err(FlameError::InvalidConfig(format!(
                                                "Invalid PATCH key '{}': {}",
                                                key, e
                                            )));
                                        }
                                    }
                                }
                            } else {
                                command = Some(cmd_str);
                            }
                        }
                    }
                }
            }

            if schema.is_none() && !flight_data.data_header.is_empty() {
                schema = Some(Self::extract_schema_from_flight_data(&flight_data)?);
            }

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
                "key must be provided in descriptor path or command".to_string(),
            )
        })?;

        Ok((session_id, object_id, command, batches))
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

    async fn handle_delete_action(&self, key_prefix: String) -> Result<String, FlameError> {
        tracing::debug!(
            "handle_delete_action: deleting objects with prefix={}",
            key_prefix
        );
        let key = ObjectKey::from_path(&key_prefix)?;
        self.cache.delete(&key).await?;
        tracing::debug!(
            "handle_delete_action: deleted objects with prefix={}",
            key_prefix
        );
        Ok("OK".to_string())
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

fn create_empty_flight_data() -> Result<Vec<FlightData>, FlameError> {
    let schema = get_object_schema();
    let options = IpcWriteOptions::default();
    let data_gen = IpcDataGenerator::default();
    let mut dict_tracker = DictionaryTracker::new(false);

    let encoded =
        data_gen.schema_to_bytes_with_dictionary_tracker(&schema, &mut dict_tracker, &options);

    Ok(vec![FlightData {
        data_header: encoded.ipc_message.into(),
        data_body: Bytes::new(),
        flight_descriptor: None,
        app_metadata: Bytes::new(),
    }])
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
        let ticket_str = String::from_utf8(ticket.ticket.to_vec())
            .map_err(|e| Status::invalid_argument(format!("Invalid ticket: {}", e)))?;

        let (key_str, client_version) = if let Some((k, v)) = ticket_str.split_once(':') {
            let version: u64 = v
                .parse()
                .map_err(|e| Status::invalid_argument(format!("Invalid version: {}", e)))?;
            (k.to_string(), version)
        } else {
            (ticket_str, 0)
        };

        tracing::debug!("do_get: key={}, client_version={}", key_str, client_version);

        let key = ObjectKey::try_from(key_str.as_str())?;

        let (server_version, metadata_keys) = {
            let metadata = lock_ptr!(self.cache.metadata)
                .map_err(|e| Status::internal(format!("Lock error: {}", e)))?;
            let version = metadata.get(&key_str).map(|m| m.version).unwrap_or(0);
            let keys: Vec<String> = metadata.keys().cloned().collect();
            (version, keys)
        };

        tracing::debug!(
            "do_get: key={}, server_version={}, metadata_keys_count={}",
            key_str,
            server_version,
            metadata_keys.len()
        );

        if client_version != 0 && server_version == client_version {
            tracing::debug!(
                "do_get: key={}, not_modified (version={})",
                key_str,
                server_version
            );
            let flight_data_vec = create_empty_flight_data()?;
            let stream = futures::stream::iter(flight_data_vec.into_iter().map(Ok));
            return Ok(Response::new(Box::pin(stream)));
        }

        let object = self.cache.get(&key).await?;

        tracing::debug!(
            "do_get: key={}, base_size={}, delta_count={}",
            key_str,
            object.data.len(),
            object.deltas.len()
        );

        let flight_data_vec = object_to_flight_data_vec(&object)?;

        let stream = futures::stream::iter(flight_data_vec.into_iter().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn do_put(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        let stream = request.into_inner();

        let (key_or_prefix, object_id, command, batches) =
            Self::collect_batches_from_stream(stream).await?;
        tracing::debug!(
            "do_put: key_or_prefix={}, object_id={:?}, command={:?}, batch_count={}",
            key_or_prefix,
            object_id,
            command,
            batches.len()
        );

        let combined_batch = Self::combine_batches(batches)?;
        let object = batch_to_object(&combined_batch)?;

        let metadata = if command.as_deref() == Some("PATCH") {
            let key_str = match object_id {
                Some(oid) => format!("{}/{}", key_or_prefix, oid),
                None => key_or_prefix,
            };
            tracing::debug!("do_put: PATCH operation for key={}", key_str);
            let key = ObjectKey::try_from(key_str.as_str())?;
            self.cache.patch(&key, object).await?
        } else {
            let key = ObjectKey::from_path(&key_or_prefix)?;
            let key = match object_id {
                Some(oid) => {
                    tracing::debug!(
                        "do_put: PUT with explicit object_id={} for prefix={}",
                        oid,
                        key_or_prefix
                    );
                    key.with_object_id(oid)?
                }
                None => key,
            };
            self.cache.put(key, object).await?
        };

        tracing::debug!(
            "do_put: key={}, version={}, size={} bytes",
            metadata.key,
            metadata.version,
            metadata.size
        );

        let result = Self::create_put_result(&metadata)?;
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
            "DELETE" => self.handle_delete_action(action_body).await?,
            _ => {
                return Err(Status::invalid_argument(format!(
                    "Unknown action type: {}. Only DELETE is supported.",
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
        let actions = vec![ActionType {
            r#type: "DELETE".to_string(),
            description: "Delete objects by key prefix".to_string(),
        }];

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

    let storage_url = cache_config.storage.as_deref().unwrap_or("none");
    let storage = crate::storage::connect(storage_url).await?;

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
        storage,
        Some(&eviction_config),
    )?);

    cache.load_from_storage().await?;

    let server = FlightCacheServer::new(Arc::clone(&cache));

    tracing::info!("Starting Arrow Flight cache server at {}", address_str);

    let addr = address_str
        .parse()
        .map_err(|e| FlameError::InvalidConfig(format!("Invalid address: {}", e)))?;

    let mut builder = tonic::transport::Server::builder();

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

#[cfg(test)]
mod tests {
    use super::*;

    mod validation {
        use super::*;

        #[test]
        fn object_key_accepts_valid_keys() {
            assert!(ObjectKey::try_from("app/session/object").is_ok());
            assert!(ObjectKey::try_from("my-app/my-session/my-object").is_ok());
            assert!(ObjectKey::try_from("test-app/test-session/test-object").is_ok());
            assert!(
                ObjectKey::try_from("app1/session1/550e8400-e29b-41d4-a716-446655440000").is_ok()
            );
        }

        #[test]
        fn object_key_rejects_path_traversal() {
            assert!(ObjectKey::try_from("../etc/passwd").is_err());
            assert!(ObjectKey::try_from("app/../other/object").is_err());
            assert!(ObjectKey::try_from("app/session/..").is_err());
        }

        #[test]
        fn object_key_rejects_two_part_keys() {
            assert!(ObjectKey::try_from("session/object").is_err());
        }

        #[test]
        fn object_key_rejects_empty_components() {
            assert!(ObjectKey::try_from("app//object").is_err());
            assert!(ObjectKey::try_from("/session/object").is_err());
        }

        #[test]
        fn object_key_from_path_two_parts() {
            let key = ObjectKey::from_path("my-app/my-session").unwrap();
            assert_eq!(key.app_name, "my-app");
            assert_eq!(key.session_id, "my-session");
            assert!(key.object_id.is_none());
        }

        #[test]
        fn object_key_from_path_three_parts() {
            let key = ObjectKey::from_path("my-app/my-session/my-uuid").unwrap();
            assert_eq!(key.app_name, "my-app");
            assert_eq!(key.session_id, "my-session");
            assert_eq!(key.object_id, Some("my-uuid".to_string()));
        }

        #[test]
        fn object_key_to_key_and_prefix() {
            let key = ObjectKey::from_path("app/session/uuid").unwrap();
            assert_eq!(key.to_key(), Some("app/session/uuid".to_string()));
            assert_eq!(key.to_prefix(), "app/session");
        }

        #[test]
        fn object_key_with_generated_id() {
            let key = ObjectKey::from_path("app/session").unwrap();
            assert!(key.object_id.is_none());
            let key_with_id = key.with_generated_id();
            assert!(key_with_id.object_id.is_some());
            assert!(key_with_id.to_key().is_some());
        }
    }

    mod object_struct {
        use super::*;

        #[test]
        fn new_creates_object_without_deltas() {
            let obj = Object::new(1, vec![1, 2, 3]);
            assert_eq!(obj.version, 1);
            assert_eq!(obj.data, vec![1, 2, 3]);
            assert!(obj.deltas.is_empty());
        }

        #[test]
        fn with_deltas_creates_object_with_deltas() {
            let delta1 = Object::new(1, vec![4, 5]);
            let delta2 = Object::new(2, vec![6, 7]);
            let obj = Object::with_deltas(0, vec![1, 2, 3], vec![delta1.clone(), delta2.clone()]);

            assert_eq!(obj.version, 0);
            assert_eq!(obj.data, vec![1, 2, 3]);
            assert_eq!(obj.deltas.len(), 2);
            assert_eq!(obj.deltas[0].data, vec![4, 5]);
            assert_eq!(obj.deltas[1].data, vec![6, 7]);
        }

        #[test]
        fn object_clone_works() {
            let obj = Object::new(42, vec![10, 20, 30]);
            let cloned = obj.clone();
            assert_eq!(cloned.version, obj.version);
            assert_eq!(cloned.data, obj.data);
        }
    }

    mod cache_endpoint {
        use super::*;

        #[test]
        fn to_uri_formats_grpc_scheme() {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            };
            assert_eq!(endpoint.to_uri(), "grpc://localhost:9090");
        }

        #[test]
        fn to_uri_converts_grpcs_to_grpc_tls() {
            let endpoint = CacheEndpoint {
                scheme: "grpcs".to_string(),
                host: "example.com".to_string(),
                port: 443,
            };
            assert_eq!(endpoint.to_uri(), "grpc+tls://example.com:443");
        }

        #[test]
        fn try_from_string_parses_valid_endpoint() {
            let endpoint_str = "grpc://localhost:9090".to_string();
            let endpoint = CacheEndpoint::try_from(&endpoint_str).unwrap();
            assert_eq!(endpoint.scheme, "grpc");
            assert_eq!(endpoint.host, "localhost");
            assert_eq!(endpoint.port, 9090);
        }

        #[test]
        fn try_from_string_uses_default_port() {
            let endpoint_str = "grpc://localhost".to_string();
            let endpoint = CacheEndpoint::try_from(&endpoint_str).unwrap();
            assert_eq!(endpoint.port, 9090);
        }

        #[test]
        fn try_from_string_rejects_invalid_url() {
            let endpoint_str = "not a valid url".to_string();
            assert!(CacheEndpoint::try_from(&endpoint_str).is_err());
        }

        #[test]
        fn try_from_string_rejects_missing_host() {
            let endpoint_str = "grpc:///path".to_string();
            assert!(CacheEndpoint::try_from(&endpoint_str).is_err());
        }
    }

    mod object_batch_conversion {
        use super::*;

        #[test]
        fn object_to_batch_and_back() {
            let original = Object::new(42, vec![1, 2, 3, 4, 5]);
            let batch = object_to_batch(&original).unwrap();

            assert_eq!(batch.num_rows(), 1);
            assert_eq!(batch.num_columns(), 2);

            let recovered = batch_to_object(&batch).unwrap();
            assert_eq!(recovered.version, original.version);
            assert_eq!(recovered.data, original.data);
            assert!(recovered.deltas.is_empty());
        }

        #[test]
        fn object_to_batch_handles_empty_data() {
            let obj = Object::new(0, vec![]);
            let batch = object_to_batch(&obj).unwrap();
            let recovered = batch_to_object(&batch).unwrap();
            assert_eq!(recovered.version, 0);
            assert!(recovered.data.is_empty());
        }

        #[test]
        fn object_to_batch_handles_large_data() {
            let large_data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
            let obj = Object::new(999, large_data.clone());
            let batch = object_to_batch(&obj).unwrap();
            let recovered = batch_to_object(&batch).unwrap();
            assert_eq!(recovered.version, 999);
            assert_eq!(recovered.data, large_data);
        }

        #[test]
        fn get_object_schema_returns_correct_schema() {
            let schema = get_object_schema();
            assert_eq!(schema.fields().len(), 2);
            assert_eq!(schema.field(0).name(), "version");
            assert_eq!(schema.field(1).name(), "data");
        }
    }

    mod object_cache_operations {
        use super::*;

        async fn create_test_cache() -> ObjectCache {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            };
            let storage = crate::storage::connect("none").await.unwrap();
            ObjectCache::new(endpoint, storage, None).unwrap()
        }

        #[tokio::test]
        async fn put_and_get_object() {
            let cache = create_test_cache().await;
            let obj = Object::new(1, vec![1, 2, 3]);

            let key = ObjectKey::from_path("test-app/test-session").unwrap();
            let meta = cache.put(key, obj.clone()).await.unwrap();
            assert!(meta.key.starts_with("test-app/test-session/"));
            assert_eq!(meta.size, 3);

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let retrieved = cache.get(&key).await.unwrap();
            assert_eq!(retrieved.version, 1);
            assert_eq!(retrieved.data, vec![1, 2, 3]);
        }

        #[tokio::test]
        async fn put_with_custom_id() {
            let cache = create_test_cache().await;
            let obj = Object::new(0, vec![42]);

            let key = ObjectKey::from_path("app/session")
                .unwrap()
                .with_object_id("custom-id".to_string())
                .unwrap();
            let meta = cache.put(key, obj).await.unwrap();

            assert_eq!(meta.key, "app/session/custom-id");
        }

        #[tokio::test]
        async fn get_returns_not_found_for_missing_key() {
            let cache = create_test_cache().await;
            let key = ObjectKey::try_from("app/session/nonexistent").unwrap();
            let result = cache.get(&key).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn get_rejects_invalid_key() {
            let _cache = create_test_cache().await;
            let key = ObjectKey::try_from("../invalid");
            assert!(key.is_err());
        }

        #[tokio::test]
        async fn delete_removes_session_objects() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session-to-delete").unwrap();
            cache
                .put(key.clone(), Object::new(0, vec![1]))
                .await
                .unwrap();
            cache
                .put(key.clone(), Object::new(0, vec![2]))
                .await
                .unwrap();

            let other_key = ObjectKey::from_path("app/other-session").unwrap();
            cache.put(other_key, Object::new(0, vec![3])).await.unwrap();

            let delete_key = ObjectKey::from_path("app/session-to-delete").unwrap();
            cache.delete(&delete_key).await.unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert!(all[0].key.starts_with("app/other-session/"));
        }

        #[tokio::test]
        async fn list_all_returns_all_metadata() {
            let cache = create_test_cache().await;

            let key1 = ObjectKey::from_path("app/s1").unwrap();
            cache.put(key1, Object::new(0, vec![1])).await.unwrap();

            let key2 = ObjectKey::from_path("app/s2").unwrap();
            cache.put(key2, Object::new(0, vec![2])).await.unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 2);
        }

        #[tokio::test]
        async fn put_rejects_invalid_key_prefix() {
            let key = ObjectKey::from_path("../bad");
            assert!(key.is_err());
        }

        #[tokio::test]
        async fn put_rejects_invalid_object_id() {
            let key = ObjectKey::from_path("app/session").unwrap();
            let result = key.with_object_id("../bad".to_string());
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn create_metadata_includes_endpoint() {
            let cache = create_test_cache().await;
            let meta = cache.create_metadata("app/session/key".to_string(), 1, 100, 5);

            assert_eq!(meta.key, "app/session/key");
            assert_eq!(meta.version, 1);
            assert_eq!(meta.size, 100);
            assert_eq!(meta.delta_count, 5);
            assert!(meta.endpoint.contains("localhost:9090"));
        }
    }

    mod versioning {
        use super::*;

        async fn create_test_cache() -> ObjectCache {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            };
            let storage = crate::storage::connect("none").await.unwrap();
            ObjectCache::new(endpoint, storage, None).unwrap()
        }

        #[tokio::test]
        async fn put_initializes_version_to_one() {
            let cache = create_test_cache().await;
            let obj = Object::new(0, vec![1, 2, 3]);

            let key = ObjectKey::from_path("test-app/test-session").unwrap();
            let meta = cache.put(key, obj).await.unwrap();

            assert_eq!(meta.version, 1);

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let retrieved = cache.get(&key).await.unwrap();
            assert_eq!(retrieved.version, 1);
        }

        #[tokio::test]
        async fn version_persists_after_get() {
            let cache = create_test_cache().await;
            let obj = Object::new(0, vec![1, 2, 3]);

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta = cache.put(key, obj).await.unwrap();

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let retrieved1 = cache.get(&key).await.unwrap();
            assert_eq!(retrieved1.version, 1);

            let retrieved2 = cache.get(&key).await.unwrap();
            assert_eq!(retrieved2.version, 1);
        }
    }

    mod flight_data_conversion {
        use super::*;

        #[test]
        fn object_to_flight_data_vec_creates_valid_flight_data() {
            let obj = Object::new(1, vec![1, 2, 3]);
            let flight_data = object_to_flight_data_vec(&obj).unwrap();

            assert!(!flight_data.is_empty());
        }

        #[test]
        fn object_to_flight_data_vec_includes_deltas() {
            let delta = Object::new(1, vec![4, 5]);
            let obj = Object::with_deltas(0, vec![1, 2, 3], vec![delta]);
            let flight_data = object_to_flight_data_vec(&obj).unwrap();

            assert!(flight_data.len() >= 2);
        }

        #[test]
        fn encode_schema_returns_valid_bytes() {
            let schema = get_object_schema();
            let encoded = encode_schema(&schema).unwrap();
            assert!(!encoded.is_empty());
        }
    }
}

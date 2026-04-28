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

/// Default batch size for eviction operations
const EVICTION_BATCH_SIZE: usize = 10;

/// Parsed object key: `<app_name>/<session_id>/<object_id>`
#[derive(Debug, Clone)]
pub struct ObjectKey {
    pub app_name: String,
    pub session_id: String,
    pub object_id: Option<String>,
}

impl ObjectKey {
    /// Parse from path string (2-part or 3-part)
    pub fn from_path(path_str: &str) -> Result<Self, FlameError> {
        let parts: Vec<&str> = path_str.split('/').collect();

        for part in &parts {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid key component: '{}'",
                    part
                )));
            }
        }

        match parts.len() {
            2 => Ok(ObjectKey {
                app_name: parts[0].to_string(),
                session_id: parts[1].to_string(),
                object_id: None,
            }),
            3 => Ok(ObjectKey {
                app_name: parts[0].to_string(),
                session_id: parts[1].to_string(),
                object_id: Some(parts[2].to_string()),
            }),
            _ => Err(FlameError::InvalidConfig(format!(
                "Invalid path '{}': expected '<app>/<ssn>' or '<app>/<ssn>/<uuid>'",
                path_str
            ))),
        }
    }

    pub fn to_key(&self) -> Option<String> {
        self.object_id
            .as_ref()
            .map(|oid| format!("{}/{}/{}", self.app_name, self.session_id, oid))
    }

    pub fn to_prefix(&self) -> String {
        format!("{}/{}", self.app_name, self.session_id)
    }

    pub fn with_generated_id(self) -> Self {
        Self {
            object_id: Some(uuid::Uuid::new_v4().to_string()),
            ..self
        }
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

fn validate_key(key: &str) -> Result<(), FlameError> {
    let _ = ObjectKey::try_from(key)?;
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
    storage: crate::storage::StorageEnginePtr,
    objects: MutexPtr<HashMap<String, Object>>,
    metadata: MutexPtr<HashMap<String, ObjectMetadata>>,
    eviction_policy: EvictionPolicyPtr,
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
        })
    }

    async fn load_from_storage(&self) -> Result<(), FlameError> {
        let items = self.storage.load_objects().await?;

        let mut objects = lock_ptr!(self.objects)?;
        let mut metadata = lock_ptr!(self.metadata)?;

        for (key, object, delta_count) in items {
            let size = object.data.len() as u64;
            let meta = self.create_metadata(key.clone(), size, delta_count);

            objects.insert(key.clone(), object);
            metadata.insert(key.clone(), meta);

            self.eviction_policy.on_add(&key, size);
        }

        drop(objects);
        drop(metadata);
        self.run_eviction()?;

        Ok(())
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

    async fn put(
        &self,
        session_id: SessionID,
        object: Object,
    ) -> Result<ObjectMetadata, FlameError> {
        self.put_with_id(session_id, None, object).await
    }

    async fn put_with_id(
        &self,
        key_prefix: SessionID,
        object_id: Option<String>,
        object: Object,
    ) -> Result<ObjectMetadata, FlameError> {
        let object_key = ObjectKey::from_path(&key_prefix)?;
        let object_key = match object_id {
            Some(id) => {
                if id.is_empty() || id.contains("..") || id.contains('\\') || id.contains('/') {
                    return Err(FlameError::InvalidConfig(format!(
                        "Invalid object_id: '{}'",
                        id
                    )));
                }
                ObjectKey {
                    object_id: Some(id),
                    ..object_key
                }
            }
            None => object_key.with_generated_id(),
        };

        let key = object_key.to_key().ok_or_else(|| {
            FlameError::Internal("ObjectKey missing object_id in put_with_id".to_string())
        })?;
        let size = object.data.len() as u64;

        self.storage.write_object(&key, &object).await?;

        let meta = self.create_metadata(key.clone(), size, 0);

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key.clone(), object);
            metadata.insert(key.clone(), meta.clone());
        }

        self.eviction_policy.on_add(&key, size);
        self.run_eviction()?;

        tracing::debug!("Object put: {}", key);

        Ok(meta)
    }

    async fn get(&self, key: String) -> Result<Object, FlameError> {
        validate_key(&key)?;

        self.eviction_policy.on_access(&key);

        {
            let objects = lock_ptr!(self.objects)?;
            if let Some(object) = objects.get(&key) {
                tracing::debug!("Object get from memory: {}", key);
                return Ok(object.clone());
            }
        }

        if let Some(object) = self.storage.read_object(&key).await? {
            let size = object.data.len() as u64;
            let delta_count = object.deltas.len() as u64;

            {
                let mut objects = lock_ptr!(self.objects)?;
                let mut metadata = lock_ptr!(self.metadata)?;

                objects.insert(key.clone(), object.clone());

                let meta = self.create_metadata(key.clone(), size, delta_count);
                metadata.insert(key.clone(), meta);
            }

            self.eviction_policy.on_add(&key, size);
            self.run_eviction()?;

            tracing::debug!("Object loaded from storage: {}", key);
            return Ok(object);
        }

        Err(FlameError::NotFound(format!("object <{}> not found", key)))
    }

    async fn update(&self, key: String, new_object: Object) -> Result<ObjectMetadata, FlameError> {
        validate_key(&key)?;

        let size = new_object.data.len() as u64;

        self.storage.write_object(&key, &new_object).await?;

        let meta = self.create_metadata(key.clone(), size, 0);

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key.clone(), new_object);
            metadata.insert(key.clone(), meta.clone());
        }

        self.eviction_policy.on_add(&key, size);
        self.run_eviction()?;

        tracing::debug!("Object update: {}", key);

        Ok(meta)
    }

    async fn patch(&self, key: String, delta: Object) -> Result<ObjectMetadata, FlameError> {
        validate_key(&key)?;

        self.eviction_policy.on_access(&key);

        let mut meta = self.storage.patch_object(&key, &delta).await?;
        meta.endpoint = self.endpoint.to_uri();

        {
            let mut metadata = lock_ptr!(self.metadata)?;
            metadata.insert(key.clone(), meta.clone());
        }

        // Invalidate in-memory cache to force reload with new delta on next access
        {
            let mut objects = lock_ptr!(self.objects)?;
            if objects.remove(&key).is_some() {
                self.eviction_policy.on_remove(&key);
            }
        }

        tracing::debug!("Object patch: {} (delta_count: {})", key, meta.delta_count);
        Ok(meta)
    }

    async fn delete(&self, prefix: SessionID) -> Result<(), FlameError> {
        let parts: Vec<&str> = prefix.split('/').collect();
        if parts.is_empty() || parts.len() > 2 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid prefix '{}': expected '<app>' or '<app>/<ssn>'",
                prefix
            )));
        }

        for part in &parts {
            if part.is_empty() || part.contains("..") || part.contains('\\') {
                return Err(FlameError::InvalidConfig(format!(
                    "Invalid prefix component: '{}'",
                    part
                )));
            }
        }

        let prefix_with_slash = format!("{}/", prefix);

        let keys_to_remove: Vec<String> = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata
                .keys()
                .filter(|k| k.starts_with(&prefix_with_slash))
                .cloned()
                .collect()
        };

        for key in &keys_to_remove {
            self.eviction_policy.on_remove(key);
        }

        self.storage.delete_objects(&prefix).await?;

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.retain(|key, _| !key.starts_with(&prefix_with_slash));
            metadata.retain(|key, _| !key.starts_with(&prefix_with_slash));
        }

        tracing::debug!("Deleted prefix: <{}>", prefix);

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
        fn validate_key_accepts_valid_keys() {
            assert!(validate_key("app/session/object").is_ok());
            assert!(validate_key("my-app/my-session/my-object").is_ok());
            assert!(validate_key("test-app/test-session/test-object").is_ok());
            assert!(validate_key("app1/session1/550e8400-e29b-41d4-a716-446655440000").is_ok());
        }

        #[test]
        fn validate_key_rejects_path_traversal() {
            assert!(validate_key("../etc/passwd").is_err());
            assert!(validate_key("app/../other/object").is_err());
            assert!(validate_key("app/session/..").is_err());
        }

        #[test]
        fn validate_key_rejects_two_part_keys() {
            assert!(validate_key("session/object").is_err());
        }

        #[test]
        fn validate_key_rejects_empty_components() {
            assert!(validate_key("app//object").is_err());
            assert!(validate_key("/session/object").is_err());
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

            let meta = cache
                .put("test-app/test-session".to_string(), obj.clone())
                .await
                .unwrap();
            assert!(meta.key.starts_with("test-app/test-session/"));
            assert_eq!(meta.size, 3);

            let retrieved = cache.get(meta.key.clone()).await.unwrap();
            assert_eq!(retrieved.version, 1);
            assert_eq!(retrieved.data, vec![1, 2, 3]);
        }

        #[tokio::test]
        async fn put_with_custom_id() {
            let cache = create_test_cache().await;
            let obj = Object::new(0, vec![42]);

            let meta = cache
                .put_with_id(
                    "app/session".to_string(),
                    Some("custom-id".to_string()),
                    obj,
                )
                .await
                .unwrap();

            assert_eq!(meta.key, "app/session/custom-id");
        }

        #[tokio::test]
        async fn get_returns_not_found_for_missing_key() {
            let cache = create_test_cache().await;
            let result = cache.get("app/session/nonexistent".to_string()).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn get_rejects_invalid_key() {
            let cache = create_test_cache().await;
            let result = cache.get("../invalid".to_string()).await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn update_replaces_object() {
            let cache = create_test_cache().await;
            let obj1 = Object::new(1, vec![1, 2, 3]);

            let meta = cache.put("app/session".to_string(), obj1).await.unwrap();

            let obj2 = Object::new(2, vec![4, 5, 6, 7]);
            let updated_meta = cache.update(meta.key.clone(), obj2).await.unwrap();

            assert_eq!(updated_meta.size, 4);

            let retrieved = cache.get(meta.key).await.unwrap();
            assert_eq!(retrieved.version, 2);
            assert_eq!(retrieved.data, vec![4, 5, 6, 7]);
        }

        #[tokio::test]
        async fn delete_removes_session_objects() {
            let cache = create_test_cache().await;

            cache
                .put("app/session-to-delete".to_string(), Object::new(0, vec![1]))
                .await
                .unwrap();
            cache
                .put("app/session-to-delete".to_string(), Object::new(0, vec![2]))
                .await
                .unwrap();
            cache
                .put("app/other-session".to_string(), Object::new(0, vec![3]))
                .await
                .unwrap();

            cache
                .delete("app/session-to-delete".to_string())
                .await
                .unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert!(all[0].key.starts_with("app/other-session/"));
        }

        #[tokio::test]
        async fn list_all_returns_all_metadata() {
            let cache = create_test_cache().await;

            cache
                .put("app/s1".to_string(), Object::new(0, vec![1]))
                .await
                .unwrap();
            cache
                .put("app/s2".to_string(), Object::new(0, vec![2]))
                .await
                .unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 2);
        }

        #[tokio::test]
        async fn put_rejects_invalid_key_prefix() {
            let cache = create_test_cache().await;
            let result = cache
                .put("../bad".to_string(), Object::new(0, vec![]))
                .await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn put_with_id_rejects_invalid_object_id() {
            let cache = create_test_cache().await;
            let result = cache
                .put_with_id(
                    "app/session".to_string(),
                    Some("../bad".to_string()),
                    Object::new(0, vec![]),
                )
                .await;
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn create_metadata_includes_endpoint() {
            let cache = create_test_cache().await;
            let meta = cache.create_metadata("app/session/key".to_string(), 100, 5);

            assert_eq!(meta.key, "app/session/key");
            assert_eq!(meta.size, 100);
            assert_eq!(meta.delta_count, 5);
            assert!(meta.endpoint.contains("localhost:9090"));
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

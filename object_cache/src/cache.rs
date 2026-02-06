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
use arrow::ipc::{reader::FileReader, writer::FileWriter};
use arrow_flight::{
    flight_service_server::{FlightService, FlightServiceServer},
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, Location, PutResult, Result as FlightResult, SchemaResult,
    Ticket,
};
use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures::Stream;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use regex::Regex;
use stdng::{lock_ptr, new_ptr, MutexPtr};
use tonic::{Request, Response, Status, Streaming};
use url::Url;

use common::apis::SessionID;
use common::ctx::FlameCache;
use common::FlameError;

#[derive(Debug, Clone)]
pub struct Object {
    pub version: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct CacheEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl CacheEndpoint {
    fn to_uri(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
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
    objects: MutexPtr<HashMap<String, ObjectMetadata>>, // key -> metadata
}

impl ObjectCache {
    fn new(endpoint: CacheEndpoint, storage_path: Option<PathBuf>) -> Result<Self, FlameError> {
        let cache = Self {
            endpoint,
            storage_path: storage_path.clone(),
            objects: new_ptr(HashMap::new()),
        };

        // Load existing objects from disk
        if let Some(storage_path) = &storage_path {
            cache.load_from_disk(storage_path)?;
        }

        Ok(cache)
    }

    fn create_metadata(&self, key: String, size: u64) -> ObjectMetadata {
        ObjectMetadata {
            endpoint: self.endpoint.to_uri(),
            key,
            version: 0,
            size,
        }
    }

    fn load_session_objects(
        &self,
        application_id: &str,
        session_path: &Path,
        objects: &mut HashMap<String, ObjectMetadata>,
    ) -> Result<(), FlameError> {
        let session_id = session_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| FlameError::Internal("Invalid session directory name".to_string()))?;

        for object_entry in fs::read_dir(session_path)? {
            let object_entry = object_entry?;
            let object_path = object_entry.path();

            if !object_path.is_file()
                || object_path.extension().and_then(|e| e.to_str()) != Some("arrow")
            {
                continue;
            }

            let object_id = object_path
                .file_stem()
                .and_then(|n| n.to_str())
                .ok_or_else(|| FlameError::Internal("Invalid object file name".to_string()))?;

            let key = format!("{}/{}/{}", application_id, session_id, object_id);
            let size = fs::metadata(&object_path)?.len();
            let metadata = self.create_metadata(key.clone(), size);

            tracing::debug!("Loaded object: {}", key);
            objects.insert(key, metadata);
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

        // Iterate over application directories
        for application_entry in fs::read_dir(storage_path)? {
            let application_entry = application_entry?;
            let application_path = application_entry.path();

            if !application_path.is_dir() {
                continue;
            }

            let application_id = application_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| FlameError::Internal("Invalid application directory name".to_string()))?;

            // Iterate over session directories within application directory
            for session_entry in fs::read_dir(&application_path)? {
                let session_entry = session_entry?;
                let session_path = session_entry.path();

                if !session_path.is_dir() {
                    continue;
                }

                self.load_session_objects(application_id, &session_path, &mut objects)?;
            }
        }

        tracing::info!("Loaded {} objects from disk", objects.len());
        Ok(())
    }

    async fn put(
        &self,
        application_id: String,
        session_id: SessionID,
        object: Object,
    ) -> Result<ObjectMetadata, FlameError> {
        self.put_with_id(application_id, session_id, None, object).await
    }

    async fn put_with_id(
        &self,
        application_id: String,
        session_id: SessionID,
        object_id: Option<String>,
        object: Object,
    ) -> Result<ObjectMetadata, FlameError> {
        let object_id = object_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let key = format!("{}/{}/{}", application_id, session_id, object_id);

        // Write to disk if storage is configured
        if let Some(storage_path) = &self.storage_path {
            // Create application/session directory structure
            let application_dir = storage_path.join(&application_id);
            let session_dir = application_dir.join(&session_id);
            fs::create_dir_all(&session_dir)?;

            // Write object to Arrow IPC file
            let object_path = session_dir.join(format!("{}.arrow", object_id));
            let batch = object_to_batch(&object)
                .map_err(|e| FlameError::Internal(format!("Failed to create batch: {}", e)))?;

            write_batch_to_file(&object_path, &batch)?;
            tracing::debug!("Wrote object to disk: {:?}", object_path);
        }

        let metadata = self.create_metadata(key.clone(), object.data.len() as u64);

        // Update in-memory index
        let mut objects = lock_ptr!(self.objects)?;
        objects.insert(key.clone(), metadata.clone());

        tracing::debug!("Object put: {}", key);

        Ok(metadata)
    }

    fn load_object_from_disk(&self, key: &str) -> Result<Object, FlameError> {
        let storage_path = self
            .storage_path
            .as_ref()
            .ok_or_else(|| FlameError::InvalidConfig("Storage path not configured".to_string()))?;

        // Parse key: application_id/session_id/object_id
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 3 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid key format (expected application_id/session_id/object_id): {}",
                key
            )));
        }

        let application_id = parts[0];
        let session_id = parts[1];
        let object_id = parts[2];

        // Build path: storage_path/application_id/session_id/object_id.arrow
        let object_path = storage_path
            .join(application_id)
            .join(session_id)
            .join(format!("{}.arrow", object_id));

        let file = fs::File::open(&object_path)
            .map_err(|e| FlameError::NotFound(format!("Object file not found: {}", e)))?;
        let reader = FileReader::try_new(file, None)
            .map_err(|e| FlameError::Internal(format!("Failed to create reader: {}", e)))?;

        let batch = reader
            .into_iter()
            .next()
            .ok_or_else(|| FlameError::Internal("No batches in file".to_string()))?
            .map_err(|e| FlameError::Internal(format!("Failed to read batch: {}", e)))?;

        let object = batch_to_object(&batch)
            .map_err(|e| FlameError::Internal(format!("Failed to parse batch: {}", e)))?;

        Ok(object)
    }

    fn try_load_and_index(&self, key: &str) -> Result<Option<Object>, FlameError> {
        let storage_path = match &self.storage_path {
            Some(path) => path,
            None => return Ok(None),
        };

        // Parse key: application_id/session_id/object_id
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 3 {
            return Ok(None);
        }

        let application_id = parts[0];
        let session_id = parts[1];
        let object_id = parts[2];

        // Build path: storage_path/application_id/session_id/object_id.arrow
        let object_path = storage_path
            .join(application_id)
            .join(session_id)
            .join(format!("{}.arrow", object_id));

        if !object_path.exists() {
            return Ok(None);
        }

        let object = self.load_object_from_disk(key)?;

        // Add to in-memory index
        let metadata = self.create_metadata(key.to_string(), object.data.len() as u64);
        let mut objects = lock_ptr!(self.objects)?;
        objects.insert(key.to_string(), metadata);

        tracing::debug!("Loaded object from disk: {}", key);
        Ok(Some(object))
    }

    async fn get(&self, key: String) -> Result<Object, FlameError> {
        // Check if object exists in index
        let exists_in_index = {
            let objects = lock_ptr!(self.objects)?;
            objects.contains_key(&key)
        };

        // If not in index, try to load from disk and add to index
        if !exists_in_index {
            if let Some(object) = self.try_load_and_index(&key)? {
                return Ok(object);
            }
            return Err(FlameError::NotFound(format!("object <{}> not found", key)));
        }

        // Object is in index, load from disk
        let object = self.load_object_from_disk(&key)?;
        tracing::debug!("Object get from disk: {}", key);
        Ok(object)
    }

    async fn update(&self, key: String, new_object: Object) -> Result<ObjectMetadata, FlameError> {
        // Parse key: application_id/session_id/object_id
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 3 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid key format (expected application_id/session_id/object_id): {}",
                key
            )));
        }
        let application_id = parts[0];
        let session_id = parts[1];
        let object_id = parts[2];

        // Write to disk if storage is configured
        if let Some(storage_path) = &self.storage_path {
            let object_path = storage_path
                .join(application_id)
                .join(session_id)
                .join(format!("{}.arrow", object_id));
            let batch = object_to_batch(&new_object)
                .map_err(|e| FlameError::Internal(format!("Failed to create batch: {}", e)))?;

            write_batch_to_file(&object_path, &batch)?;
            tracing::debug!("Updated object on disk: {:?}", object_path);
        }

        let metadata = self.create_metadata(key.clone(), new_object.data.len() as u64);

        // Update in-memory index
        let mut objects = lock_ptr!(self.objects)?;
        objects.insert(key.clone(), metadata.clone());

        tracing::debug!("Object update: {}", key);

        Ok(metadata)
    }

    async fn delete(&self, application_id: String, session_id: SessionID) -> Result<(), FlameError> {
        // Delete session directory and all objects
        if let Some(storage_path) = &self.storage_path {
            let session_dir = storage_path.join(&application_id).join(&session_id);
            if session_dir.exists() {
                fs::remove_dir_all(&session_dir)?;
                tracing::debug!("Deleted session directory: {:?}", session_dir);
            }
        }

        // Remove from in-memory index
        let mut objects = lock_ptr!(self.objects)?;
        let prefix = format!("{}/{}/", application_id, session_id);
        objects.retain(|key, _| !key.starts_with(&prefix));

        tracing::debug!("Session deleted: <{}/{}>", application_id, session_id);

        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError> {
        let objects = lock_ptr!(self.objects)?;
        Ok(objects.values().cloned().collect())
    }
}

pub struct FlightCacheServer {
    cache: Arc<ObjectCache>,
}

impl FlightCacheServer {
    pub fn new(cache: Arc<ObjectCache>) -> Self {
        Self { cache }
    }

    fn extract_application_session_and_object_id(
        flight_data: &FlightData,
        application_id: &mut Option<String>,
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
                    if parts.len() == 3 {
                        // Format: application_id/session_id/object_id
                        *application_id = Some(parts[0].to_string());
                        *session_id = Some(parts[1].to_string());
                        *object_id = Some(parts[2].to_string());
                    } else if parts.len() == 2 {
                        // Legacy format: session_id/object_id (for backward compatibility)
                        *session_id = Some(parts[0].to_string());
                        *object_id = Some(parts[1].to_string());
                    }
                } else {
                    // Only session_id provided
                    *session_id = Some(path_str.clone());
                }
            }
        }

        // Try to extract application_id from app_metadata if not found in path
        if application_id.is_none() && !flight_data.app_metadata.is_empty() {
            if let Ok(metadata_str) = String::from_utf8(flight_data.app_metadata.to_vec()) {
                for line in metadata_str.lines() {
                    if let Some(id) = line.strip_prefix("application_id:") {
                        *application_id = Some(id.trim().to_string());
                        break;
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
    ) -> Result<(String, String, Option<String>, Vec<RecordBatch>), FlameError> {
        let mut batches = Vec::new();
        let mut application_id: Option<String> = None;
        let mut session_id: Option<String> = None;
        let mut object_id: Option<String> = None;
        let mut schema: Option<Arc<Schema>> = None;

        while let Some(flight_data) = stream
            .message()
            .await
            .map_err(|e| FlameError::Internal(format!("Stream error: {}", e)))?
        {
            Self::extract_application_session_and_object_id(
                &flight_data,
                &mut application_id,
                &mut session_id,
                &mut object_id,
            );

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

        let application_id = application_id.ok_or_else(|| {
            FlameError::InvalidState(
                "application_id must be provided in flight descriptor path or app_metadata".to_string(),
            )
        })?;

        let session_id = session_id.ok_or_else(|| {
            FlameError::InvalidState(
                "session_id must be provided in flight descriptor path or app_metadata".to_string(),
            )
        })?;

        Ok((application_id, session_id, object_id, batches))
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
        // Format: "application_id:session_id:data_b64" or "application_id/session_id:data_b64"
        let parts: Vec<&str> = action_body.splitn(3, ':').collect();
        if parts.len() < 2 {
            return Err(FlameError::InvalidState("Invalid PUT action format".to_string()));
        }

        let (application_id, session_id) = if parts[0].contains('/') {
            // Format: "application_id/session_id:data_b64"
            let path_parts: Vec<&str> = parts[0].split('/').collect();
            if path_parts.len() != 2 {
                return Err(FlameError::InvalidState("Invalid PUT action format".to_string()));
            }
            (path_parts[0].to_string(), path_parts[1].to_string())
        } else if parts.len() == 3 {
            // Format: "application_id:session_id:data_b64"
            (parts[0].to_string(), parts[1].to_string())
        } else {
            return Err(FlameError::InvalidState("Invalid PUT action format".to_string()));
        };

        let data_b64 = parts[parts.len() - 1];
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| FlameError::InvalidState(format!("Invalid base64: {}", e)))?;

        let object = Object { version: 0, data };
        let metadata = self.cache.put(application_id, session_id, object).await?;

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

        let object = Object { version: 0, data };
        let metadata = self.cache.update(key, object).await?;

        serde_json::to_string(&metadata)
            .map_err(|e| FlameError::Internal(format!("Failed to serialize: {}", e)))
    }

    async fn handle_delete_action(&self, action_body: String) -> Result<String, FlameError> {
        // Format: "application_id/session_id" or "application_id:session_id"
        let (application_id, session_id) = if action_body.contains('/') {
            let parts: Vec<&str> = action_body.split('/').collect();
            if parts.len() != 2 {
                return Err(FlameError::InvalidState("Invalid DELETE action format".to_string()));
            }
            (parts[0].to_string(), parts[1].to_string())
        } else if action_body.contains(':') {
            let parts: Vec<&str> = action_body.split(':').collect();
            if parts.len() != 2 {
                return Err(FlameError::InvalidState("Invalid DELETE action format".to_string()));
            }
            (parts[0].to_string(), parts[1].to_string())
        } else {
            // Legacy format: only session_id (for backward compatibility)
            // Try to infer application_id from existing objects
            return Err(FlameError::InvalidState(
                "DELETE action requires application_id/session_id format".to_string(),
            ));
        };

        self.cache.delete(application_id, session_id).await?;
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

// Helper function to convert RecordBatch to FlightData
fn batch_to_flight_data_vec(batch: &RecordBatch) -> Result<Vec<FlightData>, FlameError> {
    use arrow::ipc::writer::{DictionaryTracker, IpcDataGenerator, IpcWriteOptions};

    tracing::debug!(
        "batch_to_flight_data_vec: batch rows={}, cols={}",
        batch.num_rows(),
        batch.num_columns()
    );

    // Create IPC write options with alignment to ensure proper encoding
    let options = IpcWriteOptions::default()
        .try_with_compression(None)
        .map_err(|e| FlameError::Internal(format!("Failed to set compression: {}", e)))?;

    let mut flight_data_vec = Vec::new();

    // Encode using IpcDataGenerator directly with DictionaryTracker
    let data_gen = IpcDataGenerator::default();
    let mut dict_tracker = DictionaryTracker::new(false);

    // First, encode and send schema
    let encoded_schema = data_gen.schema_to_bytes_with_dictionary_tracker(
        batch.schema().as_ref(),
        &mut dict_tracker,
        &options,
    );

    let schema_flight_data = FlightData {
        flight_descriptor: None,
        app_metadata: vec![].into(),
        data_header: encoded_schema.ipc_message.into(),
        data_body: vec![].into(),
    };
    flight_data_vec.push(schema_flight_data);

    // Then, send the batch data
    let (encoded_dictionaries, encoded_batch) = data_gen
        .encoded_batch(batch, &mut dict_tracker, &options)
        .map_err(|e| FlameError::Internal(format!("Failed to encode batch: {}", e)))?;

    // Add dictionary batches if any
    for dict_batch in encoded_dictionaries {
        flight_data_vec.push(dict_batch.into());
    }

    // Add the data batch
    flight_data_vec.push(encoded_batch.into());

    tracing::debug!(
        "batch_to_flight_data_vec: final {} FlightData messages",
        flight_data_vec.len()
    );

    if flight_data_vec.is_empty() {
        Err(FlameError::Internal(
            "No FlightData generated from batch".to_string(),
        ))
    } else {
        Ok(flight_data_vec)
    }
}

// Helper function to get the object schema
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

    Ok(Object { version, data })
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

        // Key format: "application_id/session_id/object_id"

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

        // Key format: "application_id/session_id/object_id"
        let object = self.cache.get(key.clone()).await?;

        let batch = object_to_batch(&object)?;
        tracing::debug!(
            "do_get: batch has {} rows, {} columns",
            batch.num_rows(),
            batch.num_columns()
        );

        let flight_data_vec = batch_to_flight_data_vec(&batch)?;
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

        let (application_id, session_id, object_id, batches) = Self::collect_batches_from_stream(stream).await?;
        let combined_batch = Self::combine_batches(batches)?;
        let object = batch_to_object(&combined_batch)?;

        let metadata = self
            .cache
            .put_with_id(application_id, session_id, object_id, object)
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
                description: "Update an existing object".to_string(),
            },
            ActionType {
                r#type: "DELETE".to_string(),
                description: "Delete a session and all its objects".to_string(),
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

    let cache = Arc::new(ObjectCache::new(endpoint.clone(), storage_path)?);
    let server = FlightCacheServer::new(Arc::clone(&cache));

    tracing::info!("Starting Arrow Flight cache server at {}", address_str);

    let addr = address_str
        .parse()
        .map_err(|e| FlameError::InvalidConfig(format!("Invalid address: {}", e)))?;

    tonic::transport::Server::builder()
        .add_service(FlightServiceServer::new(server))
        .serve(addr)
        .await
        .map_err(|e| FlameError::Internal(format!("Server error: {}", e)))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_cache() -> Result<(ObjectCache, TempDir), FlameError> {
        let temp_dir = TempDir::new().map_err(|e| {
            FlameError::Internal(format!("Failed to create temp directory: {}", e))
        })?;
        let storage_path = temp_dir.path().to_path_buf();

        let endpoint = CacheEndpoint {
            scheme: "grpc".to_string(),
            host: "127.0.0.1".to_string(),
            port: 9090,
        };

        let cache = ObjectCache::new(endpoint, Some(storage_path))?;
        Ok((cache, temp_dir))
    }

    fn create_test_object(data: &[u8]) -> Object {
        Object {
            version: 0,
            data: data.to_vec(),
        }
    }

    #[tokio::test]
    async fn test_put_and_get() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        let application_id = "test_app".to_string();
        let session_id = "test_session".to_string();
        let test_data = b"Hello, World!";
        let object = create_test_object(test_data);

        // Test put
        let metadata = cache.put(application_id.clone(), session_id.clone(), object).await?;
        assert_eq!(metadata.version, 0);
        assert_eq!(metadata.size, test_data.len() as u64);
        assert!(metadata.key.contains(&application_id));
        assert!(metadata.key.contains(&session_id));

        // Test get
        let retrieved_object = cache.get(metadata.key.clone()).await?;
        assert_eq!(retrieved_object.version, 0);
        assert_eq!(retrieved_object.data, test_data);

        Ok(())
    }

    #[tokio::test]
    async fn test_put_with_id() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        let application_id = "test_app".to_string();
        let session_id = "test_session".to_string();
        let object_id = "custom_object_id".to_string();
        let test_data = b"Custom object data";
        let object = create_test_object(test_data);

        // Test put_with_id
        let metadata = cache
            .put_with_id(
                application_id.clone(),
                session_id.clone(),
                Some(object_id.clone()),
                object,
            )
            .await?;

        assert_eq!(metadata.version, 0);
        assert!(metadata.key.contains(&application_id));
        assert!(metadata.key.contains(&session_id));
        assert!(metadata.key.contains(&object_id));

        // Verify the object can be retrieved
        let retrieved_object = cache.get(metadata.key.clone()).await?;
        assert_eq!(retrieved_object.data, test_data);

        Ok(())
    }

    #[tokio::test]
    async fn test_update() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        let application_id = "test_app".to_string();
        let session_id = "test_session".to_string();
        let object_id = "test_object".to_string();
        let initial_data = b"Initial data";
        let updated_data = b"Updated data";

        // Put initial object
        let initial_object = create_test_object(initial_data);
        let metadata = cache
            .put_with_id(
                application_id.clone(),
                session_id.clone(),
                Some(object_id.clone()),
                initial_object,
            )
            .await?;

        // Update the object
        let updated_object = create_test_object(updated_data);
        let updated_metadata = cache.update(metadata.key.clone(), updated_object).await?;

        assert_eq!(updated_metadata.key, metadata.key);
        assert_eq!(updated_metadata.size, updated_data.len() as u64);

        // Verify the object was updated
        let retrieved_object = cache.get(metadata.key.clone()).await?;
        assert_eq!(retrieved_object.data, updated_data);

        Ok(())
    }

    #[tokio::test]
    async fn test_delete() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        let application_id = "test_app".to_string();
        let session_id = "test_session".to_string();
        let test_data = b"Data to be deleted";
        let object = create_test_object(test_data);

        // Put an object
        let metadata = cache
            .put_with_id(
                application_id.clone(),
                session_id.clone(),
                Some("obj1".to_string()),
                object.clone(),
            )
            .await?;

        // Put another object in the same session
        let metadata2 = cache
            .put_with_id(
                application_id.clone(),
                session_id.clone(),
                Some("obj2".to_string()),
                object,
            )
            .await?;

        // Verify objects exist
        assert!(cache.get(metadata.key.clone()).await.is_ok());
        assert!(cache.get(metadata2.key.clone()).await.is_ok());

        // Delete the session
        cache.delete(application_id.clone(), session_id.clone()).await?;

        // Verify objects are deleted
        assert!(cache.get(metadata.key).await.is_err());
        assert!(cache.get(metadata2.key).await.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_list_all() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        let application_id = "test_app".to_string();
        let session_id = "test_session".to_string();

        // Put multiple objects
        for i in 0..5 {
            let object = create_test_object(format!("data_{}", i).as_bytes());
            cache
                .put_with_id(
                    application_id.clone(),
                    session_id.clone(),
                    Some(format!("obj_{}", i)),
                    object,
                )
                .await?;
        }

        // List all objects
        let all_objects = cache.list_all().await?;
        assert_eq!(all_objects.len(), 5);

        // Verify all objects have correct metadata
        for metadata in &all_objects {
            assert_eq!(metadata.version, 0);
            assert!(metadata.key.contains(&application_id));
            assert!(metadata.key.contains(&session_id));
            assert!(metadata.size > 0);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_get_nonexistent_object() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        let nonexistent_key = "app1/session1/nonexistent".to_string();
        let result = cache.get(nonexistent_key).await;

        assert!(result.is_err());
        match result {
            Err(FlameError::NotFound(_)) => Ok(()),
            _ => Err(FlameError::Internal("Expected NotFound error".to_string())),
        }
    }

    #[tokio::test]
    async fn test_persistence() -> Result<(), FlameError> {
        let temp_dir = TempDir::new().map_err(|e| {
            FlameError::Internal(format!("Failed to create temp directory: {}", e))
        })?;
        let storage_path = temp_dir.path().to_path_buf();

        let endpoint = CacheEndpoint {
            scheme: "grpc".to_string(),
            host: "127.0.0.1".to_string(),
            port: 9090,
        };

        let application_id = "test_app".to_string();
        let session_id = "test_session".to_string();
        let test_data = b"Persistent data";

        // Create cache and put an object
        {
            let cache = ObjectCache::new(endpoint.clone(), Some(storage_path.clone()))?;
            let object = create_test_object(test_data);
            cache
                .put_with_id(
                    application_id.clone(),
                    session_id.clone(),
                    Some("persistent_obj".to_string()),
                    object,
                )
                .await?;
        }

        // Create a new cache instance (simulating restart)
        {
            let cache = ObjectCache::new(endpoint, Some(storage_path))?;
            let all_objects = cache.list_all().await?;
            assert_eq!(all_objects.len(), 1);

            let metadata = &all_objects[0];
            let retrieved_object = cache.get(metadata.key.clone()).await?;
            assert_eq!(retrieved_object.data, test_data);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_applications_and_sessions() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        // Create objects in different applications and sessions
        let apps = vec!["app1", "app2"];
        let sessions = vec!["session1", "session2"];

        for app in &apps {
            for session in &sessions {
                let object = create_test_object(format!("{}_{}_data", app, session).as_bytes());
                cache
                    .put_with_id(
                        app.to_string(),
                        session.to_string(),
                        Some("obj1".to_string()),
                        object,
                    )
                    .await?;
            }
        }

        // Verify all objects exist
        let all_objects = cache.list_all().await?;
        assert_eq!(all_objects.len(), 4);

        // Delete one session
        cache.delete("app1".to_string(), "session1".to_string()).await?;

        // Verify only objects from that session are deleted
        let remaining_objects = cache.list_all().await?;
        assert_eq!(remaining_objects.len(), 3);

        // Verify remaining objects
        for metadata in &remaining_objects {
            assert!(
                !metadata.key.contains("app1/session1"),
                "Object from deleted session should not exist"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_key_format() -> Result<(), FlameError> {
        let (cache, _temp_dir) = create_test_cache()?;

        // Test invalid key formats
        let invalid_keys = vec![
            "invalid".to_string(),                    // Missing slashes
            "app/session".to_string(),               // Missing object_id
            "app/session/obj/extra".to_string(),     // Too many parts
        ];

        for invalid_key in invalid_keys {
            let object = create_test_object(b"test");
            let result = cache.update(invalid_key.clone(), object).await;
            assert!(result.is_err(), "Should fail for invalid key: {}", invalid_key);
        }

        Ok(())
    }
}

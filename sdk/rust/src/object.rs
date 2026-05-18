/*
Copyright 2026 The Flame Authors.
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

use std::fmt::{Display, Formatter};
use std::future::Future;
use std::io::Cursor;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use arrow::array::{Array, BinaryArray, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::{Action, FlightClient, FlightDescriptor, Ticket};
use bson::{doc, Bson, Document};
use bytes::Bytes;
use common::net::host_for_uri;
use futures::{stream, TryStreamExt};
use serde_derive::{Deserialize, Serialize as DeriveSerialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::transport::Channel;
use url::Url;

use crate::apis::{FlameClientCache, FlameClientTls, FlameContext, FlameError};
use crate::message::FlameMessage;

const OBJECT_FIELD_VERSION: &str = "version";
const OBJECT_FIELD_KIND: &str = "kind";
const OBJECT_FIELD_DATA: &str = "data";
const OBJECT_KIND_BASE: &str = "base";
const OBJECT_KIND_PATCH: &str = "patch";
const WILDCARD_SESSION: &str = "*";
const DEFAULT_CACHE_PORT: u16 = 9090;
const CONNECT_TIMEOUT_SECS: u64 = 30;
const UPLOAD_CHUNK_SIZE: usize = 1024 * 1024;
const FLIGHT_MAX_MESSAGE_SIZE: usize = usize::MAX;

type ObjectFutureInner<T> = Pin<Box<dyn Future<Output = Result<T, FlameError>> + Send + 'static>>;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ObjectKey {
    pub app_name: String,
    pub session_id: String,
    pub object_id: Option<String>,
}

impl ObjectKey {
    pub fn prefix(
        app_name: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self, FlameError> {
        Self::new(app_name.into(), session_id.into(), None)
    }

    pub fn key(
        app_name: impl Into<String>,
        session_id: impl Into<String>,
        object_id: impl Into<String>,
    ) -> Result<Self, FlameError> {
        Self::new(app_name.into(), session_id.into(), Some(object_id.into()))
    }

    pub fn from_path(path: impl AsRef<str>) -> Result<Self, FlameError> {
        let path = path.as_ref();
        let parts: Vec<&str> = path.split('/').collect();
        match parts.as_slice() {
            [app_name, session_id] => Self::new((*app_name).to_string(), (*session_id).to_string(), None),
            [app_name, session_id, object_id] => Self::new(
                (*app_name).to_string(),
                (*session_id).to_string(),
                Some((*object_id).to_string()),
            ),
            _ => Err(FlameError::InvalidConfig(format!(
                "invalid object key path '{}': expected '<app>/<session>' or '<app>/<session>/<object>'",
                path
            ))),
        }
    }

    pub fn from_prefix(prefix: impl AsRef<str>) -> Result<Self, FlameError> {
        let prefix = prefix.as_ref();
        let key = Self::from_path(prefix)?;
        if key.object_id.is_some() {
            return Err(FlameError::InvalidConfig(format!(
                "invalid object key prefix '{}': expected '<app>/<session>'",
                prefix
            )));
        }
        Ok(key)
    }

    pub fn from_key(key: impl AsRef<str>) -> Result<Self, FlameError> {
        let key = key.as_ref();
        let object_key = Self::from_path(key)?;
        if object_key.object_id.is_none() {
            return Err(FlameError::InvalidConfig(format!(
                "invalid object key '{}': expected '<app>/<session>/<object>'",
                key
            )));
        }
        Ok(object_key)
    }

    pub fn for_shared(app_name: impl Into<String>) -> Result<Self, FlameError> {
        Self::prefix(app_name, "shared")
    }

    pub fn for_all_sessions(app_name: impl Into<String>) -> Result<Self, FlameError> {
        Self::prefix(app_name, WILDCARD_SESSION)
    }

    pub fn is_all_sessions(&self) -> bool {
        self.session_id == WILDCARD_SESSION
    }

    pub fn with_generated_id(&self) -> Result<Self, FlameError> {
        if self.is_all_sessions() {
            return Err(FlameError::InvalidConfig(
                "wildcard session cannot have object_id".to_string(),
            ));
        }
        Self::new(
            self.app_name.clone(),
            self.session_id.clone(),
            Some(uuid::Uuid::new_v4().to_string()),
        )
    }

    pub fn to_prefix(&self) -> String {
        format!("{}/{}", self.app_name, self.session_id)
    }

    pub fn to_key(&self) -> Option<String> {
        self.object_id
            .as_ref()
            .map(|object_id| format!("{}/{}/{}", self.app_name, self.session_id, object_id))
    }

    pub fn matches_key(&self, key: impl AsRef<str>) -> bool {
        let key = key.as_ref();
        if self.is_all_sessions() {
            let prefix = format!("{}/", self.app_name);
            if !key.starts_with(&prefix) {
                return false;
            }
            let suffix = &key[prefix.len()..];
            let Some((session_id, object_id)) = suffix.split_once('/') else {
                return false;
            };
            return !session_id.is_empty() && !object_id.is_empty() && !object_id.contains('/');
        }

        if let Some(full_key) = self.to_key() {
            return key == full_key;
        }

        let prefix = format!("{}/{}/", self.app_name, self.session_id);
        if !key.starts_with(&prefix) {
            return false;
        }
        let object_id = &key[prefix.len()..];
        !object_id.is_empty() && !object_id.contains('/')
    }

    fn new(
        app_name: String,
        session_id: String,
        object_id: Option<String>,
    ) -> Result<Self, FlameError> {
        validate_component("app_name", &app_name, false)?;
        if app_name == WILDCARD_SESSION {
            return Err(FlameError::InvalidConfig(
                "wildcard '*' not allowed for app_name".to_string(),
            ));
        }

        if session_id == WILDCARD_SESSION {
            if object_id.is_some() {
                return Err(FlameError::InvalidConfig(
                    "wildcard session '*' cannot have object_id".to_string(),
                ));
            }
        } else {
            validate_component("session_id", &session_id, false)?;
        }

        if let Some(object_id) = object_id.as_deref() {
            validate_component("object_id", object_id, true)?;
        }

        Ok(Self {
            app_name,
            session_id,
            object_id,
        })
    }
}

impl Display for ObjectKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.to_key() {
            Some(key) => write!(f, "{key}"),
            None => write!(f, "{}", self.to_prefix()),
        }
    }
}

#[derive(Clone, Debug, DeriveSerialize, Deserialize, Eq, PartialEq)]
pub struct ObjectRef {
    pub endpoint: String,
    pub key: String,
    pub version: u64,
}

impl ObjectRef {
    pub fn new(
        endpoint: impl Into<String>,
        key: impl Into<String>,
        version: u64,
    ) -> Result<Self, FlameError> {
        let reference = Self {
            endpoint: endpoint.into(),
            key: key.into(),
            version,
        };
        ObjectKey::from_key(&reference.key)?;
        Ok(reference)
    }

    pub fn encode(&self) -> Result<Bytes, FlameError> {
        let mut bytes = Vec::new();
        let version = i64::try_from(self.version).map_err(|_| {
            FlameError::InvalidConfig(format!("object version too large: {}", self.version))
        })?;
        let doc = doc! {
            "endpoint": &self.endpoint,
            "key": &self.key,
            "version": version,
        };
        doc.to_writer(&mut bytes)
            .map_err(|e| FlameError::Internal(format!("failed to encode ObjectRef: {}", e)))?;
        Ok(Bytes::from(bytes))
    }

    pub fn decode(data: impl AsRef<[u8]>) -> Result<Self, FlameError> {
        let doc = Document::from_reader(Cursor::new(data.as_ref()))
            .map_err(|e| FlameError::InvalidConfig(format!("failed to decode ObjectRef: {}", e)))?;
        object_ref_from_doc(doc)
    }

    pub fn get<T>(&self) -> ObjectFuture<T>
    where
        T: FlameMessage + Send + 'static,
    {
        get_object(self.clone())
    }
}

pub struct ObjectFuture<T> {
    inner: ObjectFutureInner<T>,
}

impl<T> ObjectFuture<T> {
    fn new(inner: impl Future<Output = Result<T, FlameError>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl<T> Future for ObjectFuture<T> {
    type Output = Result<T, FlameError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub async fn put_object<T>(key_prefix: impl AsRef<str>, object: &T) -> Result<ObjectRef, FlameError>
where
    T: FlameMessage,
{
    let context = FlameContext::from_file_with_env(None)?;
    put_object_with_context(&context, key_prefix, object).await
}

pub async fn put_object_with_context<T>(
    context: &FlameContext,
    key_prefix: impl AsRef<str>,
    object: &T,
) -> Result<ObjectRef, FlameError>
where
    T: FlameMessage,
{
    let bytes = object.encode()?;
    put_encoded_object_with_context(context, key_prefix, bytes).await
}

async fn put_encoded_object_with_context(
    context: &FlameContext,
    key_prefix: impl AsRef<str>,
    bytes: impl Into<Vec<u8>>,
) -> Result<ObjectRef, FlameError> {
    let object_key = ObjectKey::from_prefix(key_prefix.as_ref())?;
    let cache = cache_from_context(context)?;
    let descriptor = FlightDescriptor::new_path(vec![object_key.to_prefix()]);
    do_put_bytes(
        &cache.endpoint,
        cache.tls.as_ref(),
        descriptor,
        bytes.into(),
    )
    .await
}

pub fn get_object<T>(reference: ObjectRef) -> ObjectFuture<T>
where
    T: FlameMessage + Send + 'static,
{
    ObjectFuture::new(async move {
        let bytes = fetch_object_bytes(&reference).await?;
        T::decode(&bytes)
    })
}

pub async fn update_object<T>(reference: &ObjectRef, object: &T) -> Result<ObjectRef, FlameError>
where
    T: FlameMessage,
{
    let bytes = object.encode()?;
    update_object_bytes(reference, bytes).await
}

async fn update_object_bytes(
    reference: &ObjectRef,
    bytes: impl Into<Vec<u8>>,
) -> Result<ObjectRef, FlameError> {
    ObjectKey::from_key(&reference.key)?;
    let endpoint = CacheEndpoint::parse(&reference.endpoint)?;
    let tls = current_cache_tls()?;
    let descriptor = FlightDescriptor::new_path(vec![reference.key.clone()]);
    do_put_bytes(&endpoint, tls.as_ref(), descriptor, bytes.into()).await
}

pub async fn patch_object<T>(reference: &ObjectRef, delta: &T) -> Result<ObjectRef, FlameError>
where
    T: FlameMessage,
{
    let bytes = delta.encode()?;
    patch_object_bytes(reference, bytes).await
}

async fn patch_object_bytes(
    reference: &ObjectRef,
    bytes: impl Into<Vec<u8>>,
) -> Result<ObjectRef, FlameError> {
    ObjectKey::from_key(&reference.key)?;
    let endpoint = CacheEndpoint::parse(&reference.endpoint)?;
    let tls = current_cache_tls()?;
    let descriptor = FlightDescriptor::new_cmd(format!("PATCH:{}", reference.key));
    do_put_bytes(&endpoint, tls.as_ref(), descriptor, bytes.into()).await
}

pub async fn delete_objects(key_prefix: impl AsRef<str>) -> Result<(), FlameError> {
    let object_key = ObjectKey::from_path(key_prefix.as_ref())?;
    let context = FlameContext::from_file_with_env(None)?;
    let cache = cache_from_context(&context)?;
    let mut client = flight_client(connect_cache(&cache.endpoint, cache.tls.as_ref()).await?);
    let results: Vec<_> = client
        .do_action(Action::new("DELETE", object_key.to_string().into_bytes()))
        .await
        .map_err(|e| FlameError::Internal(format!("cache delete failed: {}", e)))?
        .try_collect()
        .await
        .map_err(|e| FlameError::Internal(format!("cache delete failed: {}", e)))?;

    if results.is_empty() {
        return Err(FlameError::Internal(
            "cache delete returned no result".to_string(),
        ));
    }
    Ok(())
}

pub async fn upload_object(
    key_or_prefix: impl AsRef<str>,
    file_path: impl AsRef<Path>,
) -> Result<ObjectRef, FlameError> {
    let context = FlameContext::from_file_with_env(None)?;
    upload_object_with_context(&context, key_or_prefix, file_path).await
}

pub async fn upload_object_with_context(
    context: &FlameContext,
    key_or_prefix: impl AsRef<str>,
    file_path: impl AsRef<Path>,
) -> Result<ObjectRef, FlameError> {
    let object_key = ObjectKey::from_path(key_or_prefix.as_ref())?;
    if object_key.is_all_sessions() {
        return Err(FlameError::InvalidConfig(format!(
            "invalid object key path '{}'",
            key_or_prefix.as_ref()
        )));
    }
    let cache = cache_from_context(context)?;
    let descriptor = FlightDescriptor::new_path(vec![object_key.to_string()]);
    do_put_file(
        &cache.endpoint,
        cache.tls.as_ref(),
        descriptor,
        file_path.as_ref(),
    )
    .await
}

pub async fn download_object(
    reference: &ObjectRef,
    dest_path: impl AsRef<Path>,
) -> Result<(), FlameError> {
    ObjectKey::from_key(&reference.key)?;
    let endpoint = CacheEndpoint::parse(&reference.endpoint)?;
    let tls = current_cache_tls()?;
    let mut client = flight_client(connect_cache(&endpoint, tls.as_ref()).await?);
    let mut stream = client
        .do_get(Ticket::new(format!("{}:0", reference.key)))
        .await
        .map_err(|e| FlameError::Internal(format!("cache download failed: {}", e)))?;

    if let Some(parent) = dest_path.as_ref().parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            FlameError::Internal(format!(
                "failed to create download directory {}: {}",
                parent.display(),
                e
            ))
        })?;
    }

    let temp_path = dest_path.as_ref().with_extension("tmp");
    let mut file = tokio::fs::File::create(&temp_path).await.map_err(|e| {
        FlameError::Internal(format!("failed to create {}: {}", temp_path.display(), e))
    })?;
    let mut total_size = 0usize;

    while let Some(batch) = stream
        .try_next()
        .await
        .map_err(|e| FlameError::Internal(format!("cache download failed: {}", e)))?
    {
        let data = data_column(&batch)?;
        if let Some(kind) = kind_column(&batch)? {
            for row in 0..batch.num_rows() {
                match kind.value(row) {
                    OBJECT_KIND_BASE => {
                        let chunk = data.value(row);
                        file.write_all(chunk).await.map_err(|e| {
                            FlameError::Internal(format!("failed to write download chunk: {}", e))
                        })?;
                        total_size += chunk.len();
                    }
                    OBJECT_KIND_PATCH => {
                        drop(file);
                        tokio::fs::remove_file(&temp_path).await.ok();
                        return Err(FlameError::InvalidConfig(format!(
                            "object {} contains patch rows and cannot be downloaded as a file",
                            reference.key
                        )));
                    }
                    other => {
                        drop(file);
                        tokio::fs::remove_file(&temp_path).await.ok();
                        return Err(FlameError::InvalidConfig(format!(
                            "invalid object response kind '{}'",
                            other
                        )));
                    }
                }
            }
        } else {
            for row in 0..batch.num_rows() {
                let chunk = data.value(row);
                file.write_all(chunk).await.map_err(|e| {
                    FlameError::Internal(format!("failed to write download chunk: {}", e))
                })?;
                total_size += chunk.len();
            }
        }
    }

    if total_size == 0 {
        tokio::fs::remove_file(&temp_path).await.ok();
        return Err(FlameError::NotFound(reference.key.clone()));
    }

    file.sync_all()
        .await
        .map_err(|e| FlameError::Internal(format!("failed to sync download: {}", e)))?;
    drop(file);
    tokio::fs::rename(&temp_path, dest_path.as_ref())
        .await
        .map_err(|e| FlameError::Internal(format!("failed to finish download: {}", e)))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct CacheConfig {
    endpoint: CacheEndpoint,
    tls: Option<FlameClientTls>,
}

#[derive(Debug, Clone)]
struct CacheEndpoint {
    scheme: String,
    host: String,
    port: u16,
}

impl CacheEndpoint {
    fn parse(raw: &str) -> Result<Self, FlameError> {
        let parsed = Url::parse(raw)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid cache endpoint: {}", e)))?;
        let scheme = match parsed.scheme() {
            "grpc" => "grpc",
            "grpcs" | "grpc+tls" => "grpcs",
            scheme => {
                return Err(FlameError::InvalidConfig(format!(
                    "unsupported cache endpoint scheme <{}>; expected grpc, grpcs, or grpc+tls",
                    scheme
                )));
            }
        }
        .to_string();
        let host = parsed
            .host_str()
            .ok_or_else(|| FlameError::InvalidConfig("cache endpoint missing host".to_string()))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = parsed.port().unwrap_or(DEFAULT_CACHE_PORT);
        Ok(Self { scheme, host, port })
    }

    fn uri_host(&self) -> String {
        host_for_uri(&self.host)
    }
}

fn validate_component(name: &str, value: &str, reject_wildcard: bool) -> Result<(), FlameError> {
    if value.is_empty() {
        return Err(FlameError::InvalidConfig(format!("{name} cannot be empty")));
    }
    if value.contains("..") || value.contains('\\') || value.contains('/') {
        return Err(FlameError::InvalidConfig(format!(
            "{name} contains invalid characters: '{}'",
            value
        )));
    }
    if reject_wildcard && value == WILDCARD_SESSION {
        return Err(FlameError::InvalidConfig(format!(
            "wildcard '*' not allowed for {name}"
        )));
    }
    Ok(())
}

fn cache_from_context(context: &FlameContext) -> Result<CacheConfig, FlameError> {
    let current = context.get_current_context()?;
    let cache = current
        .cache
        .as_ref()
        .ok_or_else(|| FlameError::InvalidConfig("cache configuration not found".to_string()))?;
    let endpoint = cache_endpoint(cache)?;
    Ok(CacheConfig {
        endpoint,
        tls: cache.tls.clone(),
    })
}

fn cache_endpoint(cache: &FlameClientCache) -> Result<CacheEndpoint, FlameError> {
    let endpoint = cache
        .endpoint
        .as_deref()
        .ok_or_else(|| FlameError::InvalidConfig("cache endpoint not configured".to_string()))?;
    CacheEndpoint::parse(endpoint)
}

fn current_cache_tls() -> Result<Option<FlameClientTls>, FlameError> {
    let Ok(context) = FlameContext::from_file_with_env(None) else {
        return Ok(None);
    };
    Ok(context
        .get_current_context()
        .ok()
        .and_then(|current| current.cache.as_ref())
        .and_then(|cache| cache.tls.clone()))
}

async fn connect_cache(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
) -> Result<Channel, FlameError> {
    let transport_endpoint = if endpoint.scheme == "grpcs" {
        format!("https://{}:{}", endpoint.uri_host(), endpoint.port)
    } else {
        format!("http://{}:{}", endpoint.uri_host(), endpoint.port)
    };

    let mut builder = Channel::from_shared(transport_endpoint)
        .map_err(|e| FlameError::Internal(format!("invalid cache endpoint: {}", e)))?
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS));

    if endpoint.scheme == "grpcs" {
        if let Some(tls) = tls {
            builder = builder
                .tls_config(tls.client_tls_config(&endpoint.host)?)
                .map_err(|e| FlameError::Internal(format!("cache TLS config error: {}", e)))?;
        }
    }

    builder
        .connect()
        .await
        .map_err(|e| FlameError::Internal(format!("failed to connect to object cache: {}", e)))
}

fn flight_client(channel: Channel) -> FlightClient {
    let inner = FlightServiceClient::new(channel)
        .max_decoding_message_size(FLIGHT_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(FLIGHT_MAX_MESSAGE_SIZE);
    FlightClient::new_from_inner(inner)
}

async fn do_put_bytes(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
    descriptor: FlightDescriptor,
    bytes: Vec<u8>,
) -> Result<ObjectRef, FlameError> {
    let schema = object_schema();
    let batch = object_record_batch(schema.clone(), bytes)
        .map_err(|e| FlameError::Internal(format!("failed to create object batch: {}", e)))?;
    let stream = stream::iter(vec![Ok::<RecordBatch, FlightError>(batch)]);
    do_put_batches(endpoint, tls, descriptor, schema, stream).await
}

async fn do_put_file(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
    descriptor: FlightDescriptor,
    path: &Path,
) -> Result<ObjectRef, FlameError> {
    let file = tokio::fs::File::open(path).await.map_err(|e| {
        FlameError::InvalidConfig(format!("failed to open {}: {}", path.display(), e))
    })?;
    let schema = object_schema();
    let schema_for_stream = schema.clone();
    let stream = stream::try_unfold((file, schema_for_stream), |(mut file, schema)| async move {
        let mut chunk = vec![0_u8; UPLOAD_CHUNK_SIZE];
        let read = file.read(&mut chunk).await.map_err(|e| {
            FlightError::from_external_error(Box::new(std::io::Error::new(
                e.kind(),
                format!("failed to read object chunk: {}", e),
            )))
        })?;
        if read == 0 {
            return Ok(None);
        }
        chunk.truncate(read);
        let batch = object_record_batch(schema.clone(), chunk)?;
        Ok(Some((batch, (file, schema))))
    });
    do_put_batches(endpoint, tls, descriptor, schema, stream).await
}

async fn do_put_batches<S>(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
    descriptor: FlightDescriptor,
    schema: Arc<Schema>,
    batches: S,
) -> Result<ObjectRef, FlameError>
where
    S: futures::Stream<Item = Result<RecordBatch, FlightError>> + Send + 'static,
{
    let channel = connect_cache(endpoint, tls).await?;
    let mut client = flight_client(channel);
    let flight_data = FlightDataEncoderBuilder::new()
        .with_schema(schema)
        .with_flight_descriptor(Some(descriptor))
        .build(batches);

    let results: Vec<_> = client
        .do_put(flight_data)
        .await
        .map_err(|e| FlameError::Internal(format!("cache upload failed: {}", e)))?
        .try_collect()
        .await
        .map_err(|e| FlameError::Internal(format!("cache upload failed: {}", e)))?;
    let result = results
        .into_iter()
        .next()
        .ok_or_else(|| FlameError::Internal("cache upload returned no result".to_string()))?;
    ObjectRef::decode(result.app_metadata)
}

async fn fetch_object_bytes(reference: &ObjectRef) -> Result<Vec<u8>, FlameError> {
    ObjectKey::from_key(&reference.key)?;
    let endpoint = CacheEndpoint::parse(&reference.endpoint)?;
    let tls = current_cache_tls()?;
    let mut client = flight_client(connect_cache(&endpoint, tls.as_ref()).await?);
    let mut stream = client
        .do_get(Ticket::new(format!("{}:0", reference.key)))
        .await
        .map_err(|e| FlameError::Internal(format!("cache get failed: {}", e)))?;

    let mut base = Vec::new();
    let mut found_base = false;
    while let Some(batch) = stream
        .try_next()
        .await
        .map_err(|e| FlameError::Internal(format!("cache get failed: {}", e)))?
    {
        if batch.num_rows() == 0 {
            continue;
        }
        let data = data_column(&batch)?;
        if let Some(kind) = kind_column(&batch)? {
            for row in 0..batch.num_rows() {
                match kind.value(row) {
                    OBJECT_KIND_BASE => {
                        base.extend_from_slice(data.value(row));
                        found_base = true;
                    }
                    OBJECT_KIND_PATCH => {}
                    other => {
                        return Err(FlameError::InvalidConfig(format!(
                            "invalid object response kind '{}'",
                            other
                        )))
                    }
                }
            }
        } else {
            for row in 0..batch.num_rows() {
                base.extend_from_slice(data.value(row));
                found_base = true;
            }
        }
    }

    if found_base {
        Ok(base)
    } else {
        Err(FlameError::NotFound(reference.key.clone()))
    }
}

fn object_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(OBJECT_FIELD_VERSION, DataType::UInt64, false),
        Field::new(OBJECT_FIELD_DATA, DataType::Binary, false),
    ]))
}

fn object_record_batch(
    schema: Arc<Schema>,
    bytes: Vec<u8>,
) -> arrow_flight::error::Result<RecordBatch> {
    let version_array = UInt64Array::from(vec![0_u64]);
    let data_array = BinaryArray::from(vec![bytes.as_slice()]);
    RecordBatch::try_new(schema, vec![Arc::new(version_array), Arc::new(data_array)])
        .map_err(FlightError::from)
}

fn data_column(batch: &RecordBatch) -> Result<&BinaryArray, FlameError> {
    let array = batch.column_by_name(OBJECT_FIELD_DATA).ok_or_else(|| {
        FlameError::InvalidConfig("object response missing data column".to_string())
    })?;
    array
        .as_any()
        .downcast_ref::<BinaryArray>()
        .ok_or_else(|| FlameError::InvalidConfig("invalid object data column".to_string()))
}

fn kind_column(batch: &RecordBatch) -> Result<Option<&StringArray>, FlameError> {
    let Some(array) = batch.column_by_name(OBJECT_FIELD_KIND) else {
        return Ok(None);
    };
    Ok(Some(
        array
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| FlameError::InvalidConfig("invalid object kind column".to_string()))?,
    ))
}

fn object_ref_from_doc(doc: Document) -> Result<ObjectRef, FlameError> {
    let endpoint = doc
        .get_str("endpoint")
        .map_err(|e| FlameError::InvalidConfig(format!("ObjectRef missing endpoint: {}", e)))?
        .to_string();
    let key = doc
        .get_str("key")
        .map_err(|e| FlameError::InvalidConfig(format!("ObjectRef missing key: {}", e)))?
        .to_string();
    let version = match doc.get("version") {
        Some(Bson::Int64(value)) if *value >= 0 => *value as u64,
        Some(Bson::Int32(value)) if *value >= 0 => *value as u64,
        Some(other) => {
            return Err(FlameError::InvalidConfig(format!(
                "invalid ObjectRef version: {}",
                other
            )))
        }
        None => 0,
    };
    ObjectRef::new(endpoint, key, version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, DeriveSerialize, Deserialize, PartialEq)]
    struct SampleObject {
        name: String,
        count: u32,
    }

    impl FlameMessage for SampleObject {
        fn encode(&self) -> Result<Bytes, FlameError> {
            serde_json::to_vec(self)
                .map(Bytes::from)
                .map_err(|e| FlameError::Internal(e.to_string()))
        }

        fn decode(bytes: &[u8]) -> Result<Self, FlameError> {
            serde_json::from_slice(bytes).map_err(|e| FlameError::InvalidConfig(e.to_string()))
        }
    }

    #[test]
    fn object_key_parses_prefix_and_full_key() {
        let prefix = ObjectKey::from_prefix("app/session").unwrap();
        assert_eq!(prefix.app_name, "app");
        assert_eq!(prefix.session_id, "session");
        assert_eq!(prefix.object_id, None);
        assert_eq!(prefix.to_string(), "app/session");
        assert!(prefix.matches_key("app/session/object"));
        assert!(!prefix.matches_key("app/other/object"));

        let full = ObjectKey::from_key("app/session/object").unwrap();
        assert_eq!(full.object_id.as_deref(), Some("object"));
        assert_eq!(full.to_string(), "app/session/object");
        assert!(full.matches_key("app/session/object"));
        assert!(!full.matches_key("app/session/other"));
    }

    #[test]
    fn object_key_rejects_unsafe_components() {
        assert!(ObjectKey::from_path("../session").is_err());
        assert!(ObjectKey::from_path("app/session/").is_err());
        assert!(ObjectKey::from_path("app/*/object").is_err());
        assert!(ObjectKey::from_path("*/session").is_err());
    }

    #[test]
    fn object_key_supports_shared_and_wildcard_helpers() {
        let shared = ObjectKey::for_shared("app").unwrap();
        assert_eq!(shared.to_string(), "app/shared");

        let wildcard = ObjectKey::for_all_sessions("app").unwrap();
        assert!(wildcard.is_all_sessions());
        assert!(wildcard.matches_key("app/session/object"));
        assert!(!wildcard.matches_key("other/session/object"));
        assert!(wildcard.with_generated_id().is_err());
    }

    #[test]
    fn object_ref_encodes_bson() {
        let reference = ObjectRef::new("grpc://cache:9090", "app/session/object", 7).unwrap();
        let encoded = reference.encode().unwrap();
        let decoded = ObjectRef::decode(encoded).unwrap();
        assert_eq!(decoded, reference);
    }

    #[test]
    fn cache_endpoint_accepts_grpc_tls_alias() {
        let endpoint = CacheEndpoint::parse("grpc+tls://cache.example.com:9443").unwrap();
        assert_eq!(endpoint.scheme, "grpcs");
        assert_eq!(endpoint.host, "cache.example.com");
        assert_eq!(endpoint.port, 9443);
    }

    #[test]
    fn cache_endpoint_brackets_ipv6_for_uri() {
        let endpoint = CacheEndpoint::parse("grpc://[2001:db8::1]:9090").unwrap();
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.uri_host(), "[2001:db8::1]");
    }

    #[test]
    fn typed_object_uses_flame_message_codec() {
        let object = SampleObject {
            name: "demo".to_string(),
            count: 42,
        };
        let bytes = object.encode().unwrap();
        let decoded = SampleObject::decode(&bytes).unwrap();
        assert_eq!(decoded, object);
    }

    #[tokio::test]
    async fn object_future_polls_inner_future() {
        let value = ObjectFuture::new(async { Ok::<_, FlameError>(42_u32) })
            .await
            .unwrap();
        assert_eq!(value, 42);
    }
}

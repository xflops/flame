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
use std::task::{Context, Poll};
use std::time::Duration;
use stdng::{lock_ptr, new_ptr};

use bson::{doc, Bson, Document};
use bytes::Bytes;
use futures::stream;
use serde_derive::{Deserialize, Serialize as DeriveSerialize};
use tokio::io::AsyncReadExt;
use tonic::transport::Channel;
use tonic::Request;
use url::Url;

use crate::apis::flame::v1::object_cache_service_client::ObjectCacheServiceClient;
use crate::apis::flame::v1::{
    cache_get_response, cache_write_request, CacheDeleteRequest, CacheGetChunk, CacheGetHeader,
    CacheGetRequest, CacheWriteHeader, CacheWriteRequest,
};
use crate::apis::{FlameClientCache, FlameClientTls, FlameContext, FlameError};
use crate::message::FlameMessage;

const WILDCARD_SESSION: &str = "*";
const DEFAULT_CACHE_PORT: u16 = 9090;
const CONNECT_TIMEOUT_SECS: u64 = 30;
const UPLOAD_CHUNK_SIZE: usize = 1024 * 1024;
const CACHE_TARGET_HEADER: &str = "x-flame-object-cache";

/// An object as the cache returns it: its data parts in the order they were
/// written, the first at version 1, and their client-defined type.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ObjectParts {
    data_type: String,
    version: u64,
    parts: Vec<Bytes>,
}

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

    /// The value of the object: see `get_object`.
    pub fn get<T>(&self) -> ObjectFuture<T>
    where
        T: TryFrom<Vec<Bytes>, Error = FlameError> + Send + 'static,
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
    let context = FlameContext::load(None)?;
    let object_key = ObjectKey::from_prefix(key_prefix.as_ref())?;
    let cache = cache_from_context(&context)?;
    do_put_bytes(
        &cache.endpoint,
        cache.tls.as_ref(),
        object_key.to_prefix(),
        false,
        object.encode()?,
        "raw",
    )
    .await
}

/// Get an object as a `T` made of its data parts, in the order they were
/// written (the first is the data the object was created with). A type that
/// derives `FlameMessage` is made of the newest part.
pub fn get_object<T>(reference: ObjectRef) -> ObjectFuture<T>
where
    T: TryFrom<Vec<Bytes>, Error = FlameError> + Send + 'static,
{
    ObjectFuture::new(async move {
        let object = fetch_object(&reference).await?;
        if object.data_type != "raw" {
            return Err(FlameError::InvalidConfig(format!(
                "object {} has type {}, expected raw",
                reference.key, object.data_type
            )));
        }
        T::try_from(object.parts)
    })
}

pub async fn patch_object<T>(reference: &ObjectRef, data: &T) -> Result<ObjectRef, FlameError>
where
    T: FlameMessage,
{
    ObjectKey::from_key(&reference.key)?;
    let endpoint = endpoint_for_reference(&reference.endpoint)?;
    let tls = current_cache_tls()?;
    do_put_bytes(
        &endpoint,
        tls.as_ref(),
        reference.key.clone(),
        true,
        data.encode()?,
        "raw",
    )
    .await
}

pub async fn delete_objects(key_prefix: impl AsRef<str>) -> Result<(), FlameError> {
    let context = FlameContext::load(None)?;
    delete_with_context(&context, key_prefix.as_ref()).await
}

async fn delete_with_context(context: &FlameContext, key_prefix: &str) -> Result<(), FlameError> {
    let object_key = ObjectKey::from_path(key_prefix)?;
    let cache = cache_from_context(context)?;
    let mut client =
        ObjectCacheServiceClient::new(connect_cache(&cache.endpoint, cache.tls.as_ref()).await?);
    client
        .delete(CacheDeleteRequest {
            key: object_key.to_string(),
        })
        .await
        .map_err(|e| FlameError::Internal(format!("cache delete failed: {}", e)))?;
    Ok(())
}

/// Application packages are objects of the `pkg` session of their application.
const PACKAGE_SESSION: &str = "pkg";

/// The key of the package `filename` of an application: `<app>/pkg/<filename>`.
pub fn package_key(app_name: &str, filename: &str) -> String {
    format!("{}/{}/{}", app_name, PACKAGE_SESSION, filename)
}

/// Upload the application package `file_path` as `<app>/pkg/<filename>`, with
/// the cache configured by `context` (the other helpers use the default
/// context). An object is never replaced, so a package of the same name is
/// deleted first.
pub async fn upload_package(
    context: &FlameContext,
    app_name: &str,
    filename: &str,
    file_path: impl AsRef<Path>,
) -> Result<ObjectRef, FlameError> {
    let key = package_key(app_name, filename);
    ObjectKey::from_key(&key)?;
    let cache = cache_from_context(context)?;
    delete_with_context(context, &key).await?;
    do_put_file(
        &cache.endpoint,
        cache.tls.as_ref(),
        key,
        file_path.as_ref(),
        "raw",
    )
    .await
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
    target: Option<String>,
}

impl CacheEndpoint {
    fn parse(raw: &str) -> Result<Self, FlameError> {
        let parsed = Url::parse(raw)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid cache endpoint: {}", e)))?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(FlameError::InvalidConfig(
                "cache endpoint must not contain credentials, a path, query, or fragment"
                    .to_string(),
            ));
        }
        let scheme = match parsed.scheme() {
            "grpc" => "grpc",
            "grpcs" | "grpc+tls" => "grpcs",
            "grpcs-proxy" => "grpcs-proxy",
            scheme => {
                return Err(FlameError::InvalidConfig(format!(
                    "unsupported cache endpoint scheme <{}>; expected grpc, grpcs, grpc+tls, or grpcs-proxy",
                    scheme
                )));
            }
        }
        .to_string();
        if scheme == "grpcs-proxy" && parsed.port().is_none() {
            return Err(FlameError::InvalidConfig(
                "grpcs-proxy endpoint requires an explicit port".to_string(),
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| FlameError::InvalidConfig("cache endpoint missing host".to_string()))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = parsed.port().unwrap_or(DEFAULT_CACHE_PORT);
        Ok(Self {
            scheme,
            host,
            port,
            target: None,
        })
    }

    fn uri_host(&self) -> String {
        host_for_uri(&self.host)
    }

    fn proxy_for(mut self, origin: &Self) -> Self {
        self.target = Some(format!("{}:{}", origin.uri_host(), origin.port));
        self
    }
}

fn cache_request<T>(endpoint: &CacheEndpoint, message: T) -> Result<Request<T>, FlameError> {
    let mut request = Request::new(message);
    if let Some(target) = endpoint.target.as_deref() {
        let value = target.parse().map_err(|e| {
            FlameError::InvalidConfig(format!("invalid cache proxy target <{}>: {}", target, e))
        })?;
        request.metadata_mut().insert(CACHE_TARGET_HEADER, value);
    }
    Ok(request)
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

fn host_for_uri(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
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
    let Some(context) = optional_context()? else {
        return Ok(None);
    };
    Ok(context
        .get_current_context()?
        .cache
        .as_ref()
        .and_then(|cache| cache.tls.clone()))
}

fn endpoint_for_reference(reference_endpoint: &str) -> Result<CacheEndpoint, FlameError> {
    let origin = CacheEndpoint::parse(reference_endpoint)?;
    let Some(context) = optional_context()? else {
        return Ok(origin);
    };
    let Some(cache) = context.get_current_context()?.cache.as_ref() else {
        return Ok(origin);
    };
    let Some(configured_endpoint) = cache.endpoint.as_deref() else {
        return Ok(origin);
    };
    if !configured_endpoint.starts_with("grpcs-proxy://") {
        return Ok(origin);
    }
    let proxy = CacheEndpoint::parse(configured_endpoint)?;
    Ok(proxy.proxy_for(&origin))
}

fn optional_context() -> Result<Option<FlameContext>, FlameError> {
    let config_path = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".flame/flame.yaml");
    if !config_path.is_file() {
        let has_env_config = [
            "FLAME_ENDPOINT",
            "FLAME_CACHE_ENDPOINT",
            "FLAME_CA_FILE",
            "FLAME_CERT_FILE",
            "FLAME_KEY_FILE",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
        return if has_env_config {
            FlameContext::from_env().map(Some)
        } else {
            Ok(None)
        };
    }
    FlameContext::load(Some(config_path.to_string_lossy().into_owned())).map(Some)
}

async fn connect_cache(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
) -> Result<Channel, FlameError> {
    let transport_endpoint = if matches!(endpoint.scheme.as_str(), "grpcs" | "grpcs-proxy") {
        format!("https://{}:{}", endpoint.uri_host(), endpoint.port)
    } else {
        format!("http://{}:{}", endpoint.uri_host(), endpoint.port)
    };

    let mut builder = Channel::from_shared(transport_endpoint)
        .map_err(|e| FlameError::Internal(format!("invalid cache endpoint: {}", e)))?
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS));

    if matches!(endpoint.scheme.as_str(), "grpcs" | "grpcs-proxy") {
        let tls = tls.cloned().unwrap_or_default();
        builder = builder
            .tls_config(tls.client_tls_config(&endpoint.host)?)
            .map_err(|e| FlameError::Internal(format!("cache TLS config error: {}", e)))?;
    }

    builder
        .connect()
        .await
        .map_err(|e| FlameError::Internal(format!("failed to connect to object cache: {}", e)))
}

fn put_header(key: String, data_type: &str) -> CacheWriteRequest {
    CacheWriteRequest {
        payload: Some(cache_write_request::Payload::Header(CacheWriteHeader {
            key,
            data_type: data_type.to_string(),
        })),
    }
}

fn put_chunk(data: Bytes) -> CacheWriteRequest {
    CacheWriteRequest {
        payload: Some(cache_write_request::Payload::Data(data)),
    }
}

fn write_bytes_stream(
    key: String,
    data: Bytes,
    data_type: &str,
) -> impl futures::Stream<Item = CacheWriteRequest> {
    let header = put_header(key, data_type);
    stream::unfold(
        (Some(header), data, 0usize),
        |(header, data, offset)| async move {
            if let Some(header) = header {
                return Some((header, (None, data, offset)));
            }
            if offset >= data.len() {
                return None;
            }
            let end = (offset + UPLOAD_CHUNK_SIZE).min(data.len());
            let chunk = put_chunk(data.slice(offset..end));
            Some((chunk, (None, data, end)))
        },
    )
}

fn object_ref_from_metadata(
    metadata: crate::apis::flame::v1::CacheObjectMetadata,
) -> Result<ObjectRef, FlameError> {
    ObjectRef::new(metadata.endpoint, metadata.key, metadata.version)
}

async fn do_put_bytes(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
    key: String,
    patch: bool,
    data: Bytes,
    data_type: &str,
) -> Result<ObjectRef, FlameError> {
    let input = write_bytes_stream(key, data, data_type);
    let mut client = ObjectCacheServiceClient::new(connect_cache(endpoint, tls).await?);
    let metadata = if patch {
        client.patch(cache_request(endpoint, input)?).await
    } else {
        client.put(cache_request(endpoint, input)?).await
    }
    .map_err(|e| FlameError::Internal(format!("cache upload failed: {}", e)))?
    .into_inner();
    object_ref_from_metadata(metadata)
}

async fn do_put_file(
    endpoint: &CacheEndpoint,
    tls: Option<&FlameClientTls>,
    key: String,
    path: &Path,
    data_type: &str,
) -> Result<ObjectRef, FlameError> {
    let file = tokio::fs::File::open(path).await.map_err(|e| {
        FlameError::InvalidConfig(format!("failed to open {}: {}", path.display(), e))
    })?;
    let read_error = new_ptr(None);
    let error_for_stream = read_error.clone();
    let header = put_header(key, data_type);
    let input = stream::unfold((file, Some(header)), move |(mut file, header)| {
        let read_error = error_for_stream.clone();
        async move {
            if let Some(header) = header {
                return Some((header, (file, None)));
            }
            let mut chunk = vec![0_u8; UPLOAD_CHUNK_SIZE];
            match file.read(&mut chunk).await {
                Ok(0) => None,
                Ok(count) => {
                    chunk.truncate(count);
                    Some((put_chunk(Bytes::from(chunk)), (file, None)))
                }
                Err(error) => {
                    *lock_ptr!(read_error).expect("upload error mutex poisoned") = Some(error);
                    None
                }
            }
        }
    });
    let mut client = ObjectCacheServiceClient::new(connect_cache(endpoint, tls).await?);
    let response = client.put(cache_request(endpoint, input)?).await;
    if let Some(error) = lock_ptr!(read_error)
        .expect("upload error mutex poisoned")
        .take()
    {
        return Err(FlameError::Internal(format!(
            "failed to read object chunk: {}",
            error
        )));
    }
    let metadata = response
        .map_err(|e| FlameError::Internal(format!("cache upload failed: {}", e)))?
        .into_inner();
    object_ref_from_metadata(metadata)
}

async fn read_header(
    stream: &mut tonic::Streaming<crate::apis::flame::v1::CacheGetResponse>,
) -> Result<CacheGetHeader, FlameError> {
    let first = stream
        .message()
        .await
        .map_err(|e| FlameError::Internal(format!("cache get failed: {}", e)))?
        .ok_or_else(|| FlameError::InvalidConfig("cache response missing header".to_string()))?;
    let Some(cache_get_response::Payload::Header(header)) = first.payload else {
        return Err(FlameError::InvalidConfig(
            "cache response must start with header".to_string(),
        ));
    };
    Ok(header)
}

async fn fetch_object(reference: &ObjectRef) -> Result<ObjectParts, FlameError> {
    ObjectKey::from_key(&reference.key)?;
    let endpoint = endpoint_for_reference(&reference.endpoint)?;
    let tls = current_cache_tls()?;
    let mut client = ObjectCacheServiceClient::new(connect_cache(&endpoint, tls.as_ref()).await?);
    let mut stream = client
        .get(cache_request(
            &endpoint,
            CacheGetRequest {
                key: reference.key.clone(),
                version: 0,
            },
        )?)
        .await
        .map_err(|e| FlameError::Internal(format!("cache get failed: {}", e)))?
        .into_inner();
    let header = read_header(&mut stream).await?;
    let mut builder = ObjectPartsBuilder::new(header);
    while let Some(message) = stream
        .message()
        .await
        .map_err(|e| FlameError::Internal(format!("cache get failed: {}", e)))?
    {
        let Some(cache_get_response::Payload::Chunk(chunk)) = message.payload else {
            return Err(FlameError::InvalidConfig(
                "invalid object response: expected data chunk".to_string(),
            ));
        };
        builder.push(chunk)?;
    }
    builder.finish(&reference.key)
}

struct ObjectPartsBuilder {
    data_type: String,
    version: u64,
    /// The data parts by version, starting at 1.
    parts: Vec<(u64, Vec<u8>)>,
}

impl ObjectPartsBuilder {
    fn new(header: CacheGetHeader) -> Self {
        Self {
            data_type: header.data_type,
            version: header.version,
            parts: Vec::new(),
        }
    }

    fn push(&mut self, chunk: CacheGetChunk) -> Result<(), FlameError> {
        match self.parts.last_mut() {
            Some((version, data)) if *version == chunk.version => {
                data.extend_from_slice(&chunk.data);
            }
            Some((version, _)) if *version + 1 != chunk.version => {
                return Err(FlameError::InvalidConfig(
                    "invalid object response: versions must be consecutive".to_string(),
                ));
            }
            None if chunk.version != 1 => {
                return Err(FlameError::InvalidConfig(
                    "invalid object response: the data must start at version 1".to_string(),
                ));
            }
            _ => self.parts.push((chunk.version, chunk.data.to_vec())),
        }
        Ok(())
    }

    fn finish(self, key: &str) -> Result<ObjectParts, FlameError> {
        if self.parts.is_empty() {
            return Err(FlameError::NotFound(key.to_string()));
        }
        Ok(ObjectParts {
            data_type: self.data_type,
            version: self.version,
            parts: self
                .parts
                .into_iter()
                .map(|(_, data)| Bytes::from(data))
                .collect(),
        })
    }
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
    use futures::StreamExt;

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
        assert!(endpoint.target.is_none());
    }

    #[test]
    fn cache_endpoint_accepts_grpcs_proxy() {
        let proxy = CacheEndpoint::parse("grpcs-proxy://gateway.example.com:9090").unwrap();
        let origin = CacheEndpoint::parse("grpc://cache-0.cache:9090").unwrap();
        let routed = proxy.clone().proxy_for(&origin);
        assert_eq!(routed.scheme, "grpcs-proxy");
        assert_eq!(routed.host, "gateway.example.com");
        assert_eq!(routed.port, 9090);
        assert_eq!(routed.target.as_deref(), Some("cache-0.cache:9090"));
        let request = cache_request(&routed, CacheGetRequest::default()).unwrap();
        assert_eq!(
            request.metadata().get(CACHE_TARGET_HEADER).unwrap(),
            "cache-0.cache:9090"
        );
        assert!(cache_request(&proxy, CacheGetRequest::default())
            .unwrap()
            .metadata()
            .get(CACHE_TARGET_HEADER)
            .is_none());
    }

    #[test]
    fn cache_endpoint_rejects_proxy_url_extras() {
        assert!(CacheEndpoint::parse("grpc-proxy://gateway:9090").is_err());
        assert!(CacheEndpoint::parse("grpcs-proxy://gateway:9090/path").is_err());
        assert!(CacheEndpoint::parse("grpcs-proxy://gateway:9090?host=cache:9090").is_err());
        assert!(CacheEndpoint::parse("grpcs-proxy://user@gateway:9090").is_err());
        assert!(CacheEndpoint::parse("grpcs-proxy://gateway").is_err());
    }

    #[test]
    fn cache_endpoint_brackets_ipv6_for_uri() {
        let endpoint = CacheEndpoint::parse("grpc://[2001:db8::1]:9090").unwrap();
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.uri_host(), "[2001:db8::1]");
    }

    #[test]
    fn env_only_context_routes_references() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("object::tests::env_only_context_child")
            .env("FLAME_ENV_CONTEXT_CHILD", "1")
            .env(
                "HOME",
                std::env::temp_dir().join("flame-env-only-no-config"),
            )
            .env("FLAME_ENDPOINT", "http://session.example.com:8080")
            .env(
                "FLAME_CACHE_ENDPOINT",
                "grpcs-proxy://gateway.example.com:9443",
            )
            .env("FLAME_CERT_FILE", "client.crt")
            .env("FLAME_KEY_FILE", "client.key")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn env_only_context_child() {
        if std::env::var_os("FLAME_ENV_CONTEXT_CHILD").is_none() {
            return;
        }
        let endpoint = endpoint_for_reference("grpc://cache-0.cache:9090").unwrap();
        assert_eq!(endpoint.host, "gateway.example.com");
        assert_eq!(endpoint.target.as_deref(), Some("cache-0.cache:9090"));
        let tls = current_cache_tls().unwrap().unwrap();
        assert_eq!(tls.cert_file.as_deref(), Some("client.crt"));
        assert_eq!(tls.key_file.as_deref(), Some("client.key"));
    }

    #[test]
    fn incomplete_env_context_fails_closed() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("object::tests::incomplete_env_context_child")
            .env("FLAME_INCOMPLETE_ENV_CHILD", "1")
            .env(
                "HOME",
                std::env::temp_dir().join("flame-env-only-no-config"),
            )
            .env_remove("FLAME_ENDPOINT")
            .env(
                "FLAME_CACHE_ENDPOINT",
                "grpcs-proxy://gateway.example.com:9443",
            )
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn incomplete_env_context_child() {
        if std::env::var_os("FLAME_INCOMPLETE_ENV_CHILD").is_none() {
            return;
        }
        assert!(endpoint_for_reference("grpc://cache-0.cache:9090").is_err());
        assert!(current_cache_tls().is_err());
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

    #[tokio::test]
    async fn byte_upload_stream_sends_header_then_bounded_chunks() {
        let data = vec![7_u8; UPLOAD_CHUNK_SIZE * 2 + 1];
        let messages: Vec<_> = write_bytes_stream(
            "app/session/object".to_string(),
            Bytes::from(data.clone()),
            "client.codec.v1",
        )
        .collect()
        .await;
        assert_eq!(messages.len(), 4);
        match &messages[0].payload {
            Some(cache_write_request::Payload::Header(header)) => {
                assert_eq!(header.key, "app/session/object");
                assert_eq!(header.data_type, "client.codec.v1");
            }
            _ => panic!("first message must be a header"),
        }
        let chunks: Vec<_> = messages[1..]
            .iter()
            .map(|message| match &message.payload {
                Some(cache_write_request::Payload::Data(data)) => data.as_ref(),
                _ => panic!("expected data chunk"),
            })
            .collect();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            vec![UPLOAD_CHUNK_SIZE, UPLOAD_CHUNK_SIZE, 1]
        );
        assert_eq!(chunks.concat(), data);

        let empty: Vec<_> =
            write_bytes_stream("app/session/object".to_string(), Bytes::new(), "raw")
                .collect()
                .await;
        assert_eq!(empty.len(), 1);
    }

    #[test]
    fn response_assembles_one_part_per_version() {
        let mut builder = ObjectPartsBuilder::new(CacheGetHeader {
            version: 3,
            data_type: "custom.binary".to_string(),
        });
        for (version, data) in [
            (1, b"fir".as_slice()),
            (1, b"st".as_slice()),
            (2, b"sec".as_slice()),
            (2, b"ond".as_slice()),
            (3, b"next".as_slice()),
        ] {
            builder
                .push(CacheGetChunk {
                    version,
                    data: Bytes::copy_from_slice(data),
                })
                .unwrap();
        }
        let object = builder.finish("app/session/object").unwrap();
        assert_eq!(object.data_type, "custom.binary");
        assert_eq!(object.version, 3);
        assert_eq!(
            object.parts,
            vec![
                Bytes::from_static(b"first"),
                Bytes::from_static(b"second"),
                Bytes::from_static(b"next"),
            ]
        );
    }

    #[test]
    fn response_must_start_at_version_one_and_be_consecutive() {
        let chunk = |version| CacheGetChunk {
            version,
            data: Bytes::new(),
        };
        let header = || CacheGetHeader {
            version: 3,
            data_type: "raw".to_string(),
        };
        let mut late = ObjectPartsBuilder::new(header());
        assert!(late.push(chunk(2)).is_err());
        let mut gap = ObjectPartsBuilder::new(header());
        gap.push(chunk(1)).unwrap();
        assert!(gap.push(chunk(3)).is_err());
        let empty = ObjectPartsBuilder::new(header());
        assert!(matches!(
            empty.finish("app/session/object"),
            Err(FlameError::NotFound(_))
        ));
    }
}

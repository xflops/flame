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
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use bytesize::ByteSize;
use common::net::host_for_uri;
use futures::stream::BoxStream;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use regex::Regex;
use stdng::{async_rw_ptr, lock_ptr, new_ptr, write_async, AsyncRWPtr, MutexPtr};
use url::Url;

use common::ctx::{FlameCache, FlameCluster};
use common::FlameError;

use crate::eviction::EvictionConfig;
use crate::gc::ApplicationGarbageCollector;
use crate::storage::{ObjectStat, StorageEnginePtr};

mod grpc;
mod response;

/// Chunks of a write, after its header.
type ChunkStream = BoxStream<'static, Result<Bytes, FlameError>>;

/// A write request: what the header names, and the data chunks that follow it.
struct WriteRequest {
    key: ObjectKey,
    data_type: String,
    chunks: ChunkStream,
}

use grpc::GrpcCacheServer;
use rpc::flame::v1::object_cache_service_server::ObjectCacheServiceServer;

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
        } else if let Some(full_key) = self.to_key() {
            key_str == full_key
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

fn current_time_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// An object is a list of `ObjectData` in the order it was written, like the
/// layers of an overlay filesystem: the first is written when the object is
/// created and every later one appends to it. All of them are alike; the
/// version of an object is its number of `ObjectData`. They share the object's
/// data type. Payload interpretation belongs to clients.
///
/// `version` says which parts of the object `data` holds: 0 (the default) is all
/// of them; above 0, `data` is only what was written after that version.
#[derive(Debug, Clone)]
pub struct Object {
    pub version: u64,
    pub data_type: String,
    pub data: Vec<ObjectData>,
}

impl Object {
    /// Create a default Object: version 0 (all the parts) with the raw data
    /// type and no data yet. Add data with `append_data`.
    pub fn new() -> Self {
        Self {
            version: 0,
            data_type: "raw".to_string(),
            data: Vec::new(),
        }
    }

    /// Append an `ObjectData` after the existing ones. Bytes convert into an
    /// `ObjectData` created now.
    pub fn append_data(mut self, data: impl Into<ObjectData>) -> Self {
        self.data.push(data.into());
        self
    }

    /// Set the data type.
    pub fn with_data_type(mut self, data_type: impl Into<String>) -> Self {
        self.data_type = data_type.into();
        self
    }

    #[cfg(test)]
    pub fn opaque_data(&self) -> Result<&[u8], FlameError> {
        Ok(&self.data[0].data)
    }

    #[cfg(test)]
    /// The version of the object: the parts before `data`, and those in it, one
    /// version each.
    pub fn current_version(&self) -> u64 {
        self.version + self.data.len() as u64
    }

    /// The part of this object after `version`: all of it for 0, else the parts
    /// after that version. None if `version` is outside what this
    /// object holds.
    #[cfg(test)]
    pub fn since(&self, version: u64) -> Option<Object> {
        let skip = version.checked_sub(self.version)? as usize;
        Some(Object {
            version,
            data_type: self.data_type.clone(),
            data: self.data.get(skip..)?.to_vec(),
        })
    }

    pub fn size_bytes(&self) -> u64 {
        self.data.iter().map(ObjectData::size_bytes).sum()
    }
}

/// One piece of an object's data, written at one time. It has no data type of
/// its own: the type belongs to the object.
#[derive(Debug, Clone)]
pub struct ObjectData {
    pub creation_time: i64,
    pub data: Bytes,
}

impl From<Vec<u8>> for ObjectData {
    fn from(data: Vec<u8>) -> Self {
        Self::new().with_data(data)
    }
}

impl From<Bytes> for ObjectData {
    fn from(data: Bytes) -> Self {
        Self::new().with_data(data)
    }
}

impl ObjectData {
    /// Create default data: no bytes, created now.
    pub fn new() -> Self {
        Self {
            creation_time: current_time_millis(),
            data: Bytes::new(),
        }
    }

    /// Set the creation time.
    pub fn with_creation_time(mut self, creation_time: i64) -> Self {
        self.creation_time = creation_time;
        self
    }

    /// Set the data.
    pub fn with_data(mut self, data: impl Into<Bytes>) -> Self {
        self.data = data.into();
        self
    }

    pub fn size_bytes(&self) -> u64 {
        self.data.len() as u64
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,
    pub size: u64,
    pub creation_time: i64,
    pub data_type: String,
}

impl ObjectMetadata {
    /// Metadata with default values: empty endpoint and key, version 0, empty
    /// payload, created now, and a "raw" data type.
    pub(crate) fn new() -> Self {
        Self {
            endpoint: String::new(),
            key: String::new(),
            version: 0,
            size: 0,
            creation_time: current_time_millis(),
            data_type: "raw".to_string(),
        }
    }

    pub(crate) fn with_endpoint(self, endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            ..self
        }
    }

    pub(crate) fn with_key(self, key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ..self
        }
    }

    pub(crate) fn with_version(self, version: u64) -> Self {
        Self { version, ..self }
    }

    pub(crate) fn with_size(self, size: u64) -> Self {
        Self { size, ..self }
    }

    pub(crate) fn with_creation_time(self, creation_time: i64) -> Self {
        Self {
            creation_time,
            ..self
        }
    }

    pub(crate) fn with_data_type(self, data_type: impl Into<String>) -> Self {
        Self {
            data_type: data_type.into(),
            ..self
        }
    }
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
    /// - grpcs -> grpc+tls (for client endpoint compatibility)
    fn to_uri(&self) -> String {
        let client_scheme = match self.scheme.as_str() {
            "grpcs" => "grpc+tls",
            other => other,
        };
        format!(
            "{}://{}:{}",
            client_scheme,
            host_for_uri(&self.host),
            self.port
        )
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
    /// Address placed in the references returned to clients.
    endpoint: CacheEndpoint,
    storage: StorageEnginePtr,
    metadata: MutexPtr<HashMap<String, ObjectMetadata>>,
    /// Per-key locks coordinate concurrent PUT/PATCH writes with GET snapshots.
    key_locks: MutexPtr<HashMap<String, AsyncRWPtr<()>>>,
}

impl ObjectCache {
    fn new(endpoint: CacheEndpoint, storage: StorageEnginePtr) -> Result<Self, FlameError> {
        Ok(Self {
            endpoint,
            storage,
            metadata: new_ptr(HashMap::new()),
            key_locks: new_ptr(HashMap::new()),
        })
    }

    /// Load object metadata only; objects become resident on demand.
    async fn load_from_storage(&self) -> Result<(), FlameError> {
        let items = self.storage.scan().await?;

        let mut metadata = lock_ptr!(self.metadata)?;
        for (key, stat) in items {
            let key_str = key.to_key().expect("scanned key must have object_id");
            let meta = self.stat_metadata(key_str.clone(), &stat);
            metadata.insert(key_str, meta);
        }

        Ok(())
    }

    fn get_key_lock(&self, key: &str) -> Result<AsyncRWPtr<()>, FlameError> {
        let mut locks = lock_ptr!(self.key_locks)?;
        Ok(locks
            .entry(key.to_string())
            .or_insert_with(|| async_rw_ptr(()))
            .clone())
    }

    fn stat_metadata(&self, key: String, stat: &ObjectStat) -> ObjectMetadata {
        ObjectMetadata::new()
            .with_endpoint(self.endpoint.to_uri())
            .with_key(key)
            .with_version(stat.version())
            .with_size(stat.size())
            .with_creation_time(stat.creation_time())
            .with_data_type(stat.data_type.clone())
    }

    fn metadata_of(&self, key: &str) -> Result<Option<ObjectMetadata>, FlameError> {
        let metadata = lock_ptr!(self.metadata)?;
        Ok(metadata.get(key).cloned())
    }

    #[cfg(test)]
    async fn put(&self, key: ObjectKey, object: Object) -> Result<ObjectMetadata, FlameError> {
        let chunks = futures::stream::once(futures::future::ready(Ok(object.data[0].data.clone())));
        self.put_stream(WriteRequest {
            key,
            data_type: object.data_type,
            chunks: Box::pin(chunks),
        })
        .await
    }

    /// Store an object from the chunks of a write request. Memory use is one chunk.
    async fn put_stream(&self, request: WriteRequest) -> Result<ObjectMetadata, FlameError> {
        let WriteRequest {
            key,
            data_type,
            chunks,
        } = request;
        let key = match key.object_id {
            Some(_) => key,
            None => key.with_generated_id(),
        };

        let key_str = key.to_key().ok_or_else(|| {
            FlameError::Internal("ObjectKey missing object_id in put".to_string())
        })?;

        // Acquire per-key lock to prevent concurrent version increments
        let key_lock = self.get_key_lock(&key_str)?;
        let _guard = write_async!(key_lock);

        // Objects are created once and then only appended to, never replaced.
        let exists = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata.contains_key(&key_str)
        } || self.storage.stat(&key).await?.is_some();
        if exists {
            return Err(FlameError::AlreadyExist(format!(
                "object <{}> already exists",
                key_str
            )));
        }

        let meta = ObjectMetadata::new()
            .with_key(key_str.clone())
            .with_version(1)
            .with_data_type(data_type);
        let meta = self
            .storage
            .write_stream(&key, meta, Box::pin(chunks))
            .await?
            .with_endpoint(self.endpoint.to_uri());
        {
            let mut metadata = lock_ptr!(self.metadata)?;
            metadata.insert(key_str.clone(), meta.clone());
        }

        tracing::debug!("Object put: {}", key_str);

        Ok(meta)
    }

    #[cfg(test)]
    async fn get(&self, key: &ObjectKey) -> Result<Arc<Object>, FlameError> {
        self.get_since(key, 0).await
    }

    /// Get an object after `version`: all the parts for 0, else only the parts
    /// after that version, like a `Get` request with a version.
    #[cfg(test)]
    async fn get_since(&self, key: &ObjectKey, version: u64) -> Result<Arc<Object>, FlameError> {
        self.storage
            .read_object(key, version)
            .await?
            .map(Arc::new)
            .ok_or_else(|| FlameError::NotFound(format!("object <{:?}> not found", key.to_key())))
    }

    async fn patch(&self, key: &ObjectKey, appended: Object) -> Result<ObjectMetadata, FlameError> {
        let key_str = key.to_key().ok_or_else(|| {
            FlameError::InvalidConfig("ObjectKey requires object_id for patch".to_string())
        })?;

        // Acquire per-key lock to prevent concurrent version increments
        let key_lock = self.get_key_lock(&key_str)?;
        let _guard = write_async!(key_lock);

        let stat = self.storage.stat(key).await?.ok_or_else(|| {
            FlameError::NotFound(format!("object <{}> not found for patch", key_str))
        })?;
        if appended.data_type != stat.data_type {
            return Err(FlameError::InvalidConfig(format!(
                "Patch data type '{}' does not match object data type '{}'",
                appended.data_type, stat.data_type
            )));
        }
        let new_version = stat.version() + 1;

        let part = ObjectData::new().with_data(appended.data[0].data.clone());
        // The engine's returned metadata is not used: the cache owns every field.
        self.storage.patch_object(key, &part).await?;

        let meta = self
            .stat_metadata(key_str.clone(), &stat)
            .with_version(new_version)
            .with_size(stat.size() + part.size_bytes());

        {
            let mut metadata = lock_ptr!(self.metadata)?;
            metadata.insert(key_str.clone(), meta.clone());
        }

        tracing::debug!("Object patch: {} (version={})", key_str, new_version);
        Ok(meta)
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), FlameError> {
        // Wait for the readers and writers of every object it deletes, like a
        // put or a patch: a `Get` holds the read lock until its response ends.
        // The locks are taken in key order, so two deletes cannot deadlock.
        let mut keys: Vec<String> = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata
                .keys()
                .filter(|k| key.matches(k))
                .cloned()
                .collect()
        };
        keys.sort();
        let mut locks = Vec::with_capacity(keys.len());
        for key_str in &keys {
            locks.push(self.get_key_lock(key_str)?);
        }
        let mut guards = Vec::with_capacity(locks.len());
        for lock in locks {
            guards.push(lock.write_owned().await);
        }

        self.storage.delete_objects(key).await?;

        {
            let mut metadata = lock_ptr!(self.metadata)?;
            metadata.retain(|k, _| !key.matches(k));
        }

        tracing::debug!("Deleted: <{}>", key.to_prefix());

        Ok(())
    }

    pub(crate) async fn delete_if_unchanged(
        &self,
        expected: &ObjectMetadata,
    ) -> Result<bool, FlameError> {
        let key = ObjectKey::try_from(expected.key.as_str())?;
        let key_lock = self.get_key_lock(&expected.key)?;
        let _guard = write_async!(key_lock);

        let unchanged = {
            let metadata = lock_ptr!(self.metadata)?;
            metadata
                .get(&expected.key)
                .map(|current| {
                    current.version == expected.version
                        && current.creation_time == expected.creation_time
                })
                .unwrap_or(false)
        };
        if !unchanged {
            return Ok(false);
        }

        self.storage.delete_objects(&key).await?;

        {
            let mut metadata = lock_ptr!(self.metadata)?;
            metadata.remove(&expected.key);
        }

        Ok(true)
    }

    pub(crate) async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError> {
        let metadata = lock_ptr!(self.metadata)?;
        Ok(metadata.values().cloned().collect())
    }
}

/// HTTP/2 flow-control windows of the server. A sender can have one window of
/// data in flight per round trip, so the 64 KiB default limits the upload rate
/// of one stream to about window / round-trip time; large windows lift that.
const HTTP2_STREAM_WINDOW: u32 = 16 * 1024 * 1024;
const HTTP2_CONNECTION_WINDOW: u32 = 64 * 1024 * 1024;

/// Environment variable with the advertised address, used when `cache.advertise`
/// is unset. Instances of one deployment share a configuration file, so a
/// per-instance address comes from the environment.
const ADVERTISE_ENV: &str = "FLAME_CACHE_ADVERTISE";

/// Network interface addresses by interface name. Lookups that pick one entry
/// go in name order, so the result does not depend on the map's iteration order.
type InterfaceList = HashMap<String, Vec<IpAddr>>;

fn system_interfaces() -> Result<InterfaceList, FlameError> {
    let mut interfaces = InterfaceList::new();
    for iface in NetworkInterface::show().map_err(|e| FlameError::Network(e.to_string()))? {
        interfaces
            .entry(iface.name)
            .or_default()
            .extend(iface.addr.iter().map(|addr| addr.ip()));
    }
    Ok(interfaces)
}

/// Resolve an `advertise` value. A name matching a network interface (regex,
/// as `network_interface` was) gives that interface's first IPv4 address with
/// the scheme and port of `endpoint`. Otherwise the value is an address: a full
/// `scheme://host:port` URL is used as is, a bare `host` or `host:port` takes
/// the missing scheme and port from `endpoint`.
fn resolve_advertise(
    value: &str,
    endpoint: &CacheEndpoint,
    interfaces: impl FnOnce() -> Result<InterfaceList, FlameError>,
) -> Result<CacheEndpoint, FlameError> {
    if value.contains("://") {
        return CacheEndpoint::try_from(&value.to_string());
    }

    if let Ok(reg) = Regex::new(value) {
        let interfaces = interfaces()?;
        let matched = interfaces
            .iter()
            .filter(|(name, _)| reg.is_match(name))
            .min_by_key(|(name, _)| name.as_str());
        if let Some((_, addrs)) = matched {
            let ip = addrs.iter().find(|ip| ip.is_ipv4()).ok_or_else(|| {
                FlameError::InvalidConfig(format!(
                    "network interface <{}> has no IPv4 addresses",
                    value
                ))
            })?;
            return Ok(CacheEndpoint {
                scheme: endpoint.scheme.clone(),
                host: ip.to_string(),
                port: endpoint.port,
            });
        }
    }

    let url = Url::parse(&format!("{}://{}", endpoint.scheme, value))
        .map_err(|_| FlameError::InvalidConfig(format!("invalid advertise <{}>", value)))?;
    let host = url
        .host_str()
        .ok_or_else(|| FlameError::InvalidConfig(format!("no host in advertise <{}>", value)))?;
    Ok(CacheEndpoint {
        scheme: endpoint.scheme.clone(),
        host: host.to_string(),
        port: url.port().unwrap_or(endpoint.port),
    })
}

/// The address placed in object references. `cache.advertise` is optional.
/// When set it is resolved by `resolve_advertise` (an interface's address, or
/// else the value itself); when unset the environment override is resolved the
/// same way; with neither, the first private (intranet) IPv4 address of a
/// non-loopback interface, or `cache.endpoint` as configured if there is none.
fn advertised_endpoint(
    cache_config: &FlameCache,
    from_env: Option<String>,
    interfaces: impl FnOnce() -> Result<InterfaceList, FlameError>,
) -> Result<CacheEndpoint, FlameError> {
    let endpoint = CacheEndpoint::try_from(&cache_config.endpoint)?;
    let value = cache_config
        .advertise
        .clone()
        .filter(|advertise| !advertise.is_empty())
        .or_else(|| from_env.filter(|advertise| !advertise.is_empty()));
    if let Some(advertise) = value {
        return resolve_advertise(&advertise, &endpoint, interfaces);
    }

    let interfaces = interfaces()?;
    let mut names: Vec<_> = interfaces.keys().collect();
    names.sort();
    let private = names
        .into_iter()
        .flat_map(|name| &interfaces[name])
        .find_map(|ip| match ip {
            IpAddr::V4(ip) if ip.is_private() => Some(*ip),
            _ => None,
        });
    Ok(match private {
        Some(ip) => CacheEndpoint {
            scheme: endpoint.scheme.clone(),
            host: ip.to_string(),
            port: endpoint.port,
        },
        None => endpoint,
    })
}

/// Run the object cache server.
///
/// # Arguments
/// * `cluster_config` - FSM frontend and TLS configuration used by optional GC
/// * `cache_config` - Cache configuration (includes optional TLS and GC config)
pub async fn run(
    cluster_config: &FlameCluster,
    cache_config: &FlameCache,
) -> Result<(), FlameError> {
    // Clients may use a Service to select a cache replica for the initial
    // request. References returned by that replica must identify the replica
    // itself because cached objects are replica-local.
    // The cache serves the port of `endpoint` on every interface. References
    // carry `advertise` (an interface name or an address), else
    // `FLAME_CACHE_ADVERTISE`, else a private address of this host.
    let serving = CacheEndpoint::try_from(&cache_config.endpoint)?;
    let address_str = format!("0.0.0.0:{}", serving.port);
    let env_advertise = std::env::var(ADVERTISE_ENV).ok();
    let advertised = advertised_endpoint(cache_config, env_advertise, system_interfaces)?;
    tracing::info!("Advertising {}", advertised.to_uri());

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

    let storage_url = cache_config.storage.as_deref().unwrap_or("none");
    let storage = crate::storage::connect(storage_url, Some(&eviction_config)).await?;

    let cache = Arc::new(ObjectCache::new(advertised, storage)?);

    cache.load_from_storage().await?;

    let collector = ApplicationGarbageCollector::new(
        Arc::clone(&cache),
        cluster_config,
        cache_config.gc.interval,
    )?;
    let gc_handle = tokio::spawn(collector.run());

    let grpc_server = GrpcCacheServer::new(Arc::clone(&cache));

    tracing::info!("Starting object cache gRPC server at {}", address_str);

    let addr = address_str
        .parse()
        .map_err(|e| FlameError::InvalidConfig(format!("Invalid address: {}", e)))?;

    let mut builder = tonic::transport::Server::builder()
        .initial_stream_window_size(HTTP2_STREAM_WINDOW)
        .initial_connection_window_size(HTTP2_CONNECTION_WINDOW)
        .tcp_nodelay(true);

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

    let result = builder
        .add_service(ObjectCacheServiceServer::new(grpc_server))
        .serve(addr)
        .await
        .map_err(|e| FlameError::Internal(format!("Server error: {}", e)));

    gc_handle.abort();
    let _ = gc_handle.await;

    result
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
        fn object_key_matches_exact_full_key() {
            let full_key = ObjectKey::from_path("app/session/uuid").unwrap();
            let session_key = ObjectKey::from_path("app/session").unwrap();
            let app_key = ObjectKey::from_path("app/*").unwrap();

            assert!(full_key.matches("app/session/uuid"));
            assert!(!full_key.matches("app/session/other"));
            assert!(session_key.matches("app/session/uuid"));
            assert!(session_key.matches("app/session/other"));
            assert!(app_key.matches("app/other/uuid"));
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
        fn append_data_appends_in_order() {
            let obj = Object::new().append_data(vec![1, 2, 3]);
            assert_eq!(obj.data.len(), 1);
            assert_eq!(obj.current_version(), 1);
            assert_eq!(obj.opaque_data().unwrap(), &[1, 2, 3]);
            assert!(obj.data[1..].is_empty());
        }

        #[test]
        fn with_item_appends_after_the_first() {
            let second = ObjectData::new().with_data(vec![4, 5]);
            let third = ObjectData::new().with_data(vec![6, 7]);
            let obj = Object::new()
                .append_data(vec![1, 2, 3])
                .append_data(second)
                .append_data(third);

            assert_eq!(obj.current_version(), 3);
            assert_eq!(obj.opaque_data().unwrap(), &[1, 2, 3]);
            assert_eq!(obj.data[1..].len(), 2);
            assert_eq!(&obj.data[1..][0].data[..], &[4, 5]);
            assert_eq!(&obj.data[1..][1].data[..], &[6, 7]);
        }

        #[test]
        fn since_selects_the_data_after_a_version() {
            let all = Object::new()
                .append_data(vec![1])
                .append_data(ObjectData::new().with_data(vec![2]))
                .append_data(ObjectData::new().with_data(vec![3]));
            assert_eq!((all.version, all.current_version()), (0, 3));

            // Version 0 is all the parts.
            let from_zero = all.since(0).unwrap();
            assert_eq!((from_zero.version, from_zero.data.len()), (0, 3));
            // Above 0, only the data after that version.
            let tail = all.since(1).unwrap();
            assert_eq!((tail.version, tail.current_version()), (1, 3));
            assert_eq!(&tail.data[0].data[..], &[2]);
            assert!(all.since(3).unwrap().data.is_empty());
            assert!(all.since(4).is_none());
            // A part can be narrowed further, never widened.
            assert_eq!(tail.since(2).unwrap().data.len(), 1);
            assert!(tail.since(0).is_none());
        }

        #[test]
        fn object_data_builders() {
            let default = ObjectData::new();
            assert!(default.data.is_empty());
            assert!(default.creation_time > 0);

            let part = ObjectData::new().with_data(vec![1, 2, 3]);
            assert_eq!(&part.data[..], &[1, 2, 3]);
            assert_eq!(part.size_bytes(), 3);
        }

        #[test]
        fn version_and_size_count_every_part() {
            let obj = Object::new()
                .append_data(vec![1, 2, 3])
                .append_data(ObjectData::new().with_data(vec![4, 5]))
                .append_data(ObjectData::new().with_data(vec![6]));

            assert_eq!(obj.current_version(), 3);
            assert_eq!(obj.size_bytes(), 6);
        }

        #[test]
        fn object_clone_works() {
            let obj = Object::new().append_data(vec![10, 20, 30]);
            let cloned = obj.clone();
            assert_eq!(cloned.current_version(), obj.current_version());
            assert_eq!(cloned.opaque_data().unwrap(), obj.opaque_data().unwrap());
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
        fn to_uri_brackets_ipv6_hosts() {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "2001:db8::1".to_string(),
                port: 9090,
            };
            assert_eq!(endpoint.to_uri(), "grpc://[2001:db8::1]:9090");
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

    mod object_cache_operations {
        use super::*;

        async fn create_test_cache() -> ObjectCache {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            };
            let storage = crate::storage::connect("none", None).await.unwrap();
            ObjectCache::new(endpoint, storage).unwrap()
        }

        #[tokio::test]
        async fn put_and_get_object() {
            let cache = create_test_cache().await;
            let obj = Object::new().append_data(vec![1, 2, 3]);

            let key = ObjectKey::from_path("test-app/test-session").unwrap();
            let meta = cache.put(key, obj.clone()).await.unwrap();
            assert!(meta.key.starts_with("test-app/test-session/"));
            assert_eq!(meta.size, 3);

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let retrieved = cache.get(&key).await.unwrap();
            assert_eq!(retrieved.current_version(), 1);
            assert_eq!(retrieved.opaque_data().unwrap(), &[1, 2, 3]);
        }

        #[tokio::test]
        async fn append_keeps_object_readable_with_none_storage() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta = cache
                .put(
                    key,
                    Object::new()
                        .append_data(b"first".to_vec())
                        .with_data_type("arrow_table"),
                )
                .await
                .unwrap();
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let appended_meta = cache
                .patch(
                    &key,
                    Object::new()
                        .append_data(b"second".to_vec())
                        .with_data_type("arrow_table"),
                )
                .await
                .unwrap();

            assert_eq!(appended_meta.version, 2);
            assert_eq!(appended_meta.size, 11);
            assert_eq!(appended_meta.data_type, "arrow_table");

            let object = cache.get(&key).await.unwrap();
            assert_eq!(object.current_version(), 2);
            assert_eq!(object.opaque_data().unwrap(), b"first");
            assert_eq!(object.data_type, "arrow_table");
            assert_eq!(object.data[1..].len(), 1);
            assert_eq!(&object.data[1..][0].data[..], b"second");
        }

        #[tokio::test]
        async fn append_rejects_mismatched_data_type() {
            let cache = create_test_cache().await;
            let meta = cache
                .put(
                    ObjectKey::from_path("app/session/object").unwrap(),
                    Object::new()
                        .append_data(b"first".to_vec())
                        .with_data_type("arrow_table"),
                )
                .await
                .unwrap();
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let result = cache
                .patch(
                    &key,
                    Object::new()
                        .append_data(b"second".to_vec())
                        .with_data_type("numpy"),
                )
                .await;
            assert!(matches!(result, Err(FlameError::InvalidConfig(_))));
            assert_eq!(cache.get(&key).await.unwrap().current_version(), 1);
        }

        #[tokio::test]
        async fn append_preserves_codec_suffix_and_rejects_mismatch() {
            let cache = create_test_cache().await;
            let meta = cache
                .put(
                    ObjectKey::from_path("app/session/object").unwrap(),
                    Object::new()
                        .append_data(b"first".to_vec())
                        .with_data_type("raw.zstd"),
                )
                .await
                .unwrap();
            assert_eq!(meta.data_type, "raw.zstd");
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let mismatch = cache
                .patch(
                    &key,
                    Object::new()
                        .append_data(b"second".to_vec())
                        .with_data_type("raw"),
                )
                .await;
            assert!(matches!(mismatch, Err(FlameError::InvalidConfig(_))));
            assert_eq!(cache.get(&key).await.unwrap().current_version(), 1);

            let appended = cache
                .patch(
                    &key,
                    Object::new()
                        .append_data(b"second".to_vec())
                        .with_data_type("raw.zstd"),
                )
                .await
                .unwrap();
            assert_eq!(appended.data_type, "raw.zstd");
            let object = cache.get(&key).await.unwrap();
            assert_eq!(object.data_type, "raw.zstd");
        }

        #[tokio::test]
        async fn put_with_custom_id() {
            let cache = create_test_cache().await;
            let obj = Object::new().append_data(vec![42]);

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
                .put(key.clone(), Object::new().append_data(vec![1]))
                .await
                .unwrap();
            cache
                .put(key.clone(), Object::new().append_data(vec![2]))
                .await
                .unwrap();

            let other_key = ObjectKey::from_path("app/other-session").unwrap();
            cache
                .put(other_key, Object::new().append_data(vec![3]))
                .await
                .unwrap();

            let delete_key = ObjectKey::from_path("app/session-to-delete").unwrap();
            cache.delete(&delete_key).await.unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert!(all[0].key.starts_with("app/other-session/"));
        }

        #[tokio::test]
        async fn delete_removes_exact_object() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta1 = cache
                .put(key.clone(), Object::new().append_data(vec![1]))
                .await
                .unwrap();
            let meta2 = cache
                .put(key.clone(), Object::new().append_data(vec![2]))
                .await
                .unwrap();

            let delete_key = ObjectKey::from_path(&meta1.key).unwrap();
            cache.delete(&delete_key).await.unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].key, meta2.key);
        }

        #[tokio::test]
        async fn delete_waits_for_a_reader_of_the_object() {
            let cache = Arc::new(create_test_cache().await);
            let meta = cache
                .put(
                    ObjectKey::from_path("app/session").unwrap(),
                    Object::new().append_data(vec![1]),
                )
                .await
                .unwrap();
            let key = ObjectKey::from_path(&meta.key).unwrap();

            // A reader holds the lock of the object, like a `Get` that is being sent.
            let deleting = {
                let lock = cache.get_key_lock(&meta.key).unwrap();
                let _reader = lock.read_owned().await;
                let deleting = tokio::spawn({
                    let cache = Arc::clone(&cache);
                    let key = key.clone();
                    async move { cache.delete(&key).await }
                });
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                assert!(!deleting.is_finished());
                assert!(cache.get(&key).await.is_ok());
                deleting
            };

            deleting.await.unwrap().unwrap();
            assert!(cache.get(&key).await.is_err());
            assert!(cache.list_all().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn delete_of_a_prefix_waits_for_every_object() {
            let cache = Arc::new(create_test_cache().await);
            let prefix = ObjectKey::from_path("app/session").unwrap();
            let first = cache
                .put(prefix.clone(), Object::new().append_data(vec![1]))
                .await
                .unwrap();
            cache
                .put(prefix.clone(), Object::new().append_data(vec![2]))
                .await
                .unwrap();

            let deleting = {
                let lock = cache.get_key_lock(&first.key).unwrap();
                let _reader = lock.read_owned().await;
                let deleting = tokio::spawn({
                    let cache = Arc::clone(&cache);
                    let prefix = prefix.clone();
                    async move { cache.delete(&prefix).await }
                });
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                assert!(!deleting.is_finished());
                deleting
            };

            deleting.await.unwrap().unwrap();
            assert!(cache.list_all().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn delete_if_unchanged_removes_only_the_expected_object() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let expected = cache
                .put(key.clone(), Object::new().append_data(vec![1]))
                .await
                .unwrap();
            let retained = cache
                .put(key, Object::new().append_data(vec![2]))
                .await
                .unwrap();

            assert!(cache.delete_if_unchanged(&expected).await.unwrap());

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].key, retained.key);
        }

        #[tokio::test]
        async fn delete_if_unchanged_preserves_an_object_with_appended_data() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session")
                .unwrap()
                .with_object_id("object".to_string())
                .unwrap();
            let original = cache
                .put(key.clone(), Object::new().append_data(vec![1]))
                .await
                .unwrap();
            let appended = cache
                .patch(&key, Object::new().append_data(vec![2]))
                .await
                .unwrap();

            assert!(!cache.delete_if_unchanged(&original).await.unwrap());

            let current = cache.get(&key).await.unwrap();
            assert_eq!(current.current_version(), appended.version);
            assert_eq!(current.opaque_data().unwrap(), &[1]);
            assert!(cache.delete_if_unchanged(&appended).await.unwrap());
            assert!(cache.get(&key).await.is_err());
        }

        #[tokio::test]
        async fn list_all_returns_all_metadata() {
            let cache = create_test_cache().await;

            let key1 = ObjectKey::from_path("app/s1").unwrap();
            cache
                .put(key1, Object::new().append_data(vec![1]))
                .await
                .unwrap();

            let key2 = ObjectKey::from_path("app/s2").unwrap();
            cache
                .put(key2, Object::new().append_data(vec![2]))
                .await
                .unwrap();

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
    }

    mod bounded_memory {
        use super::*;
        use crate::storage::{DiskStorage, ResidentStorage};
        use std::path::Path;
        use tempfile::tempdir;

        const MIB: usize = 1024 * 1024;

        fn endpoint() -> CacheEndpoint {
            CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            }
        }

        fn eviction(max_memory: &str) -> EvictionConfig {
            EvictionConfig {
                policy: Some("lru".to_string()),
                max_memory: Some(max_memory.to_string()),
                max_objects: None,
            }
        }

        fn disk_cache(path: &Path, max_memory: &str) -> ObjectCache {
            let storage = Box::new(ResidentStorage::new(
                Box::new(DiskStorage::new(path.to_path_buf()).unwrap()),
                Some(&eviction(max_memory)),
            ));
            ObjectCache::new(endpoint(), storage).unwrap()
        }

        /// `count` chunks of 1 MiB, produced lazily; chunk `i` is filled with `i`.
        fn chunks(count: usize) -> BoxStream<'static, Result<Bytes, FlameError>> {
            Box::pin(futures::stream::iter(
                (0..count).map(|index| Ok(Bytes::from(vec![index as u8; MIB]))),
            ))
        }

        /// A `raw` write of `count` chunks to `key`.
        fn raw_request(key: ObjectKey, count: usize) -> WriteRequest {
            WriteRequest {
                key,
                data_type: "raw".to_string(),
                chunks: chunks(count),
            }
        }

        fn object_key(name: &str) -> ObjectKey {
            ObjectKey::from_path("app/session")
                .unwrap()
                .with_object_id(name.to_string())
                .unwrap()
        }

        #[tokio::test]
        async fn streamed_put_keeps_no_payload_resident() {
            let directory = tempdir().unwrap();
            let cache = disk_cache(directory.path(), "4M");

            let key = object_key("big");
            let meta = cache
                .put_stream(raw_request(key.clone(), 12))
                .await
                .unwrap();
            assert_eq!(meta.size, (12 * MIB) as u64);
            assert_eq!(meta.version, 1);
            assert_eq!(cache.storage.resident().0, 0);

            // Ranges are served across chunk boundaries, from the open files;
            // only the handle is resident.
            let range = cache
                .storage
                .read_range(&key, 0, (3 * MIB - 1) as u64, 2)
                .await
                .unwrap();
            assert_eq!(range.as_ref(), &[2, 3]);
            let (count, bytes) = cache.storage.resident();
            assert_eq!(count, 1);
            assert!(bytes < 1024);
        }

        #[tokio::test]
        async fn startup_loads_metadata_only() {
            let directory = tempdir().unwrap();
            let writer = disk_cache(directory.path(), "100M");
            for name in ["a", "b", "c"] {
                writer
                    .put_stream(raw_request(object_key(name), 3))
                    .await
                    .unwrap();
            }

            let cache = disk_cache(directory.path(), "4M");
            cache.load_from_storage().await.unwrap();
            assert_eq!(cache.storage.resident().0, 0);
            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 3);
            assert!(all
                .iter()
                .all(|meta| meta.version == 1 && meta.size == (3 * MIB) as u64));
        }

        #[tokio::test]
        async fn put_of_an_existing_object_is_rejected() {
            let directory = tempdir().unwrap();
            let key = object_key("created");
            disk_cache(directory.path(), "100M")
                .put_stream(raw_request(key.clone(), 2))
                .await
                .unwrap();

            // Not loaded: the object is found in storage.
            let cache = disk_cache(directory.path(), "100M");
            let again = cache.put_stream(raw_request(key.clone(), 1)).await;
            assert!(matches!(again, Err(FlameError::AlreadyExist(_))));

            // The same after a metadata-only startup, and nothing changed.
            let cache = disk_cache(directory.path(), "100M");
            cache.load_from_storage().await.unwrap();
            let again = cache.put_stream(raw_request(key.clone(), 1)).await;
            assert!(matches!(again, Err(FlameError::AlreadyExist(_))));
            let meta = cache.metadata_of("app/session/created").unwrap().unwrap();
            assert_eq!((meta.version, meta.size), (1, (2 * MIB) as u64));
        }

        #[tokio::test]
        async fn get_since_a_version_returns_only_later_data() {
            let directory = tempdir().unwrap();
            let cache = disk_cache(directory.path(), "100M");
            let key = object_key("appended");
            cache.put_stream(raw_request(key.clone(), 2)).await.unwrap();
            cache
                .patch(&key, Object::new().append_data(b"second".to_vec()))
                .await
                .unwrap();

            let all = cache.get(&key).await.unwrap();
            assert_eq!((all.version, all.current_version()), (0, 2));
            let tail = cache.get_since(&key, 1).await.unwrap();
            assert_eq!((tail.version, tail.current_version()), (1, 2));
            assert_eq!(tail.data.len(), 1);
            assert_eq!(&tail.data[0].data[..], b"second");
        }

        #[tokio::test]
        async fn append_to_a_large_object_updates_metadata() {
            let directory = tempdir().unwrap();
            let cache = disk_cache(directory.path(), "100M");
            let key = object_key("appended");
            cache.put_stream(raw_request(key.clone(), 9)).await.unwrap();

            let meta = cache
                .patch(
                    &key,
                    Object::new()
                        .append_data(b"second".to_vec())
                        .with_data_type("raw"),
                )
                .await
                .unwrap();
            assert_eq!(meta.version, 2);
            assert_eq!(meta.size, (9 * MIB + 6) as u64);
            // The append opened the object and left its handle resident.
            assert_eq!(cache.storage.resident().0, 1);

            let stored = cache.metadata_of("app/session/appended").unwrap().unwrap();
            assert_eq!(stored.version, 2);
        }
    }

    mod none_storage {
        use super::*;

        const MIB: usize = 1024 * 1024;

        fn key(name: &str) -> ObjectKey {
            ObjectKey::from_path("app/session")
                .unwrap()
                .with_object_id(name.to_string())
                .unwrap()
        }

        /// A `none` cache that holds at most two objects.
        async fn small_budget_cache() -> ObjectCache {
            let eviction = EvictionConfig {
                policy: Some("lru".to_string()),
                max_memory: Some("1M".to_string()),
                max_objects: Some(2),
            };
            let storage = crate::storage::connect("none", Some(&eviction))
                .await
                .unwrap();
            ObjectCache::new(
                CacheEndpoint {
                    scheme: "grpc".to_string(),
                    host: "localhost".to_string(),
                    port: 9090,
                },
                storage,
            )
            .unwrap()
        }

        /// Without a store the cache holds the only copy of an object, so an
        /// evicted object is lost.
        #[tokio::test]
        async fn eviction_loses_objects() {
            let cache = small_budget_cache().await;
            for index in 0..3 {
                let chunks = futures::stream::once(futures::future::ready(Ok(Bytes::from(vec![
                    index as u8;
                    MIB / 4
                ]))));
                cache
                    .put_stream(WriteRequest {
                        key: key(&format!("o{index}")),
                        data_type: "raw".to_string(),
                        chunks: Box::pin(chunks),
                    })
                    .await
                    .unwrap();
            }
            // Eviction removes whole batches, so what remains is at most the limit.
            assert!(cache.get(&key("o0")).await.is_err());
            assert!(cache.storage.resident().0 <= 2);
        }
    }

    mod advertise {
        use super::*;

        fn endpoint(host: &str) -> CacheEndpoint {
            CacheEndpoint {
                scheme: "grpc".to_string(),
                host: host.to_string(),
                port: 9090,
            }
        }

        #[tokio::test]
        async fn refs_carry_advertised_address() {
            let storage = crate::storage::connect("none", None).await.unwrap();
            let cache = ObjectCache::new(endpoint("cache-1.cache.flame.svc"), storage).unwrap();

            let key = ObjectKey::from_path("app/session").unwrap();
            let chunks =
                futures::stream::once(futures::future::ready(Ok(Bytes::from(vec![7u8; 10]))));
            let meta = cache
                .put_stream(WriteRequest {
                    key,
                    data_type: "raw".to_string(),
                    chunks: Box::pin(chunks),
                })
                .await
                .unwrap();
            assert_eq!(meta.endpoint, "grpc://cache-1.cache.flame.svc:9090");

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let appended = cache
                .patch(
                    &key,
                    Object::new().append_data(vec![1u8]).with_data_type("raw"),
                )
                .await
                .unwrap();
            assert_eq!(appended.endpoint, "grpc://cache-1.cache.flame.svc:9090");
        }

        fn interfaces() -> Result<InterfaceList, FlameError> {
            Ok(InterfaceList::from([
                (
                    "lo".to_string(),
                    vec!["::1".parse().unwrap(), "127.0.0.1".parse().unwrap()],
                ),
                ("eth0".to_string(), vec!["10.1.2.3".parse().unwrap()]),
                ("v6only".to_string(), vec!["fe80::1".parse().unwrap()]),
            ]))
        }

        fn advertised(config: &FlameCache, env: Option<&str>) -> Result<String, FlameError> {
            advertised_endpoint(config, env.map(str::to_string), interfaces).map(|e| e.to_uri())
        }

        #[test]
        fn advertised_endpoint_precedence() {
            let mut config = FlameCache {
                endpoint: "grpcs://cache.flame.svc:9443".to_string(),
                ..FlameCache::default()
            };
            // Neither set: the first private address of a non-loopback interface.
            assert_eq!(
                advertised(&config, None).unwrap(),
                "grpc+tls://10.1.2.3:9443"
            );
            assert_eq!(
                advertised(&config, Some("")).unwrap(),
                "grpc+tls://10.1.2.3:9443"
            );

            // Only the environment set.
            assert_eq!(
                advertised(&config, Some("grpcs://cache-2.cache.flame.svc:9090")).unwrap(),
                "grpc+tls://cache-2.cache.flame.svc:9090"
            );
            assert!(advertised(&config, Some("grpc://")).is_err());

            // `cache.advertise` wins over the environment.
            config.advertise = Some("grpc://from-config:9091".to_string());
            assert_eq!(
                advertised(&config, Some("grpc://from-env:9092")).unwrap(),
                "grpc://from-config:9091"
            );
            assert_eq!(
                advertised(&config, None).unwrap(),
                "grpc://from-config:9091"
            );
        }

        #[test]
        fn interface_picks_go_in_name_order() {
            let two = || -> Result<InterfaceList, FlameError> {
                Ok(InterfaceList::from([
                    ("eth1".to_string(), vec!["10.0.0.2".parse().unwrap()]),
                    ("eth0".to_string(), vec!["10.0.0.1".parse().unwrap()]),
                ]))
            };
            let mut config = FlameCache {
                endpoint: "grpc://0.0.0.0:9090".to_string(),
                ..FlameCache::default()
            };
            // The first private address, and the first interface a pattern matches.
            for _ in 0..8 {
                let advertised = advertised_endpoint(&config, None, two).unwrap();
                assert_eq!(advertised.to_uri(), "grpc://10.0.0.1:9090");
            }
            config.advertise = Some("eth".to_string());
            for _ in 0..8 {
                let advertised = advertised_endpoint(&config, None, two).unwrap();
                assert_eq!(advertised.to_uri(), "grpc://10.0.0.1:9090");
            }
        }

        #[test]
        fn advertise_falls_back_to_endpoint_without_private_address() {
            let config = FlameCache {
                endpoint: "grpc://127.0.0.1:9090".to_string(),
                ..FlameCache::default()
            };
            let only_loopback = || -> Result<InterfaceList, FlameError> {
                Ok(InterfaceList::from([(
                    "lo".to_string(),
                    vec!["127.0.0.1".parse().unwrap()],
                )]))
            };
            let advertised = advertised_endpoint(&config, None, only_loopback).unwrap();
            assert_eq!(advertised.to_uri(), "grpc://127.0.0.1:9090");
        }

        #[test]
        fn advertise_resolves_interface_first() {
            let mut config = FlameCache {
                endpoint: "grpcs://0.0.0.0:9443".to_string(),
                advertise: Some("eth0".to_string()),
                ..FlameCache::default()
            };
            assert_eq!(
                advertised(&config, None).unwrap(),
                "grpc+tls://10.1.2.3:9443"
            );
            // The configuration wins over the environment, also for interfaces.
            assert_eq!(
                advertised(&config, Some("lo")).unwrap(),
                "grpc+tls://10.1.2.3:9443"
            );
            // The environment value is resolved the same way.
            config.advertise = None;
            assert_eq!(
                advertised(&config, Some("lo")).unwrap(),
                "grpc+tls://127.0.0.1:9443"
            );
            // An interface without IPv4 is a configuration error.
            config.advertise = Some("v6only".to_string());
            assert!(matches!(
                advertised(&config, None),
                Err(FlameError::InvalidConfig(_))
            ));
        }

        #[test]
        fn advertise_literal_address_forms() {
            let config = |advertise: &str| FlameCache {
                endpoint: "grpcs://0.0.0.0:9443".to_string(),
                advertise: Some(advertise.to_string()),
                ..FlameCache::default()
            };
            assert_eq!(
                advertised(&config("grpc://pod-0.svc:9000"), None).unwrap(),
                "grpc://pod-0.svc:9000"
            );
            assert_eq!(
                advertised(&config("pod-0.svc"), None).unwrap(),
                "grpc+tls://pod-0.svc:9443"
            );
            assert_eq!(
                advertised(&config("pod-0.svc:9000"), None).unwrap(),
                "grpc+tls://pod-0.svc:9000"
            );
            assert_eq!(
                advertised(&config("192.168.1.5"), None).unwrap(),
                "grpc+tls://192.168.1.5:9443"
            );
        }

        #[test]
        fn advertise_does_not_list_interfaces_for_urls() {
            let config = FlameCache {
                endpoint: "grpc://0.0.0.0:9090".to_string(),
                advertise: Some("grpc://x:1".to_string()),
                ..FlameCache::default()
            };
            let result = advertised_endpoint(&config, None, || {
                Err(FlameError::Network("must not be called".to_string()))
            });
            assert!(result.is_ok());
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
            let storage = crate::storage::connect("none", None).await.unwrap();
            ObjectCache::new(endpoint, storage).unwrap()
        }

        #[tokio::test]
        async fn put_initializes_version_to_one() {
            let cache = create_test_cache().await;
            let obj = Object::new().append_data(vec![1, 2, 3]);

            let key = ObjectKey::from_path("test-app/test-session").unwrap();
            let meta = cache.put(key, obj).await.unwrap();

            assert_eq!(meta.version, 1);

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let retrieved = cache.get(&key).await.unwrap();
            assert_eq!(retrieved.current_version(), 1);
        }

        #[tokio::test]
        async fn version_persists_after_get() {
            let cache = create_test_cache().await;
            let obj = Object::new().append_data(vec![1, 2, 3]);

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta = cache.put(key, obj).await.unwrap();

            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
            let retrieved1 = cache.get(&key).await.unwrap();
            assert_eq!(retrieved1.current_version(), 1);

            let retrieved2 = cache.get(&key).await.unwrap();
            assert_eq!(retrieved2.current_version(), 1);
        }
    }

    mod cache_benchmarks {
        use super::*;
        use std::hint::black_box;
        use std::time::Instant;

        // Run with: cargo test -p flame-object-cache cache_benchmarks -- --ignored --nocapture --test-threads=1
        #[test]
        #[ignore = "manual performance benchmark"]
        fn opaque_snapshot_clone() {
            let object = Object::new().append_data(vec![42; 8 * 1024 * 1024]);
            let shared = Arc::new(object.clone());
            let iterations = 256;

            let started = Instant::now();
            for _ in 0..iterations {
                black_box(black_box(object.opaque_data().unwrap()).to_vec());
            }
            let deep = started.elapsed();

            let started = Instant::now();
            for _ in 0..iterations {
                black_box(Arc::clone(black_box(&shared)));
            }
            let shared_time = started.elapsed();

            println!(
                "opaque snapshot, 8 MiB, {iterations} iterations: prior_payload_copy={:?}/op shared_clone={:?}/op",
                deep / iterations,
                shared_time / iterations,
            );
        }
    }
}

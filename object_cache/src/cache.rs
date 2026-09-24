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
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use bytesize::ByteSize;
use common::net::host_for_uri;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use regex::Regex;
use stdng::{lock_ptr, new_ptr, MutexPtr};
use url::Url;

use common::ctx::{FlameCache, FlameCluster};
use common::FlameError;

use crate::eviction::{new_policy, EvictionConfig, EvictionPolicyPtr};
use crate::gc::ApplicationGarbageCollector;

mod grpc;
use grpc::GrpcCacheServer;
use rpc::flame::v1::object_cache_service_server::ObjectCacheServiceServer;

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

/// Object with optional delta support. Payload interpretation belongs to clients.
#[derive(Debug, Clone)]
pub struct Object {
    pub version: u64,
    pub creation_time: i64,
    pub data: Bytes,
    pub data_type: String,
    pub deltas: Vec<Object>,
}

impl Object {
    /// Create a new Object with no deltas.
    #[allow(dead_code)] // Raw constructor remains available to internal callers and tests.
    pub fn new(version: u64, data: impl Into<Bytes>) -> Self {
        Self::new_at(version, data, current_time_millis())
    }

    pub fn new_typed(version: u64, data: impl Into<Bytes>, data_type: impl Into<String>) -> Self {
        Self::new_typed_at(version, data, data_type, current_time_millis())
    }

    #[allow(dead_code)] // Used by storage tests to set a deterministic creation time.
    pub(crate) fn new_at(version: u64, data: impl Into<Bytes>, creation_time: i64) -> Self {
        Self::new_typed_at(version, data, "raw", creation_time)
    }

    pub(crate) fn new_typed_at(
        version: u64,
        data: impl Into<Bytes>,
        data_type: impl Into<String>,
        creation_time: i64,
    ) -> Self {
        Self {
            version,
            creation_time,
            data: data.into(),
            data_type: data_type.into(),
            deltas: Vec::new(),
        }
    }

    /// Create a new Object with deltas.
    #[cfg(test)]
    pub fn with_deltas(version: u64, data: Vec<u8>, deltas: Vec<Object>) -> Self {
        Self {
            version,
            creation_time: current_time_millis(),
            data: data.into(),
            data_type: "raw".to_string(),
            deltas,
        }
    }

    pub(crate) fn with_data_at(
        version: u64,
        data: Bytes,
        data_type: String,
        deltas: Vec<Object>,
        creation_time: i64,
    ) -> Self {
        Self {
            version,
            creation_time,
            data,
            data_type,
            deltas,
        }
    }

    pub fn opaque_data(&self) -> Result<&[u8], FlameError> {
        Ok(&self.data)
    }

    pub fn current_version(&self) -> u64 {
        self.deltas
            .iter()
            .fold(self.version, |current, delta| current.max(delta.version))
    }

    pub fn size_bytes(&self) -> u64 {
        self.data.len() as u64 + self.deltas.iter().map(Object::size_bytes).sum::<u64>()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,
    pub size: u64,
    pub delta_count: u64,
    pub creation_time: i64,
    pub data_type: String,
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
    objects: MutexPtr<HashMap<String, Arc<Object>>>,
    metadata: MutexPtr<HashMap<String, ObjectMetadata>>,
    eviction_policy: EvictionPolicyPtr,
    /// Per-key locks coordinate concurrent PUT/PATCH writes with GET snapshots.
    key_locks: MutexPtr<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
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
            key_locks: new_ptr(HashMap::new()),
        })
    }

    async fn load_from_storage(&self) -> Result<(), FlameError> {
        let items = self.storage.load_objects().await?;

        let mut objects = lock_ptr!(self.objects)?;
        let mut metadata = lock_ptr!(self.metadata)?;

        for (key, object) in items {
            let key_str = key.to_key().expect("loaded key must have object_id");
            let size = object.size_bytes();
            let meta = self.create_metadata(key_str.clone(), &object);

            objects.insert(key_str.clone(), Arc::new(object));
            metadata.insert(key_str.clone(), meta);

            self.eviction_policy.on_add(&key_str, size);
        }

        drop(objects);
        drop(metadata);
        self.run_eviction()?;

        Ok(())
    }

    fn get_key_lock(&self, key: &str) -> Result<Arc<tokio::sync::RwLock<()>>, FlameError> {
        let mut locks = lock_ptr!(self.key_locks)?;
        Ok(locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone())
    }

    fn create_metadata(&self, key: String, object: &Object) -> ObjectMetadata {
        ObjectMetadata {
            endpoint: self.endpoint.to_uri(),
            key,
            version: object.current_version(),
            size: object.size_bytes(),
            delta_count: object.deltas.len() as u64,
            creation_time: object.creation_time,
            data_type: object.data_type.clone(),
        }
    }

    fn resident_version(&self, key: &str) -> Result<Option<u64>, FlameError> {
        let objects = lock_ptr!(self.objects)?;
        if !objects.contains_key(key) {
            return Ok(None);
        }
        let metadata = lock_ptr!(self.metadata)?;
        Ok(metadata.get(key).map(|meta| meta.version))
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

        // Acquire per-key lock to prevent concurrent version increments
        let key_lock = self.get_key_lock(&key_str)?;
        let _guard = key_lock.write().await;

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
                .map(|obj| obj.current_version())
                .unwrap_or(0),
        };
        let new_version = current_version + 1;

        let creation_time = current_time_millis();
        let versioned_object =
            Object::new_typed_at(new_version, object.data, object.data_type, creation_time);
        let size = versioned_object.size_bytes();

        self.storage.write_object(&key, &versioned_object).await?;

        let meta = self.create_metadata(key_str.clone(), &versioned_object);

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key_str.clone(), Arc::new(versioned_object));
            metadata.insert(key_str.clone(), meta.clone());
        }

        self.eviction_policy.on_add(&key_str, size);
        self.run_eviction()?;

        tracing::debug!("Object put: {} (version={})", key_str, new_version);

        Ok(meta)
    }

    async fn get(&self, key: &ObjectKey) -> Result<Arc<Object>, FlameError> {
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
            let size = object.size_bytes();
            let object = Arc::new(object);

            {
                let mut objects = lock_ptr!(self.objects)?;
                let mut metadata = lock_ptr!(self.metadata)?;

                objects.insert(key_str.clone(), Arc::clone(&object));

                let meta = self.create_metadata(key_str.clone(), &object);
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
        let key_lock = self.get_key_lock(&key_str)?;
        let _guard = key_lock.write().await;

        let current_object = {
            let objects = lock_ptr!(self.objects)?;
            objects.get(&key_str).cloned()
        };

        let current_object = match current_object {
            Some(object) => object,
            None => Arc::new(self.storage.read_object(key).await?.ok_or_else(|| {
                FlameError::NotFound(format!("object <{}> not found for patch", key_str))
            })?),
        };
        if delta.data_type != current_object.data_type {
            return Err(FlameError::InvalidConfig(format!(
                "Patch data type '{}' does not match object data type '{}'",
                delta.data_type, current_object.data_type
            )));
        }
        let current_version = current_object.current_version();
        let creation_time = current_object.creation_time;
        let new_version = current_version + 1;

        let versioned_delta = Object::new_typed(new_version, delta.data, delta.data_type);
        let mut meta = self.storage.patch_object(key, &versioned_delta).await?;

        let mut patched_object = (*current_object).clone();
        patched_object.deltas.push(versioned_delta);
        let size = patched_object.size_bytes();

        meta.endpoint = self.endpoint.to_uri();
        meta.version = new_version;
        meta.size = size;
        meta.delta_count = patched_object.deltas.len() as u64;
        meta.creation_time = creation_time;
        meta.data_type = patched_object.data_type.clone();

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.insert(key_str.clone(), Arc::new(patched_object));
            metadata.insert(key_str.clone(), meta.clone());
        }

        self.eviction_policy.on_update(&key_str, size);
        self.run_eviction()?;

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

        self.storage.delete_objects(key).await?;

        {
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;

            objects.retain(|k, _| !key.matches(k));
            metadata.retain(|k, _| !key.matches(k));
        }

        for k in &keys_to_remove {
            self.eviction_policy.on_remove(k);
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
        let _guard = key_lock.write().await;

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
            let mut objects = lock_ptr!(self.objects)?;
            let mut metadata = lock_ptr!(self.metadata)?;
            objects.remove(&expected.key);
            metadata.remove(&expected.key);
        }
        self.eviction_policy.on_remove(&expected.key);

        Ok(true)
    }

    pub(crate) async fn list_all(&self) -> Result<Vec<ObjectMetadata>, FlameError> {
        let metadata = lock_ptr!(self.metadata)?;
        Ok(metadata.values().cloned().collect())
    }
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

    let cache = Arc::new(ObjectCache::new(endpoint, storage, Some(&eviction_config))?);

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
        fn new_creates_object_without_deltas() {
            let obj = Object::new(1, vec![1, 2, 3]);
            assert_eq!(obj.version, 1);
            assert_eq!(obj.opaque_data().unwrap(), &[1, 2, 3]);
            assert!(obj.deltas.is_empty());
        }

        #[test]
        fn with_deltas_creates_object_with_deltas() {
            let delta1 = Object::new(1, vec![4, 5]);
            let delta2 = Object::new(2, vec![6, 7]);
            let obj = Object::with_deltas(0, vec![1, 2, 3], vec![delta1.clone(), delta2.clone()]);

            assert_eq!(obj.version, 0);
            assert_eq!(obj.opaque_data().unwrap(), &[1, 2, 3]);
            assert_eq!(obj.deltas.len(), 2);
            assert_eq!(obj.deltas[0].opaque_data().unwrap(), &[4, 5]);
            assert_eq!(obj.deltas[1].opaque_data().unwrap(), &[6, 7]);
        }

        #[test]
        fn current_version_includes_base_version() {
            let delta = Object::new(3, vec![4, 5]);
            let obj = Object::with_deltas(5, vec![1, 2, 3], vec![delta]);

            assert_eq!(obj.current_version(), 5);
        }

        #[test]
        fn object_clone_works() {
            let obj = Object::new(42, vec![10, 20, 30]);
            let cloned = obj.clone();
            assert_eq!(cloned.version, obj.version);
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

        #[tokio::test]
        async fn resident_reads_share_payload_and_patch_preserves_snapshot() {
            let cache = create_test_cache().await;
            let meta = cache
                .put(
                    ObjectKey::from_path("app/session").unwrap(),
                    Object::new(0, vec![7; 1024]),
                )
                .await
                .unwrap();
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let first = cache.get(&key).await.unwrap();
            let second = cache.get(&key).await.unwrap();
            assert!(Arc::ptr_eq(&first, &second));

            cache
                .patch(&key, Object::new(0, b"delta".to_vec()))
                .await
                .unwrap();
            let patched = cache.get(&key).await.unwrap();
            assert_eq!(first.current_version(), 1);
            assert_eq!(patched.current_version(), 2);
            assert_eq!(
                first.opaque_data().unwrap().as_ptr(),
                patched.opaque_data().unwrap().as_ptr()
            );
        }

        async fn create_test_cache() -> ObjectCache {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            };
            let storage = crate::storage::connect("none").await.unwrap();
            ObjectCache::new(endpoint, storage, None).unwrap()
        }

        async fn create_test_cache_with_max_memory(max_memory: &str) -> ObjectCache {
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "localhost".to_string(),
                port: 9090,
            };
            let storage = crate::storage::connect("none").await.unwrap();
            let eviction_config = EvictionConfig {
                policy: Some("lru".to_string()),
                max_memory: Some(max_memory.to_string()),
                max_objects: None,
            };
            ObjectCache::new(endpoint, storage, Some(&eviction_config)).unwrap()
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
            assert_eq!(retrieved.opaque_data().unwrap(), &[1, 2, 3]);
        }

        #[tokio::test]
        async fn patch_keeps_object_readable_with_none_storage() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta = cache
                .put(key, Object::new_typed(0, b"base".to_vec(), "arrow_table"))
                .await
                .unwrap();
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let patch_meta = cache
                .patch(&key, Object::new_typed(0, b"patch".to_vec(), "arrow_table"))
                .await
                .unwrap();

            assert_eq!(patch_meta.version, 2);
            assert_eq!(patch_meta.size, 9);
            assert_eq!(patch_meta.delta_count, 1);
            assert_eq!(patch_meta.data_type, "arrow_table");

            let object = cache.get(&key).await.unwrap();
            assert_eq!(object.version, 1);
            assert_eq!(object.current_version(), 2);
            assert_eq!(object.opaque_data().unwrap(), b"base");
            assert_eq!(object.data_type, "arrow_table");
            assert_eq!(object.deltas.len(), 1);
            assert_eq!(object.deltas[0].version, 2);
            assert_eq!(object.deltas[0].data_type, "arrow_table");
            assert_eq!(object.deltas[0].opaque_data().unwrap(), b"patch");
        }

        #[tokio::test]
        async fn patch_rejects_mismatched_data_type() {
            let cache = create_test_cache().await;
            let meta = cache
                .put(
                    ObjectKey::from_path("app/session/object").unwrap(),
                    Object::new_typed(0, b"base".to_vec(), "arrow_table"),
                )
                .await
                .unwrap();
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let result = cache
                .patch(&key, Object::new_typed(0, b"patch".to_vec(), "numpy"))
                .await;
            assert!(matches!(result, Err(FlameError::InvalidConfig(_))));
            assert_eq!(cache.get(&key).await.unwrap().current_version(), 1);
        }

        #[tokio::test]
        async fn patch_preserves_codec_suffix_and_rejects_mismatch() {
            let cache = create_test_cache().await;
            let meta = cache
                .put(
                    ObjectKey::from_path("app/session/object").unwrap(),
                    Object::new_typed(0, b"base".to_vec(), "raw.zstd"),
                )
                .await
                .unwrap();
            assert_eq!(meta.data_type, "raw.zstd");
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let mismatch = cache
                .patch(&key, Object::new_typed(0, b"patch".to_vec(), "raw"))
                .await;
            assert!(matches!(mismatch, Err(FlameError::InvalidConfig(_))));
            assert_eq!(cache.get(&key).await.unwrap().current_version(), 1);

            let patch = cache
                .patch(&key, Object::new_typed(0, b"patch".to_vec(), "raw.zstd"))
                .await
                .unwrap();
            assert_eq!(patch.data_type, "raw.zstd");
            let object = cache.get(&key).await.unwrap();
            assert_eq!(object.data_type, "raw.zstd");
            assert_eq!(object.deltas[0].data_type, "raw.zstd");
        }

        #[tokio::test]
        async fn patch_replaces_eviction_accounting_for_resident_object() {
            let cache = create_test_cache_with_max_memory("10").await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta = cache
                .put(key, Object::new(0, b"base".to_vec()))
                .await
                .unwrap();
            let key = ObjectKey::try_from(meta.key.as_str()).unwrap();

            let patch_meta = cache
                .patch(&key, Object::new(0, b"patch".to_vec()))
                .await
                .unwrap();

            assert_eq!(patch_meta.size, 9);

            let object = cache.get(&key).await.unwrap();
            assert_eq!(object.size_bytes(), 9);
            assert_eq!(object.deltas.len(), 1);
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
        async fn delete_removes_exact_object() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let meta1 = cache
                .put(key.clone(), Object::new(0, vec![1]))
                .await
                .unwrap();
            let meta2 = cache
                .put(key.clone(), Object::new(0, vec![2]))
                .await
                .unwrap();

            let delete_key = ObjectKey::from_path(&meta1.key).unwrap();
            cache.delete(&delete_key).await.unwrap();

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].key, meta2.key);
        }

        #[tokio::test]
        async fn delete_if_unchanged_removes_only_the_expected_object() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session").unwrap();
            let expected = cache
                .put(key.clone(), Object::new(0, vec![1]))
                .await
                .unwrap();
            let retained = cache.put(key, Object::new(0, vec![2])).await.unwrap();

            assert!(cache.delete_if_unchanged(&expected).await.unwrap());

            let all = cache.list_all().await.unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].key, retained.key);
        }

        #[tokio::test]
        async fn delete_if_unchanged_preserves_a_replaced_object() {
            let cache = create_test_cache().await;

            let key = ObjectKey::from_path("app/session")
                .unwrap()
                .with_object_id("object".to_string())
                .unwrap();
            let original = cache
                .put(key.clone(), Object::new(0, vec![1]))
                .await
                .unwrap();
            let replacement = cache
                .put(key.clone(), Object::new(0, vec![2]))
                .await
                .unwrap();

            assert!(!cache.delete_if_unchanged(&original).await.unwrap());

            let current = cache.get(&key).await.unwrap();
            assert_eq!(current.version, replacement.version);
            assert_eq!(current.opaque_data().unwrap(), &[2]);
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
            let endpoint = CacheEndpoint {
                scheme: "grpc".to_string(),
                host: "10.0.0.42".to_string(),
                port: 9090,
            };
            let storage = crate::storage::connect("none").await.unwrap();
            let cache = ObjectCache::new(endpoint, storage, None).unwrap();
            let mut object = Object::new_typed_at(1, vec![0; 100], "raw", 1234);
            object.deltas = vec![Object::new(1, Vec::<u8>::new()); 5];
            let meta = cache.create_metadata("app/session/key".to_string(), &object);

            assert_eq!(meta.key, "app/session/key");
            assert_eq!(meta.version, 1);
            assert_eq!(meta.size, 100);
            assert_eq!(meta.delta_count, 5);
            assert_eq!(meta.creation_time, 1234);
            assert_eq!(meta.endpoint, "grpc://10.0.0.42:9090");
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

    mod cache_benchmarks {
        use super::*;
        use std::hint::black_box;
        use std::time::Instant;

        // Run with: cargo test -p flame-object-cache cache_benchmarks -- --ignored --nocapture --test-threads=1
        #[test]
        #[ignore = "manual performance benchmark"]
        fn opaque_snapshot_clone() {
            let object = Object::new(1, vec![42; 8 * 1024 * 1024]);
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

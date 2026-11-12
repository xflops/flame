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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[cfg(test)]
use bytes::Bytes;
use common::ctx::{FlameCache, FlameCluster};
use common::net::{host_for_uri, ip_addr, system_interfaces};
use common::FlameError;
#[cfg(test)]
use futures::StreamExt;
use url::Url;

use crate::model::{ObjectFilter, ObjectKey, ObjectMetadata};
use crate::storage::{self, ByteStream, StorageEngine, StorageEnginePtr};
use gc::GarbageCollector;

mod gc;
pub(crate) mod grpc;
#[cfg(test)]
mod tests;
pub(crate) use grpc::GrpcCacheServer;
use rpc::flame::v1::object_cache_service_server::ObjectCacheServiceServer;

pub(crate) struct WriteRequest {
    pub(crate) key: ObjectKey,
    pub(crate) data_type: String,
    pub(crate) size: u64,
    pub(crate) chunks: ByteStream,
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

    /// Parse a cache endpoint URL, requiring a host and defaulting its port to 9090.
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

/// Serve filesystem objects through this replica's advertised RPC endpoint.
pub struct Controller {
    endpoint: CacheEndpoint,
    pub(crate) storage: Arc<dyn StorageEngine>,
}
impl Controller {
    /// Own filesystem persistence and advertise this replica in response metadata.
    pub(crate) fn new(endpoint: CacheEndpoint, storage: StorageEnginePtr) -> Self {
        Self {
            endpoint,
            storage: Arc::from(storage),
        }
    }

    /// Forward object bytes to storage, creating its object if needed and allocating a part.
    pub(crate) async fn put_stream(
        self: &Arc<Self>,
        request: WriteRequest,
    ) -> Result<ObjectMetadata, FlameError> {
        self.storage
            .write_object(
                &request.key,
                &request.data_type,
                request.size,
                request.chunks,
            )
            .await
            .map(|metadata| metadata.with_endpoint(self.endpoint.to_uri()))
    }

    /// Forward patch bytes through the same create-if-missing storage behavior.
    pub(crate) async fn patch_stream(
        self: &Arc<Self>,
        request: WriteRequest,
    ) -> Result<ObjectMetadata, FlameError> {
        self.storage
            .write_patch(
                &request.key,
                &request.data_type,
                request.size,
                request.chunks,
            )
            .await
            .map(|metadata| metadata.with_endpoint(self.endpoint.to_uri()))
    }

    /// Append one caller-provided payload through the same direct streaming path in fixtures.
    #[cfg(test)]
    pub(crate) async fn patch(
        self: &Arc<Self>,
        key: &ObjectKey,
        data_type: &str,
        data: Bytes,
    ) -> Result<ObjectMetadata, FlameError> {
        self.patch_stream(WriteRequest {
            key: key.clone(),
            data_type: data_type.to_owned(),
            size: data.len() as u64,
            chunks: futures::stream::once(async move { Ok(data) }).boxed(),
        })
        .await
    }

    /// Own GC filesystem deletion through completion even if its waiting caller is canceled.
    pub(super) async fn delete(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let storage = Arc::clone(&self.storage);
        let key = key.clone();
        tokio::spawn(async move { storage.delete_objects(&key).await })
            .await
            .map_err(|error| {
                FlameError::Internal(format!("object deletion task failed: {error}"))
            })?
    }

    /// Query metadata; storage protects any filesystem prefix projection internally.
    pub(crate) async fn list(
        &self,
        filter: &ObjectFilter,
    ) -> Result<Vec<ObjectMetadata>, FlameError> {
        self.storage.list(filter).await
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

/// The address placed in object references. `cache.advertise` is optional.
/// An explicit value selects an interface's IP or supplies an address (URL,
/// hostname, or host:port). When unset, the environment override is resolved
/// the same way; with neither, use the first private (intranet) IPv4 address of a
/// non-loopback interface, or `cache.endpoint` as configured if there is none.
fn advertised_endpoint(cache_config: &FlameCache) -> Result<CacheEndpoint, FlameError> {
    let from_env = std::env::var(ADVERTISE_ENV).ok();
    let endpoint = CacheEndpoint::try_from(&cache_config.endpoint)?;
    let value = cache_config
        .advertise
        .clone()
        .filter(|advertise| !advertise.is_empty())
        .or_else(|| from_env.filter(|advertise| !advertise.is_empty()));
    if let Some(value) = value {
        if value.contains("://") {
            return CacheEndpoint::try_from(&value);
        }
        if let Some(ip) = ip_addr(&value)? {
            return Ok(CacheEndpoint {
                scheme: endpoint.scheme.clone(),
                host: ip.to_string(),
                port: endpoint.port,
            });
        }
        let url = Url::parse(&format!("{}://{}", endpoint.scheme, value))
            .map_err(|_| FlameError::InvalidConfig(format!("invalid advertise <{}>", value)))?;
        let host = url.host_str().ok_or_else(|| {
            FlameError::InvalidConfig(format!("no host in advertise <{}>", value))
        })?;
        return Ok(CacheEndpoint {
            scheme: endpoint.scheme.clone(),
            host: host.to_string(),
            port: url.port().unwrap_or(endpoint.port),
        });
    }

    let interfaces = system_interfaces()?;
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

/// Bind explicit IP/interface selectors; other advertised addresses retain
/// wildcard listening. The listener port always comes from cache.endpoint.
fn bind_address(cache_config: &FlameCache) -> Result<SocketAddr, FlameError> {
    let env_advertise = std::env::var(ADVERTISE_ENV).ok();
    let endpoint = CacheEndpoint::try_from(&cache_config.endpoint)?;
    let value = cache_config
        .advertise
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| env_advertise.as_deref().filter(|value| !value.is_empty()));
    let ip = match value {
        Some(value) => ip_addr(value)?.unwrap_or(Ipv4Addr::UNSPECIFIED.into()),
        None => Ipv4Addr::UNSPECIFIED.into(),
    };
    Ok(SocketAddr::new(ip, endpoint.port))
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
    // Explicit IP/interface advertisements also select the local listener IP.
    // DNS advertisements and default address discovery keep wildcard binding.
    let addr = bind_address(cache_config)?;
    let advertised = advertised_endpoint(cache_config)?;
    tracing::info!(
        bind = %addr,
        advertise = %advertised.to_uri(),
        "Object cache addresses"
    );

    let storage_url = cache_config.storage.as_deref().unwrap_or("fs://cache");
    let storage = storage::connect(storage_url, &cache_config.pool).await?;
    let controller = Arc::new(Controller::new(advertised, storage));

    let collector = GarbageCollector::new(
        Arc::clone(&controller),
        cluster_config,
        cache_config.gc.interval,
    )?;
    let gc_handle = tokio::spawn(collector.run());

    let grpc_server = GrpcCacheServer::new(Arc::clone(&controller));

    tracing::info!("Starting object cache gRPC server at {}", addr);

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

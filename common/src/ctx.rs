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

use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::PathBuf;

use bytesize::ByteSize;
use serde_derive::{Deserialize, Serialize};
use tonic::transport::server::ServerTlsConfig;
use tonic::transport::{Certificate, ClientTlsConfig, Identity};

use crate::apis::{ResourceRequirement, Shim};
use crate::FlameError;

const DEFAULT_FLAME_CONF: &str = "flame-cluster.yaml";
const DEFAULT_CONTEXT_NAME: &str = "flame";
const DEFAULT_FLAME_ENDPOINT: &str = "http://127.0.0.1:8080";
/// Default policies to enable when none specified in config.
/// Available configurable policies: "priority", "drf", "das"
/// Note: "shim" plugin is always enabled (required for executor matching)
pub const DEFAULT_POLICIES: &[&str] = &["priority", "drf", "das"];
const DEFAULT_STORAGE: &str = "sqlite://flame.db";
const DEFAULT_MAX_EXECUTORS_PER_NODE: u32 = 128;
pub const DEFAULT_SESSION_RETRY_LIMITS: u32 = 5;
const DEFAULT_SCHEDULE_INTERVAL: u64 = 100;
const DEFAULT_SHIM: &str = "host";
const DEFAULT_FLAME_CACHE_ENDPOINT: &str = "http://127.0.0.1:9090";
const DEFAULT_EVICTION_POLICY: &str = "lru";
const DEFAULT_MAX_MEMORY: &str = "1G";

// ============================================================
// YAML deserialization structs (serde layer)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameClusterContextYaml {
    pub cluster: FlameClusterYaml,
    pub cache: Option<FlameCacheYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameClusterYaml {
    pub name: String,
    pub endpoint: String,
    /// Cluster-wide default `resreq` for sessions that supply no explicit
    /// `resreq`. String form `"cpu=2,mem=4g,gpu=1"` parsed by
    /// `ResourceRequirement::parse`.
    pub resreq: Option<String>,
    pub policies: Option<Vec<String>>,
    pub storage: Option<String>,
    /// Maximum scheduler wait in milliseconds when no state change occurs.
    pub schedule_interval: Option<u64>,
    /// Executors configuration (moved under cluster)
    pub executors: Option<FlameExecutorsYaml>,
    /// TLS configuration for Session Manager
    pub tls: Option<FlameTlsYaml>,
    /// Resource limits configuration
    pub limits: Option<FlameLimitsYaml>,
    /// pprof profiling configuration
    pub pprof: Option<FlamePprofYaml>,
    /// Recovery configuration
    pub recovery: Option<FlameRecoveryYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameRecoveryYaml {
    pub session: Option<FlameSessionRecoveryYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameSessionRecoveryYaml {
    pub retry_limits: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameExecutorsYaml {
    pub shims: Option<Vec<String>>,
    pub devices: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameApplicationLimitsYaml {
    pub max_executors: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameSessionLimitsYaml {
    pub max_executors: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameNodeLimitsYaml {
    pub max_executors: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameLimitsYaml {
    pub max_sessions: Option<usize>,
    pub application: Option<FlameApplicationLimitsYaml>,
    pub session: Option<FlameSessionLimitsYaml>,
    pub node: Option<FlameNodeLimitsYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameTlsYaml {
    /// Path to PEM-encoded server certificate
    pub cert_file: Option<String>,
    /// Path to PEM-encoded private key
    pub key_file: Option<String>,
    /// Path to PEM-encoded CA certificate (for certificate chain validation)
    pub ca_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameCacheYaml {
    pub endpoint: Option<String>,
    /// Address placed in object references: a network interface name (regex)
    /// or an address (defaults to `endpoint`)
    pub advertise: Option<String>,
    pub storage: Option<String>,
    pub eviction: Option<FlameEvictionYaml>,
    pub gc: Option<FlameCacheGcYaml>,
    /// TLS configuration for Object Cache (optional, independent from cluster.tls)
    pub tls: Option<FlameTlsYaml>,
    /// pprof profiling configuration
    pub pprof: Option<FlamePprofYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlameCacheGcYaml {
    pub interval: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameEvictionYaml {
    /// Eviction policy: "lru" or "none"
    pub policy: Option<String>,
    /// Maximum memory for cached objects (string with units: "1G", "512M", "1024K")
    pub max_memory: Option<String>,
    /// Maximum number of objects in memory
    pub max_objects: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlamePprofYaml {
    pub port: Option<u16>,
}

// ============================================================
// Runtime structs (validated, with helper methods)
// ============================================================

#[derive(Debug, Clone, Default)]
pub struct FlameClusterContext {
    pub cluster: FlameCluster,
    pub cache: Option<FlameCache>,
}

#[derive(Debug, Clone)]
pub struct FlameCluster {
    pub name: String,
    pub endpoint: String,
    /// Cluster-wide default `resreq`. Applied server-side when a session
    /// spec sets no explicit `resreq`. `None` if not configured.
    pub resreq: Option<ResourceRequirement>,
    pub policies: Vec<String>,
    pub storage: String,
    pub schedule_interval: u64,
    pub executors: FlameExecutors,
    pub tls: Option<FlameTls>,
    pub limits: FlameLimits,
    pub recovery: FlameRecovery,
    pub pprof: Option<FlamePprof>,
}

#[derive(Debug, Clone)]
pub struct FlameExecutors {
    /// Every shim this executor manager can run. An executor keeps the shim of its
    /// application when it is listed here; the first one is the default, for an
    /// executor that asks for a shim that is not.
    pub shims: Vec<Shim>,
    /// Optional device managers loaded by FEM; CPU and memory are always loaded.
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FlameLimits {
    /// Maximum number of sessions across the entire cluster.
    pub max_sessions: Option<usize>,
    pub application: FlameApplicationLimits,
    pub session: FlameSessionLimits,
    pub node: FlameNodeLimits,
}

#[derive(Debug, Clone, Default)]
pub struct FlameApplicationLimits {
    /// Hard ceiling on executors belonging to each application.
    /// None leaves application-specific limits in effect.
    pub max_executors: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct FlameSessionLimits {
    /// Hard ceiling on executor count, including explicitly requested limits.
    /// None leaves this scope without an additional cluster ceiling.
    pub max_executors: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FlameNodeLimits {
    /// Maximum number of non-Released executors allowed on each node.
    pub max_executors: u32,
}

#[derive(Debug, Clone, Default)]
pub struct FlameRecovery {
    pub session: FlameSessionRecovery,
}

#[derive(Debug, Clone)]
pub struct FlameSessionRecovery {
    pub retry_limits: u32,
}

#[derive(Debug, Clone)]
pub struct FlamePprof {
    pub port: u16,
}

const DEFAULT_PPROF_PORT: u16 = 6060;

/// TLS configuration for Flame services.
///
/// When this struct is present and valid (cert_file + key_file configured),
/// TLS is enabled for the service.
#[derive(Debug, Clone)]
pub struct FlameTls {
    /// Path to PEM-encoded server certificate
    pub cert_file: String,
    /// Path to PEM-encoded private key
    pub key_file: String,
    /// Path to PEM-encoded CA certificate (optional)
    pub ca_file: Option<String>,
}

impl FlameTls {
    /// Load server TLS config for tonic.
    /// Returns ServerTlsConfig with identity loaded from cert/key files.
    pub fn server_tls_config(&self) -> Result<ServerTlsConfig, FlameError> {
        let cert = fs::read_to_string(&self.cert_file).map_err(|e| {
            FlameError::InvalidConfig(format!(
                "failed to read cert_file <{}>: {}",
                self.cert_file, e
            ))
        })?;
        let key = fs::read_to_string(&self.key_file).map_err(|e| {
            FlameError::InvalidConfig(format!(
                "failed to read key_file <{}>: {}",
                self.key_file, e
            ))
        })?;

        let identity = Identity::from_pem(cert, key);
        let config = ServerTlsConfig::new().identity(identity);

        Ok(config)
    }

    /// Load client TLS config for tonic.
    /// If ca_file is specified, use it; otherwise use system CA bundle.
    pub fn client_tls_config(&self) -> Result<ClientTlsConfig, FlameError> {
        let mut config = ClientTlsConfig::new();

        if let Some(ca_file) = &self.ca_file {
            let ca = fs::read_to_string(ca_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read ca_file <{}>: {}", ca_file, e))
            })?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }

        Ok(config)
    }
}

#[derive(Debug, Clone, Default)]
pub struct FlameCache {
    pub endpoint: String,
    /// Address placed in the references returned by the cache, reachable by
    /// every client: a network interface name (its first IPv4 address is used)
    /// or an address. `None` uses the environment or the host's private address.
    pub advertise: Option<String>,
    pub storage: Option<String>,
    pub eviction: FlameEviction,
    pub gc: FlameCacheGc,
    /// TLS configuration for Object Cache (optional, independent from cluster.tls)
    pub tls: Option<FlameTls>,
    pub pprof: Option<FlamePprof>,
}

#[derive(Debug, Clone)]
pub struct FlameCacheGc {
    pub interval: std::time::Duration,
}

const DEFAULT_CACHE_GC_INTERVAL_SECS: u64 = 60;

impl Default for FlameCacheGc {
    fn default() -> Self {
        Self {
            interval: std::time::Duration::from_secs(DEFAULT_CACHE_GC_INTERVAL_SECS),
        }
    }
}

impl FlameCache {
    /// Check if cache endpoint requires TLS (grpcs:// scheme)
    pub fn requires_tls(&self) -> bool {
        self.endpoint.starts_with("grpcs://")
    }
}

impl FlameCluster {
    /// Check if cluster endpoint requires TLS (https:// scheme)
    pub fn requires_tls(&self) -> bool {
        self.endpoint.starts_with("https://")
    }
}

/// Eviction configuration for the cache.
#[derive(Debug, Clone)]
pub struct FlameEviction {
    /// Eviction policy: "lru" or "none"
    pub policy: String,
    /// Maximum memory for cached objects in bytes
    pub max_memory: u64,
    /// Maximum number of objects in memory (None means unlimited)
    pub max_objects: Option<usize>,
}

impl Default for FlameEviction {
    fn default() -> Self {
        let default_max_memory =
            parse_memory_size(DEFAULT_MAX_MEMORY).unwrap_or(1024 * 1024 * 1024); // 1GiB fallback
        FlameEviction {
            policy: DEFAULT_EVICTION_POLICY.to_string(),
            max_memory: default_max_memory,
            max_objects: None,
        }
    }
}

impl Display for FlameClusterContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "name: {}, endpoint: {}",
            self.cluster.name, self.cluster.endpoint
        )
    }
}

/// Convert SI unit suffixes to binary (IEC) unit suffixes.
///
/// **Why this is needed:** The `bytesize` crate treats G/M/K as SI (decimal) units:
/// - bytesize: "1G" = 1,000,000,000 bytes (1 GB, powers of 1000)
/// - bytesize: "1GiB" = 1,073,741,824 bytes (1 GiB, powers of 1024)
///
/// For memory configuration, we want "1G" to mean 1 GiB (binary), not 1 GB (decimal).
/// This helper converts SI suffixes to IEC suffixes before parsing with bytesize.
fn convert_to_binary_units(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return s.to_string();
    }

    // Find where the numeric part ends and the unit begins
    let unit_start = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(unit_start);

    // Convert SI units to binary units (case-insensitive)
    let unit_upper = unit_part.to_uppercase();
    let binary_unit = match unit_upper.as_str() {
        "GI" | "GIB" => "GiB",
        "MI" | "MIB" => "MiB",
        "KI" | "KIB" => "KiB",
        "TI" | "TIB" => "TiB",
        "PI" | "PIB" => "PiB",
        "G" | "GB" => "GiB",
        "M" | "MB" => "MiB",
        "K" | "KB" => "KiB",
        "T" | "TB" => "TiB",
        "P" | "PB" => "PiB",
        // Already binary or bytes - pass through
        _ => unit_part,
    };

    format!("{}{}", num_part, binary_unit)
}

/// Parse a memory size string with optional unit suffix using bytesize crate.
/// Treats G/M/K as binary units (GiB/MiB/KiB) for memory configuration.
/// Returns the size in bytes.
///
/// Examples:
/// - "1G" or "1g" or "1GB" -> 1073741824 bytes (1 GiB)
/// - "512M" or "512m" or "512MB" -> 536870912 bytes (512 MiB)
/// - "1024K" or "1024k" or "1024KB" -> 1048576 bytes (1024 KiB)
/// - "512" (no unit) -> 512 bytes
pub fn parse_memory_size(s: &str) -> Result<u64, FlameError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(FlameError::InvalidConfig(
            "empty memory size string".to_string(),
        ));
    }

    // Convert SI units to binary units before parsing
    let binary_str = convert_to_binary_units(s);

    binary_str
        .parse::<ByteSize>()
        .map(|bs| bs.as_u64())
        .map_err(|e| FlameError::InvalidConfig(format!("invalid memory size '{}': {}", s, e)))
}

impl FlameClusterContext {
    pub fn config_path(fp: Option<&str>) -> PathBuf {
        fp.map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(format!(
                "{}/.flame/{}",
                env!("HOME", "."),
                DEFAULT_FLAME_CONF
            ))
        })
    }

    pub fn from_file(fp: Option<String>) -> Result<Self, FlameError> {
        let fp = Self::config_path(fp.as_deref());

        if !fp.is_file() {
            return Err(FlameError::InvalidConfig(format!(
                "<{}> is not a file",
                fp.display()
            )));
        }

        let contents = fs::read_to_string(&fp).map_err(|e| FlameError::Internal(e.to_string()))?;
        let ctx: FlameClusterContextYaml =
            serde_yaml::from_str(&contents).map_err(|e| FlameError::Internal(e.to_string()))?;

        tracing::debug!("Load FlameClusterContext from <{}>: {ctx:?}", fp.display());

        FlameClusterContext::try_from(ctx)
    }
}

impl TryFrom<FlameClusterContextYaml> for FlameClusterContext {
    type Error = FlameError;
    fn try_from(ctx: FlameClusterContextYaml) -> Result<Self, Self::Error> {
        let cluster = FlameCluster::try_from(ctx.cluster)?;
        Ok(FlameClusterContext {
            cluster,
            cache: ctx.cache.map(FlameCache::try_from).transpose()?,
        })
    }
}

impl TryFrom<FlameClusterYaml> for FlameCluster {
    type Error = FlameError;
    fn try_from(cluster: FlameClusterYaml) -> Result<Self, Self::Error> {
        let executors = cluster
            .executors
            .map(FlameExecutors::try_from)
            .transpose()?
            .unwrap_or_default();

        let tls = cluster.tls.map(FlameTls::try_from).transpose()?;

        let limits = cluster.limits.map(FlameLimits::from).unwrap_or_default();

        let pprof = cluster.pprof.map(FlamePprof::from);
        let recovery = cluster
            .recovery
            .map(FlameRecovery::from)
            .unwrap_or_default();

        Ok(FlameCluster {
            name: cluster.name,
            endpoint: cluster.endpoint,
            resreq: cluster
                .resreq
                .as_deref()
                .map(ResourceRequirement::parse)
                .transpose()?,
            policies: cluster
                .policies
                .unwrap_or_else(|| DEFAULT_POLICIES.iter().map(|s| s.to_string()).collect()),
            storage: cluster.storage.unwrap_or(DEFAULT_STORAGE.to_string()),
            schedule_interval: cluster
                .schedule_interval
                .unwrap_or(DEFAULT_SCHEDULE_INTERVAL),
            executors,
            tls,
            limits,
            recovery,
            pprof,
        })
    }
}

impl From<FlameRecoveryYaml> for FlameRecovery {
    fn from(yaml: FlameRecoveryYaml) -> Self {
        FlameRecovery {
            session: yaml
                .session
                .map(FlameSessionRecovery::from)
                .unwrap_or_default(),
        }
    }
}

impl From<FlameSessionRecoveryYaml> for FlameSessionRecovery {
    fn from(yaml: FlameSessionRecoveryYaml) -> Self {
        FlameSessionRecovery {
            retry_limits: yaml.retry_limits.unwrap_or(DEFAULT_SESSION_RETRY_LIMITS),
        }
    }
}

impl Default for FlameSessionRecovery {
    fn default() -> Self {
        FlameSessionRecovery {
            retry_limits: DEFAULT_SESSION_RETRY_LIMITS,
        }
    }
}

impl TryFrom<FlameExecutorsYaml> for FlameExecutors {
    type Error = FlameError;
    fn try_from(executors: FlameExecutorsYaml) -> Result<Self, Self::Error> {
        let devices = executors
            .devices
            .unwrap_or_else(|| FlameExecutors::default().devices);
        let mut names = HashSet::new();
        for name in &devices {
            if !names.insert(name) {
                return Err(FlameError::InvalidConfig(format!(
                    "duplicate device manager <{}> in cluster.executors.devices",
                    name
                )));
            }
        }
        let shims = match executors.shims {
            Some(names) => {
                if names.is_empty() {
                    return Err(FlameError::InvalidConfig(
                        "cluster.executors.shims must list at least one shim".to_string(),
                    ));
                }
                let shims = names
                    .into_iter()
                    .map(Shim::try_from)
                    .collect::<Result<Vec<_>, _>>()?;
                let mut seen = HashSet::new();
                if let Some(duplicate) = shims.iter().find(|shim| !seen.insert(**shim)) {
                    return Err(FlameError::InvalidConfig(format!(
                        "duplicate shim <{:?}> in cluster.executors.shims",
                        duplicate
                    )));
                }
                shims
            }
            None => vec![Shim::try_from(DEFAULT_SHIM.to_string())?],
        };
        Ok(FlameExecutors { shims, devices })
    }
}

impl FlameExecutors {
    /// The default shim: the first one listed.
    pub fn default_shim(&self) -> Shim {
        self.shims[0]
    }

    /// The shim of an executor that asks for `requested`: that one when it is
    /// supported, the default one otherwise.
    pub fn shim_for(&self, requested: Shim) -> Shim {
        if self.shims.contains(&requested) {
            requested
        } else {
            self.default_shim()
        }
    }
}

impl Default for FlameExecutors {
    fn default() -> Self {
        Self {
            shims: vec![Shim::default()],
            devices: vec!["gpu".to_string()],
        }
    }
}

impl From<FlameLimitsYaml> for FlameLimits {
    fn from(yaml: FlameLimitsYaml) -> Self {
        FlameLimits {
            max_sessions: yaml.max_sessions,
            application: yaml
                .application
                .map(FlameApplicationLimits::from)
                .unwrap_or_default(),
            session: yaml
                .session
                .map(FlameSessionLimits::from)
                .unwrap_or_default(),
            node: FlameNodeLimits {
                max_executors: yaml
                    .node
                    .and_then(|node| node.max_executors)
                    .unwrap_or(DEFAULT_MAX_EXECUTORS_PER_NODE),
            },
        }
    }
}

impl From<FlameApplicationLimitsYaml> for FlameApplicationLimits {
    fn from(yaml: FlameApplicationLimitsYaml) -> Self {
        Self {
            max_executors: yaml.max_executors,
        }
    }
}

impl From<FlameSessionLimitsYaml> for FlameSessionLimits {
    fn from(yaml: FlameSessionLimitsYaml) -> Self {
        Self {
            max_executors: yaml.max_executors,
        }
    }
}

impl Default for FlameNodeLimits {
    fn default() -> Self {
        Self {
            max_executors: DEFAULT_MAX_EXECUTORS_PER_NODE,
        }
    }
}

impl From<FlamePprofYaml> for FlamePprof {
    fn from(yaml: FlamePprofYaml) -> Self {
        FlamePprof {
            port: yaml.port.unwrap_or(DEFAULT_PPROF_PORT),
        }
    }
}

impl Default for FlameCluster {
    fn default() -> Self {
        FlameCluster {
            name: DEFAULT_CONTEXT_NAME.to_string(),
            endpoint: DEFAULT_FLAME_ENDPOINT.to_string(),
            resreq: None,
            policies: DEFAULT_POLICIES.iter().map(|s| s.to_string()).collect(),
            storage: DEFAULT_STORAGE.to_string(),
            schedule_interval: DEFAULT_SCHEDULE_INTERVAL,
            executors: FlameExecutors::default(),
            tls: None,
            limits: FlameLimits::default(),
            recovery: FlameRecovery::default(),
            pprof: None,
        }
    }
}

impl TryFrom<FlameTlsYaml> for FlameTls {
    type Error = FlameError;
    fn try_from(yaml: FlameTlsYaml) -> Result<Self, Self::Error> {
        let cert_file = yaml
            .cert_file
            .ok_or_else(|| FlameError::InvalidConfig("tls.cert_file is required".to_string()))?;
        let key_file = yaml
            .key_file
            .ok_or_else(|| FlameError::InvalidConfig("tls.key_file is required".to_string()))?;

        // Note: File existence is validated when loading certificates in server_tls_config()
        // and client_tls_config() methods, which provide more descriptive error messages.

        Ok(FlameTls {
            cert_file,
            key_file,
            ca_file: yaml.ca_file,
        })
    }
}

impl TryFrom<FlameCacheYaml> for FlameCache {
    type Error = FlameError;
    fn try_from(cache: FlameCacheYaml) -> Result<Self, Self::Error> {
        let tls = cache.tls.map(FlameTls::try_from).transpose()?;
        let pprof = cache.pprof.map(FlamePprof::from);
        let gc = cache
            .gc
            .map(FlameCacheGc::try_from)
            .transpose()?
            .unwrap_or_default();

        Ok(FlameCache {
            endpoint: cache
                .endpoint
                .unwrap_or(DEFAULT_FLAME_CACHE_ENDPOINT.to_string()),
            advertise: cache.advertise.filter(|advertise| !advertise.is_empty()),
            storage: cache.storage,
            eviction: cache
                .eviction
                .map(FlameEviction::try_from)
                .transpose()?
                .unwrap_or_default(),
            gc,
            tls,
            pprof,
        })
    }
}

impl TryFrom<FlameCacheGcYaml> for FlameCacheGc {
    type Error = FlameError;

    fn try_from(gc: FlameCacheGcYaml) -> Result<Self, Self::Error> {
        let Some(interval) = gc.interval else {
            return Ok(Self::default());
        };
        let interval = humantime::parse_duration(&interval).map_err(|error| {
            FlameError::InvalidConfig(format!(
                "invalid cache.gc.interval <{}>: {}",
                interval, error
            ))
        })?;
        if interval.is_zero() {
            return Err(FlameError::InvalidConfig(
                "cache.gc.interval must be greater than zero".to_string(),
            ));
        }

        Ok(Self { interval })
    }
}

impl TryFrom<FlameEvictionYaml> for FlameEviction {
    type Error = FlameError;
    fn try_from(eviction: FlameEvictionYaml) -> Result<Self, Self::Error> {
        // Parse max_memory using bytesize crate
        let max_memory = if let Some(ref max_memory_str) = eviction.max_memory {
            parse_memory_size(max_memory_str)?
        } else {
            parse_memory_size(DEFAULT_MAX_MEMORY)?
        };

        Ok(FlameEviction {
            policy: eviction
                .policy
                .unwrap_or(DEFAULT_EVICTION_POLICY.to_string()),
            max_memory,
            max_objects: eviction.max_objects,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn executors(yaml: &str) -> Result<FlameExecutors, FlameError> {
        FlameExecutors::try_from(serde_yaml::from_str::<FlameExecutorsYaml>(yaml).unwrap())
    }

    #[test]
    fn executor_shims_default_to_the_host_shim() {
        let executors = executors("devices: []").unwrap();
        assert_eq!(executors.shims, [Shim::Host]);
        assert_eq!(executors.default_shim(), Shim::Host);
        assert_eq!(FlameExecutors::default().shims, [Shim::default()]);
    }

    #[test]
    fn executor_shims_with_one_shim_run_only_that_shim() {
        let executors = executors("shims: [cri]").unwrap();
        assert_eq!(executors.shims, [Shim::Cri]);
        assert_eq!(executors.default_shim(), Shim::Cri);
    }

    #[test]
    fn executor_shims_list_every_supported_shim_and_the_first_is_the_default() {
        let executors = executors("shims: [cri, host]").unwrap();
        assert_eq!(executors.shims, [Shim::Cri, Shim::Host]);
        assert_eq!(executors.default_shim(), Shim::Cri);
    }

    #[test]
    fn executor_keeps_a_supported_shim_and_falls_back_to_the_default() {
        let both = executors("shims: [cri, host]").unwrap();
        assert_eq!(both.shim_for(Shim::Host), Shim::Host);
        assert_eq!(both.shim_for(Shim::Cri), Shim::Cri);

        let cri = executors("shims: [cri]").unwrap();
        assert_eq!(cri.shim_for(Shim::Host), Shim::Cri);
        assert_eq!(cri.shim_for(Shim::Cri), Shim::Cri);
    }

    #[test]
    fn executor_shims_reject_bad_lists() {
        assert!(executors("shims: []").is_err());
        assert!(executors("shims: [host, host]").is_err());
        assert!(executors("shims: [nope]").is_err());
    }

    #[test]
    fn executor_devices_default_to_gpu() {
        let yaml: FlameExecutorsYaml = serde_yaml::from_str("shims: [host]").unwrap();
        let executors = FlameExecutors::try_from(yaml).unwrap();
        assert_eq!(executors.devices, ["gpu"]);
        assert_eq!(FlameExecutors::default().devices, ["gpu"]);
        assert_eq!(FlameCluster::default().executors.devices, ["gpu"]);
    }

    #[test]
    fn executor_devices_explicit_empty_disables_managers() {
        let yaml: FlameExecutorsYaml = serde_yaml::from_str("devices: []").unwrap();
        assert!(FlameExecutors::try_from(yaml).unwrap().devices.is_empty());
    }

    #[test]
    fn executor_devices_preserve_configured_names() {
        let yaml: FlameExecutorsYaml = serde_yaml::from_str("devices: [gpu, tpu]").unwrap();
        assert_eq!(
            FlameExecutors::try_from(yaml).unwrap().devices,
            ["gpu", "tpu"]
        );
    }

    #[test]
    fn executor_devices_reject_duplicates() {
        let yaml: FlameExecutorsYaml = serde_yaml::from_str("devices: [gpu, gpu]").unwrap();
        let error = FlameExecutors::try_from(yaml).unwrap_err();
        assert!(matches!(error, FlameError::InvalidConfig(_)));
        assert!(error.to_string().contains("duplicate device manager <gpu>"));
    }

    #[test]
    fn test_executor_limit_defaults() {
        let limits = FlameLimits::default();
        assert_eq!(limits.max_sessions, None);
        assert_eq!(limits.application.max_executors, None);
        assert_eq!(limits.session.max_executors, None);
        assert_eq!(limits.node.max_executors, DEFAULT_MAX_EXECUTORS_PER_NODE);
    }

    #[test]
    fn test_executor_limits_partial_scopes() {
        let yaml: FlameLimitsYaml =
            serde_yaml::from_str("max_sessions: 50\napplication:\n  max_executors: 12\n").unwrap();
        let limits = FlameLimits::from(yaml);
        assert_eq!(limits.max_sessions, Some(50));
        assert_eq!(limits.application.max_executors, Some(12));
        assert_eq!(limits.session.max_executors, None);
        assert_eq!(limits.node.max_executors, DEFAULT_MAX_EXECUTORS_PER_NODE);

        let yaml: FlameLimitsYaml =
            serde_yaml::from_str("session:\n  max_executors: 3\nnode:\n  max_executors: 8\n")
                .unwrap();
        let limits = FlameLimits::from(yaml);
        assert_eq!(limits.application.max_executors, None);
        assert_eq!(limits.session.max_executors, Some(3));
        assert_eq!(limits.node.max_executors, 8);
    }

    #[test]
    fn test_executor_limits_preserve_zero() {
        let yaml: FlameLimitsYaml = serde_yaml::from_str(
            "application:\n  max_executors: 0\nsession:\n  max_executors: 0\nnode:\n  max_executors: 0\n",
        )
        .unwrap();
        let limits = FlameLimits::from(yaml);
        assert_eq!(limits.application.max_executors, Some(0));
        assert_eq!(limits.session.max_executors, Some(0));
        assert_eq!(limits.node.max_executors, 0);

        let yaml: FlameLimitsYaml = serde_yaml::from_str("node: {}\n").unwrap();
        assert_eq!(
            FlameLimits::from(yaml).node.max_executors,
            DEFAULT_MAX_EXECUTORS_PER_NODE
        );
    }

    #[test]
    fn test_flame_context_from_file() -> Result<(), FlameError> {
        // Executor limits explicitly identify their scope.
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=1,mem=1g"
  policies:
    - priority
    - drf
  storage: sqlite://flame.db
  executors:
    shims: [host]
  limits:
    node:
      max_executors: 10
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))
            .map_err(|e| FlameError::Internal(e.to_string()))?;
        assert_eq!(ctx.cluster.name, "flame");
        assert_eq!(ctx.cluster.endpoint, "http://flame-session-manager:8080");
        assert_eq!(
            ctx.cluster.resreq,
            Some(ResourceRequirement::from("cpu=1,mem=1g"))
        );
        assert_eq!(ctx.cluster.policies, vec!["priority", "drf"]);
        assert_eq!(ctx.cluster.storage, "sqlite://flame.db");
        assert_eq!(ctx.cluster.executors.shims, [Shim::Host]);
        assert_eq!(ctx.cluster.limits.node.max_executors, 10);

        Ok(())
    }

    #[test]
    fn test_flame_context_with_session_recovery_retry_limits() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  recovery:
    session:
      retry_limits: 2
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        assert_eq!(ctx.cluster.recovery.session.retry_limits, 2);

        Ok(())
    }

    #[test]
    fn test_flame_context_with_cache_eviction() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  storage: "/var/lib/flame/cache"
  eviction:
    policy: "lru"
    max_memory: "512M"
    max_objects: 5000
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        assert!(ctx.cache.is_some());
        let cache = ctx.cache.unwrap();
        assert_eq!(cache.endpoint, "grpc://127.0.0.1:9090");
        assert_eq!(cache.storage, Some("/var/lib/flame/cache".to_string()));
        assert_eq!(cache.eviction.policy, "lru");
        assert_eq!(cache.eviction.max_memory, 512 * 1024 * 1024); // 512MB in bytes
        assert_eq!(cache.eviction.max_objects, Some(5000));

        Ok(())
    }

    fn cache_context(cache_yaml: &str) -> Result<FlameCache, FlameError> {
        let context_string = format!(
            "---\ncluster:\n  name: flame\n  endpoint: \"http://flame-session-manager:8080\"\ncache:\n{}",
            cache_yaml
        );
        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;
        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;
        Ok(ctx.cache.unwrap())
    }

    #[test]
    fn test_flame_context_cache_advertise_default() -> Result<(), FlameError> {
        let cache = cache_context("  endpoint: \"grpc://127.0.0.1:9090\"\n")?;
        assert_eq!(cache.advertise, None);
        // An empty value counts as unset.
        let cache = cache_context("  advertise: \"\"\n")?;
        assert_eq!(cache.advertise, None);
        Ok(())
    }

    #[test]
    fn test_flame_context_cache_advertise() -> Result<(), FlameError> {
        let cache = cache_context("  advertise: \"grpc://cache-0.cache.flame.svc:9090\"\n")?;
        assert_eq!(
            cache.advertise.as_deref(),
            Some("grpc://cache-0.cache.flame.svc:9090")
        );
        Ok(())
    }

    #[test]
    fn test_flame_context_cache_ignores_legacy_network_interface() -> Result<(), FlameError> {
        // Old configs that still set `network_interface` keep loading.
        let cache = cache_context(
            "  endpoint: \"grpc://127.0.0.1:9090\"\n  network_interface: \"eth0\"\n",
        )?;
        assert_eq!(cache.endpoint, "grpc://127.0.0.1:9090");
        assert_eq!(cache.advertise, None);
        Ok(())
    }

    #[test]
    fn test_flame_context_with_cache_no_eviction() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  eviction:
    policy: "none"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))
            .map_err(|e| FlameError::Internal(e.to_string()))?;

        assert!(ctx.cache.is_some());
        let cache = ctx.cache.unwrap();
        assert_eq!(cache.eviction.policy, "none");
        // Default max_memory should be applied (1G = 1073741824 bytes)
        assert_eq!(cache.eviction.max_memory, 1024 * 1024 * 1024);
        assert_eq!(cache.eviction.max_objects, None);

        Ok(())
    }

    #[test]
    fn test_flame_context_with_cache_default_eviction() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))
            .map_err(|e| FlameError::Internal(e.to_string()))?;

        assert!(ctx.cache.is_some());
        let cache = ctx.cache.unwrap();
        // Default eviction config should be applied
        assert_eq!(cache.eviction.policy, DEFAULT_EVICTION_POLICY);
        assert_eq!(cache.eviction.max_memory, 1024 * 1024 * 1024); // 1G in bytes
        assert_eq!(cache.eviction.max_objects, None);
        assert_eq!(
            cache.gc.interval,
            std::time::Duration::from_secs(DEFAULT_CACHE_GC_INTERVAL_SECS)
        );

        Ok(())
    }

    #[test]
    fn test_flame_context_with_cache_gc() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc:
    interval: 30s
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;
        assert_eq!(
            ctx.cache.unwrap().gc.interval,
            std::time::Duration::from_secs(30)
        );

        Ok(())
    }

    #[test]
    fn test_flame_context_defaults_cache_gc_without_interval() {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc: {}
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).unwrap();

        let ctx =
            FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string())).unwrap();
        assert_eq!(
            ctx.cache.unwrap().gc.interval,
            std::time::Duration::from_secs(DEFAULT_CACHE_GC_INTERVAL_SECS)
        );
    }

    #[test]
    fn test_flame_context_rejects_invalid_cache_gc_interval() {
        for interval in ["0s", "invalid"] {
            let context_string = format!(
                r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc:
    interval: {interval}
        "#
            );

            let tmp_dir = TempDir::new().unwrap();
            let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
            fs::write(&tmp_file, context_string).unwrap();

            assert!(
                FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))
                    .is_err(),
                "interval {interval} should be rejected"
            );
        }
    }

    #[test]
    fn test_flame_context_rejects_unknown_cache_gc_fields() {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
cache:
  gc:
    interval: 60s
    enabled: true
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).unwrap();

        assert!(
            FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string())).is_err()
        );
    }

    // ============================================================
    // Tests for max_memory config parsing with units (using bytesize)
    // ============================================================

    #[test]
    fn test_parse_memory_size_gigabytes_uppercase() {
        // Test uppercase G unit
        assert_eq!(parse_memory_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("4G").unwrap(), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_gigabytes_lowercase() {
        // Test lowercase g unit
        assert_eq!(parse_memory_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_gigabytes_with_b() {
        // Test GB unit
        assert_eq!(parse_memory_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("1gb").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_binary_suffix_without_b() {
        assert_eq!(parse_memory_size("1Gi").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("512Mi").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_size("1024Ki").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_size("2Ti").unwrap(), 2 * 1024_u64.pow(4));
        assert_eq!(parse_memory_size("1Pi").unwrap(), 1024_u64.pow(5));
    }

    #[test]
    fn test_parse_memory_size_terabytes_and_petabytes() {
        assert_eq!(parse_memory_size("1T").unwrap(), 1024_u64.pow(4));
        assert_eq!(parse_memory_size("1TB").unwrap(), 1024_u64.pow(4));
        assert_eq!(parse_memory_size("2P").unwrap(), 2 * 1024_u64.pow(5));
        assert_eq!(parse_memory_size("2PB").unwrap(), 2 * 1024_u64.pow(5));
    }

    #[test]
    fn test_parse_memory_size_megabytes_uppercase() {
        // Test uppercase M unit
        assert_eq!(parse_memory_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_size("1024M").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("256M").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_megabytes_lowercase() {
        // Test lowercase m unit
        assert_eq!(parse_memory_size("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_size("1024m").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_megabytes_with_b() {
        // Test MB unit
        assert_eq!(parse_memory_size("512MB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_size("512mb").unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_kilobytes_uppercase() {
        // Test uppercase K unit
        assert_eq!(parse_memory_size("1024K").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_size("2048K").unwrap(), 2048 * 1024);
    }

    #[test]
    fn test_parse_memory_size_kilobytes_lowercase() {
        // Test lowercase k unit
        assert_eq!(parse_memory_size("1024k").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_size("2048k").unwrap(), 2048 * 1024);
    }

    #[test]
    fn test_parse_memory_size_kilobytes_with_b() {
        // Test KB unit
        assert_eq!(parse_memory_size("1024KB").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_size("1024kb").unwrap(), 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_bytes() {
        // Test B unit and no unit (bytes)
        assert_eq!(parse_memory_size("512B").unwrap(), 512);
        assert_eq!(parse_memory_size("512b").unwrap(), 512);
        assert_eq!(parse_memory_size("1024").unwrap(), 1024);
    }

    #[test]
    fn test_parse_memory_size_with_whitespace() {
        // Test with leading/trailing whitespace
        assert_eq!(parse_memory_size("  512M  ").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_size(" 1G ").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_size_invalid_unit() {
        // Test invalid unit
        let result = parse_memory_size("512X");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
    }

    #[test]
    fn test_parse_memory_size_invalid_number() {
        // Test invalid number
        let result = parse_memory_size("abcM");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
    }

    #[test]
    fn test_parse_memory_size_empty_string() {
        // Test empty string
        let result = parse_memory_size("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
    }

    #[test]
    fn test_parse_memory_size_only_whitespace() {
        // Test only whitespace
        let result = parse_memory_size("   ");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_memory_size_zero() {
        // Test zero value
        assert_eq!(parse_memory_size("0").unwrap(), 0);
        assert_eq!(parse_memory_size("0M").unwrap(), 0);
        assert_eq!(parse_memory_size("0G").unwrap(), 0);
    }

    #[test]
    fn test_parse_memory_size_large_values() {
        // Test large values
        assert_eq!(parse_memory_size("100G").unwrap(), 100 * 1024 * 1024 * 1024);
    }

    // ============================================================
    // Integration tests for max_memory in config files
    // ============================================================

    #[test]
    fn test_flame_context_with_max_memory_gigabytes() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  eviction:
    policy: "lru"
    max_memory: "1G"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        assert!(ctx.cache.is_some());
        let cache = ctx.cache.unwrap();
        assert_eq!(cache.eviction.max_memory, 1024 * 1024 * 1024); // 1G in bytes

        Ok(())
    }

    #[test]
    fn test_flame_context_with_max_memory_megabytes() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  eviction:
    policy: "lru"
    max_memory: "512M"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        assert!(ctx.cache.is_some());
        let cache = ctx.cache.unwrap();
        assert_eq!(cache.eviction.max_memory, 512 * 1024 * 1024); // 512M in bytes

        Ok(())
    }

    #[test]
    fn test_flame_context_with_max_memory_lowercase() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  eviction:
    policy: "lru"
    max_memory: "2g"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        assert!(ctx.cache.is_some());
        let cache = ctx.cache.unwrap();
        assert_eq!(cache.eviction.max_memory, 2 * 1024 * 1024 * 1024); // 2G in bytes

        Ok(())
    }

    #[test]
    fn test_flame_context_with_invalid_max_memory_unit() {
        // Test invalid unit should fail
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  eviction:
    policy: "lru"
    max_memory: "512X"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).unwrap();

        let result = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()));
        assert!(result.is_err());
    }

    // ============================================================
    // Tests for cluster.resreq (cluster-wide default
    // resreq applied server-side when a session spec supplies neither
    // `slots` nor `resreq`).
    // ============================================================

    #[test]
    fn cluster_yaml_with_resreq_parses() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=2,mem=4g,gpu=1"
  executors:
    shims: [host]
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        let resreq = ctx.cluster.resreq.expect("resreq should be parsed");
        assert_eq!(resreq.cpu, 2);
        assert_eq!(resreq.memory, 4 * 1024 * 1024 * 1024);
        assert_eq!(resreq.gpu, 1);

        Ok(())
    }

    #[test]
    fn cluster_yaml_without_resreq_is_none() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))?;

        assert!(ctx.cluster.resreq.is_none());

        Ok(())
    }

    #[test]
    fn cluster_yaml_with_invalid_resreq_fails() {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=two,mem=4g"
  executors:
    shims: [host]
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).unwrap();

        let result = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn test_flame_context_with_invalid_max_memory_number() {
        // Test invalid number should fail
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shims: [host]
cache:
  endpoint: "grpc://127.0.0.1:9090"
  eviction:
    policy: "lru"
    max_memory: "abcM"
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");
        fs::write(&tmp_file, context_string).unwrap();

        let result = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()));
        assert!(result.is_err());
    }
}

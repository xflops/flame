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

use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;

use bytesize::ByteSize;
use serde_derive::{Deserialize, Serialize};
use tonic::transport::server::ServerTlsConfig;
use tonic::transport::{Certificate, ClientTlsConfig, Identity};

use crate::apis::{ResourceRequirement, Shim};
use crate::FlameError;

const DEFAULT_FLAME_CONF: &str = "flame-cluster.yaml";
const DEFAULT_CONTEXT_NAME: &str = "flame";
const DEFAULT_FLAME_ENDPOINT: &str = "http://127.0.0.1:8080";
const DEFAULT_SLOT: &str = "cpu=1,mem=2g";
const DEFAULT_POLICY: &str = "proportion";
const DEFAULT_STORAGE: &str = "sqlite://flame.db";
const DEFAULT_MAX_EXECUTORS_PER_NODE: u32 = 128;
const DEFAULT_SCHEDULE_INTERVAL: u64 = 500;
const DEFAULT_SHIM: &str = "host";
const DEFAULT_FLAME_CACHE_ENDPOINT: &str = "http://127.0.0.1:9090";
const DEFAULT_FLAME_CACHE_NETWORK_INTERFACE: &str = "eth0";
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
    pub slot: Option<String>,
    pub policy: Option<String>,
    pub storage: Option<String>,
    /// Schedule interval in milliseconds for the session scheduler loop
    pub schedule_interval: Option<u64>,
    /// Executors configuration (moved under cluster)
    pub executors: Option<FlameExecutorsYaml>,
    /// TLS configuration for Session Manager
    pub tls: Option<FlameTlsYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameExecutorsYaml {
    pub shim: Option<String>,
    pub limits: Option<FlameExecutorLimitsYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameExecutorLimitsYaml {
    pub max_executors: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameTlsYaml {
    /// Path to PEM-encoded server certificate
    pub cert_file: Option<String>,
    /// Path to PEM-encoded private key
    pub key_file: Option<String>,
    /// Path to PEM-encoded CA certificate (for certificate chain validation)
    pub ca_file: Option<String>,
    /// Path to PEM-encoded CA private key (for signing session certificates)
    pub ca_key_file: Option<String>,
    /// Default validity for session certificates (e.g., "24h", "7d")
    pub cert_validity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameCacheYaml {
    pub endpoint: Option<String>,
    pub network_interface: Option<String>,
    pub storage: Option<String>,
    pub eviction: Option<FlameEvictionYaml>,
    /// TLS configuration for Object Cache (optional, independent from cluster.tls)
    pub tls: Option<FlameTlsYaml>,
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
    pub slot: ResourceRequirement,
    pub policy: String,
    pub storage: String,
    /// Schedule interval in milliseconds for the session scheduler loop
    pub schedule_interval: u64,
    /// Executors configuration
    pub executors: FlameExecutors,
    /// TLS configuration for Session Manager
    pub tls: Option<FlameTls>,
}

#[derive(Debug, Clone, Default)]
pub struct FlameExecutors {
    pub shim: Shim,
    pub limits: FlameExecutorLimits,
}

#[derive(Debug, Clone)]
pub struct FlameExecutorLimits {
    pub max_executors: u32,
}

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
    /// Path to PEM-encoded CA private key (for signing session certificates)
    pub ca_key_file: Option<String>,
    /// Default validity for session certificates
    pub cert_validity: std::time::Duration,
}

impl FlameTls {
    /// Load server TLS config for tonic.
    /// When ca_file is configured, mTLS is enabled (client certificates required).
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
        let mut config = ServerTlsConfig::new().identity(identity);

        // Enable mTLS when CA file is configured - requires client certificates
        if let Some(ca_file) = &self.ca_file {
            let ca = fs::read_to_string(ca_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read ca_file <{}>: {}", ca_file, e))
            })?;
            config = config.client_ca_root(Certificate::from_pem(ca));
            tracing::info!("mTLS enabled: client certificates will be required");
        }

        Ok(config)
    }

    /// Load client TLS config for tonic with optional mTLS.
    /// If ca_file is specified, use it for server verification.
    /// If cert_file and key_file are specified, use them for client authentication.
    pub fn client_tls_config(&self) -> Result<ClientTlsConfig, FlameError> {
        let mut config = ClientTlsConfig::new();

        if let Some(ca_file) = &self.ca_file {
            let ca = fs::read_to_string(ca_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read ca_file <{}>: {}", ca_file, e))
            })?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }

        if !self.cert_file.is_empty() && !self.key_file.is_empty() {
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
            config = config.identity(Identity::from_pem(cert, key));
        }

        Ok(config)
    }

    pub fn can_sign_certificates(&self) -> bool {
        self.ca_key_file.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FlameCache {
    pub endpoint: String,
    pub network_interface: String,
    pub storage: Option<String>,
    pub eviction: FlameEviction,
    /// TLS configuration for Object Cache (optional, independent from cluster.tls)
    pub tls: Option<FlameTls>,
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
    pub fn from_file(fp: Option<String>) -> Result<Self, FlameError> {
        let fp = match fp {
            None => {
                format!("{}/.flame/{}", env!("HOME", "."), DEFAULT_FLAME_CONF)
            }
            Some(path) => path,
        };

        if !Path::new(&fp).is_file() {
            return Err(FlameError::InvalidConfig(format!("<{fp}> is not a file")));
        }

        let contents =
            fs::read_to_string(fp.clone()).map_err(|e| FlameError::Internal(e.to_string()))?;
        let ctx: FlameClusterContextYaml =
            serde_yaml::from_str(&contents).map_err(|e| FlameError::Internal(e.to_string()))?;

        tracing::debug!("Load FlameClusterContext from <{fp}>: {ctx:?}");

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

        Ok(FlameCluster {
            name: cluster.name,
            endpoint: cluster.endpoint,
            slot: ResourceRequirement::from(&cluster.slot.unwrap_or(DEFAULT_SLOT.to_string())),
            policy: cluster.policy.unwrap_or(DEFAULT_POLICY.to_string()),
            storage: cluster.storage.unwrap_or(DEFAULT_STORAGE.to_string()),
            schedule_interval: cluster
                .schedule_interval
                .unwrap_or(DEFAULT_SCHEDULE_INTERVAL),
            executors,
            tls,
        })
    }
}

impl TryFrom<FlameExecutorsYaml> for FlameExecutors {
    type Error = FlameError;
    fn try_from(executors: FlameExecutorsYaml) -> Result<Self, Self::Error> {
        Ok(FlameExecutors {
            shim: Shim::try_from(executors.shim.unwrap_or(DEFAULT_SHIM.to_string()))?,
            limits: executors
                .limits
                .map(FlameExecutorLimits::try_from)
                .unwrap_or_else(|| Ok(FlameExecutorLimits::default()))?,
        })
    }
}

impl TryFrom<FlameExecutorLimitsYaml> for FlameExecutorLimits {
    type Error = FlameError;
    fn try_from(limits: FlameExecutorLimitsYaml) -> Result<Self, Self::Error> {
        Ok(FlameExecutorLimits {
            max_executors: limits
                .max_executors
                .unwrap_or(DEFAULT_MAX_EXECUTORS_PER_NODE),
        })
    }
}

impl Default for FlameExecutorLimits {
    fn default() -> Self {
        FlameExecutorLimits {
            max_executors: DEFAULT_MAX_EXECUTORS_PER_NODE,
        }
    }
}

impl Default for FlameCluster {
    fn default() -> Self {
        FlameCluster {
            name: DEFAULT_CONTEXT_NAME.to_string(),
            endpoint: DEFAULT_FLAME_ENDPOINT.to_string(),
            slot: ResourceRequirement::from(&DEFAULT_SLOT.to_string()),
            policy: DEFAULT_POLICY.to_string(),
            storage: DEFAULT_STORAGE.to_string(),
            schedule_interval: DEFAULT_SCHEDULE_INTERVAL,
            executors: FlameExecutors::default(),
            tls: None,
        }
    }
}

/// Parse a duration string (e.g., "24h", "7d", "30m") into std::time::Duration
fn parse_duration(s: &str) -> Result<std::time::Duration, FlameError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(FlameError::InvalidConfig(
            "empty duration string".to_string(),
        ));
    }

    // Find where the numeric part ends and the unit begins
    let unit_start = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(unit_start);

    let value: u64 = num_part
        .parse()
        .map_err(|_| FlameError::InvalidConfig(format!("invalid duration number: {}", s)))?;

    let seconds = match unit_part.to_lowercase().as_str() {
        "s" | "sec" | "second" | "seconds" => value,
        "m" | "min" | "minute" | "minutes" => value * 60,
        "h" | "hr" | "hour" | "hours" => value * 60 * 60,
        "d" | "day" | "days" => value * 60 * 60 * 24,
        "" => value, // Default to seconds if no unit
        _ => {
            return Err(FlameError::InvalidConfig(format!(
                "invalid duration unit: {}",
                unit_part
            )))
        }
    };

    Ok(std::time::Duration::from_secs(seconds))
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

        // Parse cert_validity duration, default to 24 hours
        let cert_validity = yaml
            .cert_validity
            .map(|s| parse_duration(&s))
            .transpose()?
            .unwrap_or_else(|| std::time::Duration::from_secs(24 * 60 * 60));

        // Note: File existence is validated when loading certificates in server_tls_config()
        // and client_tls_config() methods, which provide more descriptive error messages.

        Ok(FlameTls {
            cert_file,
            key_file,
            ca_file: yaml.ca_file,
            ca_key_file: yaml.ca_key_file,
            cert_validity,
        })
    }
}

impl TryFrom<FlameCacheYaml> for FlameCache {
    type Error = FlameError;
    fn try_from(cache: FlameCacheYaml) -> Result<Self, Self::Error> {
        let tls = cache.tls.map(FlameTls::try_from).transpose()?;

        Ok(FlameCache {
            endpoint: cache
                .endpoint
                .unwrap_or(DEFAULT_FLAME_CACHE_ENDPOINT.to_string()),
            network_interface: cache
                .network_interface
                .unwrap_or(DEFAULT_FLAME_CACHE_NETWORK_INTERFACE.to_string()),
            storage: cache.storage,
            eviction: cache
                .eviction
                .map(FlameEviction::try_from)
                .transpose()?
                .unwrap_or_default(),
            tls,
        })
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

    #[test]
    fn test_flame_context_from_file() -> Result<(), FlameError> {
        // New config structure: executors is under cluster
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  slot: "cpu=1,mem=1g"
  policy: priority
  storage: sqlite://flame.db
  executors:
    shim: host
    limits:
      max_executors: 10
        "#;

        let tmp_dir = TempDir::new().unwrap();
        let tmp_file = tmp_dir.path().join("flame-cluster.yaml");

        fs::write(&tmp_file, context_string).map_err(|e| FlameError::Internal(e.to_string()))?;

        let ctx = FlameClusterContext::from_file(Some(tmp_file.to_string_lossy().to_string()))
            .map_err(|e| FlameError::Internal(e.to_string()))?;
        assert_eq!(ctx.cluster.name, "flame");
        assert_eq!(ctx.cluster.endpoint, "http://flame-session-manager:8080");
        assert_eq!(ctx.cluster.slot, ResourceRequirement::from("cpu=1,mem=1g"));
        assert_eq!(ctx.cluster.policy, "priority");
        assert_eq!(ctx.cluster.storage, "sqlite://flame.db");
        assert_eq!(ctx.cluster.executors.shim, Shim::Host);
        assert_eq!(ctx.cluster.executors.limits.max_executors, 10);

        Ok(())
    }

    #[test]
    fn test_flame_context_with_cache_eviction() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shim: host
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "eth0"
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
        assert_eq!(cache.network_interface, "eth0");
        assert_eq!(cache.storage, Some("/var/lib/flame/cache".to_string()));
        assert_eq!(cache.eviction.policy, "lru");
        assert_eq!(cache.eviction.max_memory, 512 * 1024 * 1024); // 512MB in bytes
        assert_eq!(cache.eviction.max_objects, Some(5000));

        Ok(())
    }

    #[test]
    fn test_flame_context_with_cache_no_eviction() -> Result<(), FlameError> {
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shim: host
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
    shim: host
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

        Ok(())
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
    shim: host
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
    shim: host
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
    shim: host
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
    shim: host
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

    #[test]
    fn test_flame_context_with_invalid_max_memory_number() {
        // Test invalid number should fail
        let context_string = r#"---
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  executors:
    shim: host
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

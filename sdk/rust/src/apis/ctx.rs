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

use serde_derive::{Deserialize, Serialize};
use std::env;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;
use tonic::transport::{Certificate, ClientTlsConfig, Identity};

use crate::apis::FlameError;

const DEFAULT_FLAME_CONF: &str = "flame.yaml";
const FLAME_ENDPOINT: &str = "FLAME_ENDPOINT";
const FLAME_CACHE_ENDPOINT: &str = "FLAME_CACHE_ENDPOINT";
const FLAME_CA_FILE: &str = "FLAME_CA_FILE";
const FLAME_CERT_FILE: &str = "FLAME_CERT_FILE";
const FLAME_KEY_FILE: &str = "FLAME_KEY_FILE";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameClientTls {
    #[serde(default)]
    pub ca_file: Option<String>,
    #[serde(default)]
    pub cert_file: Option<String>,
    #[serde(default)]
    pub key_file: Option<String>,
}

impl FlameClientTls {
    pub fn client_tls_config(&self, domain: &str) -> Result<ClientTlsConfig, FlameError> {
        let mut config = ClientTlsConfig::new().domain_name(domain);

        if let Some(ref ca_file) = self.ca_file {
            let ca = fs::read_to_string(ca_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read ca_file <{}>: {}", ca_file, e))
            })?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }

        if let (Some(cert_file), Some(key_file)) = (&self.cert_file, &self.key_file) {
            let cert = fs::read_to_string(cert_file).map_err(|e| {
                FlameError::InvalidConfig(format!(
                    "failed to read cert_file <{}>: {}",
                    cert_file, e
                ))
            })?;
            let key = fs::read_to_string(key_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read key_file <{}>: {}", key_file, e))
            })?;
            config = config.identity(Identity::from_pem(cert, key));
        }

        Ok(config)
    }

    pub fn from_env() -> Self {
        Self {
            ca_file: env::var(FLAME_CA_FILE).ok(),
            cert_file: env::var(FLAME_CERT_FILE).ok(),
            key_file: env::var(FLAME_KEY_FILE).ok(),
        }
    }

    pub fn has_mtls_credentials(&self) -> bool {
        self.cert_file.is_some() && self.key_file.is_some()
    }
}

/// Cluster configuration within a context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameClusterConfig {
    /// Cluster endpoint URL (e.g., "https://flame-session-manager:8080")
    pub endpoint: String,
    /// TLS configuration for cluster connection (optional)
    #[serde(default)]
    pub tls: Option<FlameClientTls>,
}

impl FlameClusterConfig {
    /// Check if cluster endpoint requires TLS (https:// scheme)
    pub fn requires_tls(&self) -> bool {
        self.endpoint.starts_with("https://")
    }
}

/// Cache configuration within a context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameClientCache {
    /// Cache endpoint URL (e.g., "grpcs://flame-object-cache:9090")
    #[serde(default)]
    pub endpoint: Option<String>,
    /// TLS configuration for cache connection (optional)
    #[serde(default)]
    pub tls: Option<FlameClientTls>,
    /// Local storage path for cache (optional)
    #[serde(default)]
    pub storage: Option<String>,
}

/// Package configuration for application deployment.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlamePackage {
    /// Storage URL for the package (e.g., "file:///var/lib/flame/packages")
    #[serde(default)]
    pub storage: Option<String>,
    /// Patterns to exclude from the package
    #[serde(default)]
    pub excludes: Vec<String>,
}

/// Runner configuration for application execution.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameRunner {
    /// Runner template name
    #[serde(default)]
    pub template: Option<String>,
}

/// A named context containing cluster, cache, and package configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlameContextEntry {
    /// Name of this context
    pub name: String,
    /// Cluster configuration
    pub cluster: FlameClusterConfig,
    /// Cache configuration (optional)
    #[serde(default)]
    pub cache: Option<FlameClientCache>,
    /// Package configuration (optional)
    #[serde(default)]
    pub package: Option<FlamePackage>,
    /// Runner configuration (optional)
    #[serde(default)]
    pub runner: Option<FlameRunner>,
}

/// Root configuration structure for flame.yaml
///
/// Example configuration:
/// ```yaml
/// current-context: flame
/// contexts:
///   - name: flame
///     cluster:
///       endpoint: "https://flame-session-manager:8080"
///       tls:
///         ca_file: "/etc/flame/certs/ca.crt"
///     cache:
///       endpoint: "grpcs://flame-object-cache:9090"
///       tls:
///         ca_file: "/etc/flame/certs/cache-ca.crt"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameContext {
    #[serde(rename = "current-context")]
    pub current_context: String,
    pub contexts: Vec<FlameContextEntry>,
    #[serde(skip)]
    pub workspace: Option<String>,
}

impl FlameContext {
    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn get_workspace(&self) -> &str {
        self.workspace.as_deref().unwrap_or("default")
    }

    pub fn get_current_context(&self) -> Result<&FlameContextEntry, FlameError> {
        self.contexts
            .iter()
            .find(|c| c.name == self.current_context)
            .ok_or(FlameError::InvalidConfig(format!(
                "Context <{}> not found",
                self.current_context
            )))
    }

    /// Create a FlameContext from environment variables.
    ///
    /// This is useful for instances running inside executors where the
    /// executor manager passes configuration via environment variables.
    ///
    /// Supported environment variables:
    /// - FLAME_ENDPOINT: Cluster endpoint URL
    /// - FLAME_CACHE_ENDPOINT: Cache endpoint URL  
    /// - FLAME_CA_FILE: CA certificate file path for TLS
    pub fn from_env() -> Result<Self, FlameError> {
        let endpoint = env::var(FLAME_ENDPOINT).map_err(|_| {
            FlameError::InvalidConfig(format!("{} environment variable not set", FLAME_ENDPOINT))
        })?;

        let ca_file = env::var(FLAME_CA_FILE).ok();
        let cert_file = env::var(FLAME_CERT_FILE).ok();
        let key_file = env::var(FLAME_KEY_FILE).ok();
        let tls = ca_file.map(|f| FlameClientTls {
            ca_file: Some(f),
            cert_file: cert_file.clone(),
            key_file: key_file.clone(),
        });

        let cache_endpoint = env::var(FLAME_CACHE_ENDPOINT).ok();
        let cache = cache_endpoint.map(|ep| FlameClientCache {
            endpoint: Some(ep),
            tls: tls.clone(),
            storage: None,
        });

        let ctx = FlameContextEntry {
            name: "env".to_string(),
            cluster: FlameClusterConfig { endpoint, tls },
            cache,
            package: None,
            runner: None,
        };

        Ok(FlameContext {
            current_context: "env".to_string(),
            contexts: vec![ctx],
            workspace: None,
        })
    }

    /// Load FlameContext from file, then apply environment variable overrides.
    ///
    /// Environment variables take precedence over file configuration:
    /// - FLAME_ENDPOINT: Overrides cluster endpoint
    /// - FLAME_CACHE_ENDPOINT: Overrides cache endpoint
    /// - FLAME_CA_FILE: Sets CA file if not already configured
    pub fn from_file_with_env(fp: Option<String>) -> Result<Self, FlameError> {
        let mut ctx = Self::from_file(fp)?;
        ctx.apply_env_overrides();
        Ok(ctx)
    }

    /// Apply environment variable overrides to the current context.
    fn apply_env_overrides(&mut self) {
        if let Ok(current) = self.get_current_context_mut() {
            // Override endpoint if FLAME_ENDPOINT is set
            if let Ok(endpoint) = env::var(FLAME_ENDPOINT) {
                current.cluster.endpoint = endpoint;
            }

            // Override/set CA file if FLAME_CA_FILE is set
            if let Ok(ca_file) = env::var(FLAME_CA_FILE) {
                // Set for cluster TLS
                if current.cluster.tls.is_none() {
                    current.cluster.tls = Some(FlameClientTls {
                        ca_file: Some(ca_file.clone()),
                        cert_file: None,
                        key_file: None,
                    });
                } else if let Some(ref mut tls) = current.cluster.tls {
                    if tls.ca_file.is_none() {
                        tls.ca_file = Some(ca_file.clone());
                    }
                }

                // Set for cache TLS
                if let Some(ref mut cache) = current.cache {
                    if cache.tls.is_none() {
                        cache.tls = Some(FlameClientTls {
                            ca_file: Some(ca_file.clone()),
                            cert_file: None,
                            key_file: None,
                        });
                    } else if let Some(ref mut tls) = cache.tls {
                        if tls.ca_file.is_none() {
                            tls.ca_file = Some(ca_file);
                        }
                    }
                }
            }

            // Override cache endpoint if FLAME_CACHE_ENDPOINT is set
            if let Ok(cache_endpoint) = env::var(FLAME_CACHE_ENDPOINT) {
                if let Some(ref mut cache) = current.cache {
                    cache.endpoint = Some(cache_endpoint);
                } else {
                    current.cache = Some(FlameClientCache {
                        endpoint: Some(cache_endpoint),
                        tls: current.cluster.tls.clone(),
                        storage: None,
                    });
                }
            }
        }
    }

    /// Get mutable reference to the current context entry.
    fn get_current_context_mut(&mut self) -> Result<&mut FlameContextEntry, FlameError> {
        let current = self.current_context.clone();
        self.contexts
            .iter_mut()
            .find(|c| c.name == current)
            .ok_or(FlameError::InvalidConfig(format!(
                "Context <{}> not found",
                current
            )))
    }

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
        let ctx: FlameContext =
            serde_yaml::from_str(&contents).map_err(|e| FlameError::Internal(e.to_string()))?;

        tracing::debug!("Load FlameContext from <{fp}>: {ctx}");

        Ok(ctx)
    }
}

impl Display for FlameContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "current_context: {}, contexts: {}",
            self.current_context,
            self.contexts.len()
        )
    }
}

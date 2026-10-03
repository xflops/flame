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
use std::path::{Path, PathBuf};
use tonic::transport::{Certificate, ClientTlsConfig, Identity};

use crate::apis::FlameError;

const DEFAULT_FLAME_CONF: &str = "flame.yaml";
const FLAME_ENDPOINT: &str = "FLAME_ENDPOINT";
const FLAME_CACHE_ENDPOINT: &str = "FLAME_CACHE_ENDPOINT";
const FLAME_CA_FILE: &str = "FLAME_CA_FILE";
const FLAME_CERT_FILE: &str = "FLAME_CERT_FILE";
const FLAME_KEY_FILE: &str = "FLAME_KEY_FILE";
const FLAME_WORKSPACE: &str = "FLAME_WORKSPACE";

fn default_workspace() -> String {
    "default".to_string()
}

/// Client TLS configuration for connecting to Flame services.
///
/// Note: To disable TLS for development, use http:// instead of https://
/// in the endpoint URL.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameClientTls {
    /// Path to CA certificate for server verification
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Path to the PEM client certificate chain sent to the TLS server.
    #[serde(default)]
    pub cert_file: Option<String>,
    /// Path to the PEM private key for cert_file.
    #[serde(default)]
    pub key_file: Option<String>,
}

impl FlameClientTls {
    /// Load client TLS config for tonic.
    ///
    /// If ca_file is specified, use it; otherwise use system CA bundle.
    /// The domain parameter is used for server name verification.
    pub fn client_tls_config(&self, domain: &str) -> Result<ClientTlsConfig, FlameError> {
        if self.cert_file.is_some() != self.key_file.is_some() {
            return Err(FlameError::InvalidConfig(
                "client TLS requires both cert_file and key_file".to_string(),
            ));
        }
        let mut config = ClientTlsConfig::new()
            .domain_name(domain)
            .with_native_roots();

        if let Some(ref ca_file) = self.ca_file {
            let ca = fs::read_to_string(ca_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read ca_file <{}>: {}", ca_file, e))
            })?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }

        if let (Some(cert_file), Some(key_file)) = (&self.cert_file, &self.key_file) {
            let cert = fs::read(cert_file).map_err(|e| {
                FlameError::InvalidConfig(format!(
                    "failed to read cert_file <{}>: {}",
                    cert_file, e
                ))
            })?;
            let key = fs::read(key_file).map_err(|e| {
                FlameError::InvalidConfig(format!("failed to read key_file <{}>: {}", key_file, e))
            })?;
            config = config.identity(Identity::from_pem(cert, key));
        }

        Ok(config)
    }
}

fn merge_tls(target: &mut Option<FlameClientTls>, source: &FlameClientTls) {
    let tls = target.get_or_insert_with(FlameClientTls::default);
    if tls.ca_file.is_none() {
        tls.ca_file.clone_from(&source.ca_file);
    }
    if tls.cert_file.is_none() {
        tls.cert_file.clone_from(&source.cert_file);
    }
    if tls.key_file.is_none() {
        tls.key_file.clone_from(&source.key_file);
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
}

/// A named context containing cluster, cache, and package configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlameContextEntry {
    /// Name of this context
    pub name: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
    /// Cluster configuration
    pub cluster: FlameClusterConfig,
    /// Cache configuration (optional)
    #[serde(default)]
    pub cache: Option<FlameClientCache>,
    /// Package configuration (optional)
    #[serde(default)]
    pub package: Option<FlamePackage>,
    /// App template name (optional)
    #[serde(default)]
    pub app: Option<String>,
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
///         cert_file: "/etc/flame/certs/client.crt"
///         key_file: "/etc/flame/certs/client.key"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameContext {
    #[serde(rename = "current-context")]
    pub current_context: String,
    pub contexts: Vec<FlameContextEntry>,
}

impl FlameContext {
    /// Get the current context entry.
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
    /// - FLAME_CERT_FILE: Client certificate chain path for TLS
    /// - FLAME_KEY_FILE: Client private key path for TLS
    pub fn from_env() -> Result<Self, FlameError> {
        let endpoint = env::var(FLAME_ENDPOINT).map_err(|_| {
            FlameError::InvalidConfig(format!("{} environment variable not set", FLAME_ENDPOINT))
        })?;

        let ca_file = env::var(FLAME_CA_FILE).ok();
        let cert_file = env::var(FLAME_CERT_FILE).ok();
        let key_file = env::var(FLAME_KEY_FILE).ok();
        let tls = if ca_file.is_some() || cert_file.is_some() || key_file.is_some() {
            Some(FlameClientTls {
                ca_file,
                cert_file,
                key_file,
            })
        } else {
            None
        };

        let cache_endpoint = env::var(FLAME_CACHE_ENDPOINT).ok();
        let cache = cache_endpoint.map(|ep| FlameClientCache {
            endpoint: Some(ep),
            tls: tls.clone(),
            storage: None,
        });

        let ctx = FlameContextEntry {
            name: "env".to_string(),
            workspace: env::var(FLAME_WORKSPACE).unwrap_or_else(|_| default_workspace()),
            cluster: FlameClusterConfig { endpoint, tls },
            cache,
            package: None,
            app: None,
        };

        Ok(FlameContext {
            current_context: "env".to_string(),
            contexts: vec![ctx],
        })
    }

    /// Load FlameContext from file, then apply environment variable overrides.
    ///
    /// Environment variables take precedence over file configuration:
    /// - FLAME_ENDPOINT: Overrides cluster endpoint
    /// - FLAME_CACHE_ENDPOINT: Overrides cache endpoint
    /// - FLAME_CA_FILE: Sets CA file if not already configured
    /// - FLAME_CERT_FILE: Sets client certificate if not already configured
    /// - FLAME_KEY_FILE: Sets client private key if not already configured
    pub fn from_file_with_env(fp: Option<String>) -> Result<Self, FlameError> {
        let mut ctx = Self::from_file(fp)?;
        ctx.apply_env_overrides();
        Ok(ctx)
    }

    /// Apply environment variable overrides to the current context.
    fn apply_env_overrides(&mut self) {
        if let Ok(current) = self.get_current_context_mut() {
            if let Ok(workspace) = env::var(FLAME_WORKSPACE) {
                current.workspace = workspace;
            }
            // Override endpoint if FLAME_ENDPOINT is set
            if let Ok(endpoint) = env::var(FLAME_ENDPOINT) {
                current.cluster.endpoint = endpoint;
            }

            let ca_file = env::var(FLAME_CA_FILE).ok();
            let cert_file = env::var(FLAME_CERT_FILE).ok();
            let key_file = env::var(FLAME_KEY_FILE).ok();
            let env_tls = FlameClientTls {
                ca_file,
                cert_file,
                key_file,
            };
            if env_tls.ca_file.is_some()
                || env_tls.cert_file.is_some()
                || env_tls.key_file.is_some()
            {
                merge_tls(&mut current.cluster.tls, &env_tls);
                if let Some(ref mut cache) = current.cache {
                    merge_tls(&mut cache.tls, &env_tls);
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

    pub fn set_workspace(&mut self, workspace: impl Into<String>) -> Result<(), FlameError> {
        self.get_current_context_mut()?.workspace = workspace.into();
        Ok(())
    }

    pub fn from_file(fp: Option<String>) -> Result<Self, FlameError> {
        let fp = match fp {
            None => env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".flame")
                .join(DEFAULT_FLAME_CONF)
                .to_string_lossy()
                .into_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_identity_requires_both_files() {
        for tls in [
            FlameClientTls {
                cert_file: Some("client.crt".into()),
                ..Default::default()
            },
            FlameClientTls {
                key_file: Some("client.key".into()),
                ..Default::default()
            },
        ] {
            let error = tls.client_tls_config("gateway.example.com").unwrap_err();
            assert!(matches!(error, FlameError::InvalidConfig(_)));
            assert!(error.to_string().contains("both cert_file and key_file"));
        }
    }

    #[test]
    fn client_identity_loads_pem_files() {
        let cert_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ci/docker/certs");
        let tls = FlameClientTls {
            ca_file: Some(cert_dir.join("ca.crt").to_string_lossy().into_owned()),
            cert_file: Some(cert_dir.join("server.crt").to_string_lossy().into_owned()),
            key_file: Some(cert_dir.join("server.key").to_string_lossy().into_owned()),
        };
        tls.client_tls_config("gateway.example.com").unwrap();
    }

    #[test]
    fn client_identity_yaml_fields_round_trip() {
        let tls: FlameClientTls =
            serde_yaml::from_str("ca_file: ca.crt\ncert_file: client.crt\nkey_file: client.key\n")
                .unwrap();
        assert_eq!(tls.ca_file.as_deref(), Some("ca.crt"));
        assert_eq!(tls.cert_file.as_deref(), Some("client.crt"));
        assert_eq!(tls.key_file.as_deref(), Some("client.key"));
    }
}

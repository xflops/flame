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

use base64::{engine::general_purpose::STANDARD, Engine};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
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

/// Client TLS configuration for connecting to Flame services.
///
/// Note: To disable TLS for development, use http:// instead of https://
/// in the endpoint URL.
#[derive(Clone, Serialize, Deserialize, Default)]
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
    /// Standard-base64-encoded PEM CA certificates for server verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_data: Option<String>,
    /// Standard-base64-encoded PEM client certificate chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_data: Option<String>,
    /// Standard-base64-encoded PEM private key matching the client certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_data: Option<String>,
}

impl std::fmt::Debug for FlameClientTls {
    /// Keep inline credentials out of configuration diagnostics and logs.
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlameClientTls")
            .field("ca_file", &self.ca_file)
            .field("cert_file", &self.cert_file)
            .field("key_file", &self.key_file)
            .field("ca_data", &self.ca_data.as_ref().map(|_| "<redacted>"))
            .field("cert_data", &self.cert_data.as_ref().map(|_| "<redacted>"))
            .field("key_data", &self.key_data.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl FlameClientTls {
    /// Load client TLS config for tonic.
    ///
    /// Credentials may come from files or standard-base64-encoded PEM data.
    /// Each credential accepts only one source; client authentication requires
    /// both a certificate and its matching private key. Inline data stays in memory.
    /// The domain parameter is used for server name verification.
    pub fn client_tls_config(&self, domain: &str) -> Result<ClientTlsConfig, FlameError> {
        stdng::tls::install_crypto_provider();
        for (name, file, data) in [
            ("ca", &self.ca_file, &self.ca_data),
            ("cert", &self.cert_file, &self.cert_data),
            ("key", &self.key_file, &self.key_data),
        ] {
            validate_source(name, file.as_deref(), data.as_deref())?;
        }
        let has_cert = self.cert_file.is_some() || self.cert_data.is_some();
        let has_key = self.key_file.is_some() || self.key_data.is_some();
        if has_cert != has_key {
            return Err(FlameError::InvalidConfig(
                "client TLS requires both a certificate (cert_file or cert_data) and a private key (key_file or key_data)".to_string(),
            ));
        }
        let mut config = ClientTlsConfig::new()
            .domain_name(domain)
            .with_native_roots();

        if let Some(ca) = load_credential("ca", self.ca_file.as_deref(), self.ca_data.as_deref())? {
            validate_certificates(&ca, "ca_file or ca_data")?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }

        if let (Some(cert), Some(key)) = (
            load_credential("cert", self.cert_file.as_deref(), self.cert_data.as_deref())?,
            load_credential("key", self.key_file.as_deref(), self.key_data.as_deref())?,
        ) {
            let certs = validate_certificates(&cert, "cert_file or cert_data")?;
            std::str::from_utf8(&key).map_err(|_| invalid_key())?;
            let keys = PrivateKeyDer::pem_slice_iter(&key)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| invalid_key())?;
            let mut keys = keys.into_iter();
            let private_key = keys.next().ok_or_else(invalid_key)?;
            if keys.next().is_some() {
                return Err(invalid_key());
            }
            rustls::sign::CertifiedKey::from_der(
                certs,
                private_key,
                &rustls::crypto::ring::default_provider(),
            )
            .map_err(|_| {
                FlameError::InvalidConfig(
                    "client TLS certificate and private key must form a valid matching identity"
                        .to_string(),
                )
            })?;
            config = config.identity(Identity::from_pem(cert, key));
        }

        Ok(config)
    }
}

/// Reject ambiguous and empty credential sources without displaying their values.
fn validate_source(name: &str, file: Option<&str>, data: Option<&str>) -> Result<(), FlameError> {
    if file.is_some() && data.is_some() {
        return Err(FlameError::InvalidConfig(format!(
            "{name}_file and {name}_data cannot both be configured"
        )));
    }
    if file == Some("") || data == Some("") {
        return Err(FlameError::InvalidConfig(format!(
            "{name}_file and {name}_data must not be empty when configured"
        )));
    }
    Ok(())
}

/// Read or decode one credential; errors name the source field, never its contents.
fn load_credential(
    name: &str,
    file: Option<&str>,
    data: Option<&str>,
) -> Result<Option<Vec<u8>>, FlameError> {
    if let Some(data) = data {
        let invalid = || {
            FlameError::InvalidConfig(format!(
                "{name}_data must contain canonical standard base64-encoded PEM"
            ))
        };
        let decoded = STANDARD.decode(data).map_err(|_| invalid())?;
        if STANDARD.encode(&decoded) != data {
            return Err(invalid());
        }
        return Ok(Some(decoded));
    }
    file.map(|path| {
        fs::read(path).map_err(|error| {
            FlameError::InvalidConfig(format!("failed to read {name}_file: {error}"))
        })
    })
    .transpose()
}

/// Parse and validate every X.509 certificate in a PEM certificate bundle.
fn validate_certificates(
    pem: &[u8],
    field: &str,
) -> Result<Vec<CertificateDer<'static>>, FlameError> {
    let invalid =
        || FlameError::InvalidConfig(format!("{field} must contain valid PEM certificates"));
    std::str::from_utf8(pem).map_err(|_| invalid())?;
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    if certificates.is_empty() {
        return Err(invalid());
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in &certificates {
        roots.add(cert.clone()).map_err(|_| invalid())?;
    }
    Ok(certificates)
}

/// Keep private-key parser details out of user-visible errors.
fn invalid_key() -> FlameError {
    FlameError::InvalidConfig(
        "key_file or key_data must contain exactly one valid, unencrypted PEM private key"
            .to_string(),
    )
}

fn merge_tls(target: &mut Option<FlameClientTls>, source: &FlameClientTls) {
    let tls = target.get_or_insert_with(FlameClientTls::default);
    if tls.ca_file.is_none() && tls.ca_data.is_none() {
        tls.ca_file.clone_from(&source.ca_file);
        tls.ca_data.clone_from(&source.ca_data);
    }
    if tls.cert_file.is_none() && tls.cert_data.is_none() {
        tls.cert_file.clone_from(&source.cert_file);
        tls.cert_data.clone_from(&source.cert_data);
    }
    if tls.key_file.is_none() && tls.key_data.is_none() {
        tls.key_file.clone_from(&source.key_file);
        tls.key_data.clone_from(&source.key_data);
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
                ..Default::default()
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

    /// The context of a client: `from_file(fp)` with the environment applied (`with_env`). When there is no
    /// file, `FLAME_ENDPOINT` alone names the cluster (`from_env`), which is how an application running in an
    /// executor gets its cluster; with neither, the error is `from_file`'s: no default is assumed.
    pub(crate) fn load(fp: Option<String>) -> Result<Self, FlameError> {
        let path = Self::config_path(fp);
        if !Path::new(&path).is_file() && env::var_os(FLAME_ENDPOINT).is_some() {
            return Self::from_env();
        }
        Ok(Self::from_file(Some(path))?.with_env())
    }

    fn config_path(fp: Option<String>) -> String {
        match fp {
            None => env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".flame")
                .join(DEFAULT_FLAME_CONF)
                .to_string_lossy()
                .into_owned(),
            Some(path) => path,
        }
    }

    /// Apply environment variable overrides to the current context; use it as `from_file(fp)?.with_env()`.
    ///
    /// Endpoint variables override the file. TLS file variables fill credentials
    /// only when neither a file nor inline data is configured:
    /// - FLAME_ENDPOINT: Overrides cluster endpoint
    /// - FLAME_CACHE_ENDPOINT: Overrides cache endpoint
    /// - FLAME_CA_FILE: Sets CA file if not already configured
    /// - FLAME_CERT_FILE: Sets client certificate if not already configured
    /// - FLAME_KEY_FILE: Sets client private key if not already configured
    pub fn with_env(mut self) -> Self {
        if let Ok(current) = self.get_current_context_mut() {
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
                ..Default::default()
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
        self
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
        let fp = Self::config_path(fp);

        if !Path::new(&fp).is_file() {
            return Err(FlameError::InvalidConfig(missing_config_message(&fp)));
        }

        let contents =
            fs::read_to_string(fp.clone()).map_err(|e| FlameError::Internal(e.to_string()))?;
        let ctx: FlameContext = serde_yaml::from_str(&contents).map_err(|error| {
            // Serde errors can quote an invalid input value, including a secret.
            let location = error
                .location()
                .map(|location| {
                    format!(" at line {}, column {}", location.line(), location.column())
                })
                .unwrap_or_default();
            FlameError::InvalidConfig(format!(
                "<{fp}> is not a valid Flame configuration{location}"
            ))
        })?;

        tracing::debug!("Load FlameContext from <{fp}>: {ctx}");

        Ok(ctx)
    }
}

/// The error for a cluster that is configured nowhere: Flame never guesses an endpoint.
fn missing_config_message(path: &str) -> String {
    format!(
        "no Flame cluster configured: <{path}> does not exist.\n\
         Create it:\n  current-context: flame\n  contexts:\n    - name: flame\n      cluster:\n        \
         endpoint: \"http://<head>:8080\"\n      cache:\n        endpoint: \"grpc://<worker>:9090\""
    )
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
mod tests;

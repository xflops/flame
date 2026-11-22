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

//! Transport routing is separate from the canonical image/cache identity.

use std::fs;
use std::time::Duration;

use flame_rs::apis::FlameError;
use http::uri::Scheme;
use oci_client::client::{Certificate, CertificateEncoding, Client, ClientConfig, ClientProtocol};
use oci_client::secrets::RegistryAuth;
use oci_client::Reference;

use super::store::linux_resolver;
use crate::config::FlameRegistryMirror;

pub(super) struct PullSource {
    /// Canonical reference used for cache metadata.
    pub reference: Reference,
    /// Registry and repository used for this transport attempt.
    pub remote: Reference,
    pub client: Client,
    pub auth: RegistryAuth,
    pub endpoint: String,
    secrets: Vec<String>,
}

pub(super) fn client_config(protocol: ClientProtocol) -> ClientConfig {
    ClientConfig {
        protocol,
        platform_resolver: Some(Box::new(linux_resolver)),
        connect_timeout: Some(Duration::from_secs(30)),
        read_timeout: Some(Duration::from_secs(120)),
        ..ClientConfig::default()
    }
}

impl PullSource {
    pub fn upstream(reference: &Reference, client: &Client) -> Self {
        Self {
            reference: reference.clone(),
            remote: reference.clone(),
            client: client.clone(),
            auth: RegistryAuth::Anonymous,
            endpoint: reference.resolve_registry().to_string(),
            secrets: Vec::new(),
        }
    }

    pub fn mirror(reference: &Reference, mirror: &FlameRegistryMirror) -> Result<Self, FlameError> {
        let url = url::Url::parse(&mirror.endpoint)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid mirror URL: {e}")))?;
        let mut endpoint = url.clone();
        let _ = endpoint.set_username("");
        let _ = endpoint.set_password(None);
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        let endpoint = endpoint.to_string();
        let invalid =
            |why: String| FlameError::InvalidConfig(format!("registry mirror {endpoint}: {why}"));
        let protocol = match url.scheme() {
            scheme if scheme == Scheme::HTTP.as_str() => ClientProtocol::Http,
            scheme if scheme == Scheme::HTTPS.as_str() => ClientProtocol::Https,
            _ => return Err(invalid("unsupported registry scheme".into())),
        };
        // These components cannot be represented by an OCI registry reference.
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "expected an HTTP(S) registry endpoint without URL credentials, query or fragment"
                    .into(),
            ));
        }
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        let prefix = url.path().trim_matches('/');
        let repository = if prefix.is_empty() {
            reference.repository().to_string()
        } else {
            format!("{prefix}/{}", reference.repository())
        };
        let remote = match reference.digest() {
            Some(digest) => {
                Reference::with_digest(authority.to_string(), repository, digest.to_string())
            }
            None => Reference::with_tag(
                authority.to_string(),
                repository,
                reference.tag().unwrap_or("latest").to_string(),
            ),
        };
        let mut config = client_config(protocol);
        if let Some(path) = &mirror.ca_file {
            let data =
                fs::read(path).map_err(|e| invalid(format!("read CA {}: {e}", path.display())))?;
            config.extra_root_certificates.push(Certificate {
                encoding: CertificateEncoding::Pem,
                data,
            });
        }
        let client =
            Client::try_from(config).map_err(|e| invalid(format!("create TLS client: {e}")))?;
        let mut secrets = Vec::new();
        let auth = match (&mirror.username_env, &mirror.password_env) {
            (Some(user), Some(password)) => {
                let get = |key: &str| {
                    std::env::var(key)
                        .ok()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| {
                            invalid(format!("environment variable {key} is missing or empty"))
                        })
                };
                let username = get(user)?;
                let password = get(password)?;
                secrets.extend([username.clone(), password.clone()]);
                secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
                RegistryAuth::Basic(username, password)
            }
            (None, None) => RegistryAuth::Anonymous,
            _ => return Err(invalid("set both username_env and password_env".into())),
        };
        Ok(Self {
            reference: reference.clone(),
            remote,
            client,
            auth,
            endpoint,
            secrets,
        })
    }

    pub fn error(&self, error: &FlameError) -> String {
        let mut message = format!("{}: {error}", self.endpoint);
        for secret in &self.secrets {
            message = message.replace(secret, "[REDACTED]");
        }
        message
    }
}

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

//! Node-local OCI image store.
//!
//! An image is resolved against a registry v2 API (anonymous bearer auth),
//! pulled by manifest digest with every blob verified, unpacked once into a
//! flattened root filesystem and published atomically under
//! `<state>/images/sha256/<hex>`. Concurrent pulls of one digest run once.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use flame_rs::apis::FlameError;
use futures_util::StreamExt;
use oci_spec::image::{Arch, Descriptor, ImageConfiguration, ImageIndex, ImageManifest, Os};
use reqwest::StatusCode;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::{Allowlist, Config, ImageRef};
use crate::unpack::unpack_layers;

const IMAGE_JSON: &str = "image.json";
const ROOTFS_DIR: &str = "rootfs";
const TMP_PREFIX: &str = ".tmp-";
const STALE_TMP_AGE: Duration = Duration::from_secs(3600);
const MAX_MANIFEST_BYTES: u64 = 4 << 20;
const MAX_CONFIG_BYTES: u64 = 4 << 20;
const MAX_LAYERS: usize = 256;
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// Runtime configuration of an image, from the OCI image config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    /// Environment, `KEY=value`.
    #[serde(default)]
    pub env: Vec<String>,
    /// Default command.
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Entrypoint.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// Working directory.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// User, `uid[:gid]` or a name.
    #[serde(default)]
    pub user: Option<String>,
}

/// An unpacked image in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRoot {
    /// Digest of the platform manifest, `sha256:<hex>`.
    pub digest: String,
    /// Flattened root filesystem; treat as read-only.
    pub rootfs: PathBuf,
    /// Image configuration.
    pub config: ImageConfig,
}

#[derive(Serialize, Deserialize)]
struct ImageMeta {
    digest: String,
    config: ImageConfig,
}

/// A registry manifest document: an index (OCI) or manifest list (Docker
/// schema 2, the same shape), or an image manifest (OCI, or Docker schema 2).
/// The `mediaType` strings are not interpreted: `oci-spec` keeps unknown ones
/// as `MediaType::Other`, so Docker documents parse the same way.
#[derive(Deserialize)]
#[serde(untagged)]
enum Manifest {
    Index(Box<ImageIndex>),
    Image(Box<ImageManifest>),
}

struct Resolved {
    digest: String,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

fn internal(msg: impl Into<String>) -> FlameError {
    FlameError::Internal(msg.into())
}

/// Returns the hex part of a strict `sha256:<64 lowercase hex>` digest.
fn sha256_hex(digest: &str) -> Result<&str, FlameError> {
    digest
        .strip_prefix("sha256:")
        .filter(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| {
            internal(format!(
                "unsupported digest '{digest}' in registry response"
            ))
        })
}

fn pick_manifest<'a>(list: &'a [Descriptor], arch: &Arch) -> Result<&'a Descriptor, FlameError> {
    list.iter()
        .find(|d| {
            d.platform()
                .as_ref()
                .is_some_and(|p| *p.os() == Os::Linux && p.architecture() == arch)
        })
        .ok_or_else(|| internal(format!("image has no linux/{arch} manifest")))
}

/// Parses `Bearer realm="...",service="...",scope="..."`.
fn parse_bearer_challenge(header: &str) -> Option<HashMap<String, String>> {
    let (scheme, params) = header.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut map = HashMap::new();
    let mut chars = params.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=') {
            key.push(c);
        }
        if chars.next() != Some('=') {
            break;
        }
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '"' => break,
                    _ => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                value.push(c);
            }
        }
        map.insert(key.trim().to_ascii_lowercase(), value);
    }
    Some(map)
}

/// Removes `.tmp-*` directories in `dir` that are at least `min_age` old.
fn sweep_tmp(dir: &Path, min_age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= min_age);
        if stale {
            if let Err(e) = fs::remove_dir_all(entry.path()) {
                tracing::warn!("failed to remove stale {}: {e}", entry.path().display());
            }
        }
    }
}

fn build_client(direct: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .user_agent("flmbox");
    if direct {
        builder = builder.no_proxy();
    }
    builder.build().unwrap_or_default()
}

/// Pulls, unpacks and caches OCI images.
pub struct ImageStore {
    images_dir: PathBuf,
    allowlist: Allowlist,
    max_bytes: u64,
    client: reqwest::Client,
    /// Used for loopback registries, bypassing any configured proxy.
    direct: reqwest::Client,
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ImageStore {
    /// Creates a store under `<state_dir>/images` and sweeps stale `.tmp-*`
    /// directories (older than one hour, so a concurrent pull is not hurt).
    pub fn new(config: &Config) -> Self {
        let images_dir = config.state_dir.join("images");
        sweep_tmp(&images_dir, STALE_TMP_AGE);
        Self {
            images_dir,
            allowlist: config.images.clone(),
            max_bytes: config.max_image_bytes,
            client: build_client(false),
            direct: build_client(true),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Makes sure the image is in the cache and returns it. The reference is
    /// checked against the allowlist before any network I/O. A tag costs one
    /// manifest request to resolve the digest; a cached digest costs none.
    pub async fn ensure_image(&self, reference: &str) -> Result<ImageRoot, FlameError> {
        let image = ImageRef::parse(reference)?;
        self.allowlist.check(&image)?;

        if let Some(digest) = &image.digest {
            if let Some(root) = self.cached(digest)? {
                return Ok(root);
            }
        }

        let client = if image.is_loopback() {
            &self.direct
        } else {
            &self.client
        };
        let mut registry = Registry {
            client: client.clone(),
            base: image.base_url(),
            repository: image.repository.clone(),
            token: None,
        };
        let resolved = registry.resolve(&image).await?;

        let lock = self.lock_for(&resolved.digest);
        let _guard = lock.lock().await;
        if let Some(root) = self.cached(&resolved.digest)? {
            return Ok(root);
        }
        self.pull(&mut registry, resolved).await
    }

    fn lock_for(&self, digest: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        map.entry(digest.to_string()).or_default().clone()
    }

    fn final_dir(&self, digest: &str) -> Result<PathBuf, FlameError> {
        Ok(self.images_dir.join("sha256").join(sha256_hex(digest)?))
    }

    fn cached(&self, digest: &str) -> Result<Option<ImageRoot>, FlameError> {
        let dir = self.final_dir(digest)?;
        let json = match fs::read(dir.join(IMAGE_JSON)) {
            Ok(json) => json,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(internal(format!("read {}: {e}", dir.display()))),
        };
        let meta: ImageMeta = serde_json::from_slice(&json)
            .map_err(|e| internal(format!("corrupt {}: {e}", dir.join(IMAGE_JSON).display())))?;
        Ok(Some(ImageRoot {
            digest: meta.digest,
            rootfs: dir.join(ROOTFS_DIR),
            config: meta.config,
        }))
    }

    async fn pull(
        &self,
        registry: &mut Registry,
        resolved: Resolved,
    ) -> Result<ImageRoot, FlameError> {
        let total = resolved
            .layers
            .iter()
            .chain([&resolved.config])
            .fold(0u64, |acc, d| acc.saturating_add(d.size()));
        if total > self.max_bytes {
            return Err(FlameError::InvalidConfig(format!(
                "image is {total} bytes, over the {} byte limit",
                self.max_bytes
            )));
        }

        fs::create_dir_all(&self.images_dir)
            .map_err(|e| internal(format!("create {}: {e}", self.images_dir.display())))?;
        // Removed on every error path when dropped.
        let tmp = tempfile::Builder::new()
            .prefix(TMP_PREFIX)
            .tempdir_in(&self.images_dir)
            .map_err(|e| internal(format!("create temp dir: {e}")))?;
        let blobs = tmp.path().join("blobs");
        let rootfs = tmp.path().join(ROOTFS_DIR);
        for dir in [&blobs, &rootfs] {
            fs::create_dir(dir).map_err(|e| internal(format!("create {}: {e}", dir.display())))?;
        }

        let config_path = blobs.join("config");
        registry
            .download_blob(&resolved.config, &config_path)
            .await?;
        let config = parse_image_config(&config_path, &resolved.config)?;

        let mut layer_files = Vec::new();
        for (i, layer) in resolved.layers.iter().enumerate() {
            let path = blobs.join(format!("layer-{i}"));
            registry.download_blob(layer, &path).await?;
            layer_files.push(path);
        }

        let (root, max) = (rootfs.clone(), self.max_bytes);
        tokio::task::spawn_blocking(move || unpack_layers(&root, &layer_files, max))
            .await
            .map_err(|e| internal(format!("unpack task failed: {e}")))??;

        let meta = ImageMeta {
            digest: resolved.digest.clone(),
            config,
        };
        let json = serde_json::to_vec_pretty(&meta).map_err(|e| internal(e.to_string()))?;
        let json_path = tmp.path().join(IMAGE_JSON);
        fs::write(&json_path, json)
            .map_err(|e| internal(format!("write {}: {e}", json_path.display())))?;
        fs::remove_dir_all(&blobs)
            .map_err(|e| internal(format!("remove {}: {e}", blobs.display())))?;

        let final_dir = self.final_dir(&resolved.digest)?;
        if let Some(parent) = final_dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| internal(format!("create {}: {e}", parent.display())))?;
        }
        if let Err(e) = fs::rename(tmp.path(), &final_dir) {
            // Another process may have published the same digest.
            if final_dir.join(IMAGE_JSON).exists() {
                tracing::debug!("image {} was published concurrently", resolved.digest);
            } else {
                return Err(internal(format!("publish {}: {e}", final_dir.display())));
            }
        }

        tracing::info!("pulled image {}", resolved.digest);
        self.cached(&resolved.digest)?
            .ok_or_else(|| internal(format!("image {} missing after publish", resolved.digest)))
    }
}

fn parse_image_config(path: &Path, desc: &Descriptor) -> Result<ImageConfig, FlameError> {
    if desc.size() > MAX_CONFIG_BYTES {
        return Err(internal("image config is too large"));
    }
    let bytes = fs::read(path).map_err(|e| internal(format!("read image config: {e}")))?;
    let doc: ImageConfiguration =
        serde_json::from_slice(&bytes).map_err(|e| internal(format!("parse image config: {e}")))?;
    let non_empty = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    Ok(match doc.config() {
        Some(c) => ImageConfig {
            env: c.env().clone().unwrap_or_default(),
            cmd: c.cmd().clone().unwrap_or_default(),
            entrypoint: c.entrypoint().clone().unwrap_or_default(),
            working_dir: non_empty(c.working_dir()),
            user: non_empty(c.user()),
        },
        None => ImageConfig::default(),
    })
}

/// A registry v2 client for one repository, with anonymous bearer auth.
struct Registry {
    client: reqwest::Client,
    base: String,
    repository: String,
    token: Option<String>,
}

impl Registry {
    async fn send(&self, url: &str, accept: Option<&str>) -> Result<reqwest::Response, FlameError> {
        let mut request = self.client.get(url);
        if let Some(accept) = accept {
            request = request.header(reqwest::header::ACCEPT, accept);
        }
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
            .send()
            .await
            .map_err(|e| FlameError::Network(format!("registry request {url}: {e}")))
    }

    async fn get(
        &mut self,
        url: &str,
        accept: Option<&str>,
    ) -> Result<reqwest::Response, FlameError> {
        let mut response = self.send(url, accept).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| {
                    FlameError::Network(format!("{url}: authentication required, no challenge"))
                })?;
            self.token = Some(self.fetch_token(&challenge).await?);
            response = self.send(url, accept).await?;
        }
        match response.status() {
            s if s.is_success() => Ok(response),
            StatusCode::NOT_FOUND => Err(FlameError::NotFound(url.to_string())),
            s => Err(FlameError::Network(format!(
                "registry returned {s} for {url}"
            ))),
        }
    }

    async fn fetch_token(&self, challenge: &str) -> Result<String, FlameError> {
        let params = parse_bearer_challenge(challenge).ok_or_else(|| {
            FlameError::Network(format!("unsupported registry authentication '{challenge}'"))
        })?;
        let realm = params
            .get("realm")
            .ok_or_else(|| FlameError::Network("bearer challenge has no realm".to_string()))?;
        if self.base.starts_with("https://") && !realm.starts_with("https://") {
            return Err(FlameError::Network(format!(
                "refusing token realm '{realm}' over insecure transport"
            )));
        }
        let scope = params
            .get("scope")
            .cloned()
            .unwrap_or_else(|| format!("repository:{}:pull", self.repository));
        let mut query = vec![("scope", scope)];
        if let Some(service) = params.get("service") {
            query.push(("service", service.clone()));
        }
        let response = self
            .client
            .get(realm)
            .query(&query)
            .send()
            .await
            .map_err(|e| FlameError::Network(format!("token request {realm}: {e}")))?;
        if !response.status().is_success() {
            return Err(FlameError::Network(format!(
                "token endpoint {realm} returned {}",
                response.status()
            )));
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| FlameError::Network(format!("read token response: {e}")))?;
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let token: Token = serde_json::from_slice(&body)
            .map_err(|e| FlameError::Network(format!("parse token response: {e}")))?;
        token
            .token
            .or(token.access_token)
            .ok_or_else(|| FlameError::Network("token response has no token".to_string()))
    }

    /// Fetches a manifest; a digest reference is verified against the bytes.
    async fn fetch_manifest(&mut self, reference: &str) -> Result<(Vec<u8>, String), FlameError> {
        let url = format!("{}/v2/{}/manifests/{reference}", self.base, self.repository);
        let response = self.get(&url, Some(MANIFEST_ACCEPT)).await?;
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| FlameError::Network(format!("read {url}: {e}")))?;
            if (body.len() + chunk.len()) as u64 > MAX_MANIFEST_BYTES {
                return Err(internal(format!("manifest {url} is too large")));
            }
            body.extend_from_slice(&chunk);
        }
        let digest = format!("sha256:{:x}", Sha256::digest(&body));
        if reference.starts_with("sha256:") && digest != reference {
            return Err(internal(format!(
                "manifest digest mismatch: requested {reference}, got {digest}"
            )));
        }
        Ok((body, digest))
    }

    async fn resolve(&mut self, image: &ImageRef) -> Result<Resolved, FlameError> {
        let reference = image
            .digest
            .as_deref()
            .or(image.tag.as_deref())
            .unwrap_or("latest");
        let (mut body, mut digest) = self.fetch_manifest(reference).await?;
        let mut doc = parse_manifest(&body)?;
        if let Manifest::Index(index) = &doc {
            let chosen = pick_manifest(index.manifests(), &Arch::default())?
                .digest()
                .to_string();
            sha256_hex(&chosen)?;
            (body, digest) = self.fetch_manifest(&chosen).await?;
            doc = parse_manifest(&body)?;
        }
        let Manifest::Image(manifest) = doc else {
            return Err(internal("nested image index is not supported"));
        };
        let (config, layers) = (manifest.config().clone(), manifest.layers().clone());
        if layers.len() > MAX_LAYERS {
            return Err(internal(format!(
                "image has too many layers ({})",
                layers.len()
            )));
        }
        for d in layers.iter().chain([&config]) {
            sha256_hex(d.digest().as_ref())?;
        }
        Ok(Resolved {
            digest,
            config,
            layers,
        })
    }

    /// Streams a blob to `dest`, verifying its size and SHA-256.
    async fn download_blob(&mut self, desc: &Descriptor, dest: &Path) -> Result<(), FlameError> {
        let digest = desc.digest().to_string();
        let expected = sha256_hex(&digest)?.to_string();
        let url = format!("{}/v2/{}/blobs/{digest}", self.base, self.repository);
        let response = self.get(&url, None).await?;
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| internal(format!("create {}: {e}", dest.display())))?;
        let mut hasher = Sha256::new();
        let mut read = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| FlameError::Network(format!("read blob {digest}: {e}")))?;
            read += chunk.len() as u64;
            if read > desc.size() {
                return Err(internal(format!(
                    "blob {digest} is larger than its declared {} bytes",
                    desc.size()
                )));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| internal(format!("write {}: {e}", dest.display())))?;
        }
        file.flush()
            .await
            .map_err(|e| internal(format!("write {}: {e}", dest.display())))?;
        if read != desc.size() {
            return Err(internal(format!(
                "blob {} is {read} bytes, declared {}",
                digest,
                desc.size()
            )));
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected {
            return Err(internal(format!(
                "blob digest mismatch: expected sha256:{expected}, got sha256:{actual}"
            )));
        }
        Ok(())
    }
}

fn parse_manifest(body: &[u8]) -> Result<Manifest, FlameError> {
    serde_json::from_slice(body).map_err(|e| internal(format!("parse manifest: {e}")))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use oci_spec::image::{DescriptorBuilder, Digest, MediaType, PlatformBuilder};

    use super::*;

    fn desc(platform: Option<(&str, &str)>, fill: char) -> Descriptor {
        let digest = format!("sha256:{}", fill.to_string().repeat(64));
        let mut builder = DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .digest(Digest::from_str(&digest).unwrap())
            .size(0u64);
        if let Some((os, arch)) = platform {
            builder = builder.platform(
                PlatformBuilder::default()
                    .os(Os::from(os))
                    .architecture(Arch::from(arch))
                    .build()
                    .unwrap(),
            );
        }
        builder.build().unwrap()
    }

    #[test]
    fn picks_linux_manifest_for_arch() {
        let list = [
            desc(Some(("windows", "amd64")), 'b'),
            desc(Some(("linux", "arm64")), 'a'),
            desc(Some(("linux", "amd64")), 'c'),
            desc(None, 'd'),
        ];
        let pick = |arch: Arch| pick_manifest(&list, &arch).map(|d| d.digest().to_string());
        assert!(pick(Arch::Amd64).unwrap().ends_with("cc"));
        assert!(pick(Arch::ARM64).unwrap().ends_with("aa"));
        assert!(pick(Arch::RISCV64).is_err());
    }

    #[test]
    fn docker_schema2_documents_parse_like_oci() {
        let hex = "a".repeat(64);
        let list = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests":[{{"mediaType":"application/vnd.docker.distribution.manifest.v2+json","size":528,
            "digest":"sha256:{hex}","platform":{{"architecture":"arm64","os":"linux","variant":"v8"}}}},
            {{"mediaType":"application/vnd.docker.distribution.manifest.v2+json","size":528,
            "digest":"sha256:{hex}","platform":{{"architecture":"unknown","os":"unknown"}}}}]}}"#
        );
        let Manifest::Index(index) = parse_manifest(list.as_bytes()).unwrap() else {
            panic!("expected an index");
        };
        assert_eq!(index.manifests().len(), 2);
        assert!(pick_manifest(index.manifests(), &Arch::ARM64).is_ok());

        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json",
            "config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":1469,"digest":"sha256:{hex}"}},
            "layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":2,"digest":"sha256:{hex}"}}]}}"#
        );
        let Manifest::Image(image) = parse_manifest(manifest.as_bytes()).unwrap() else {
            panic!("expected an image manifest");
        };
        assert_eq!(image.layers().len(), 1);
        assert_eq!(image.config().size(), 1469);
        assert!(parse_manifest(br#"{"schemaVersion":1,"fsLayers":[]}"#).is_err());
    }

    #[test]
    fn parses_bearer_challenge() {
        let p = parse_bearer_challenge(
            r#"Bearer realm="https://auth.io/token",service="registry.io",scope="repository:a/b:pull,push""#,
        )
        .unwrap();
        assert_eq!(p["realm"], "https://auth.io/token");
        assert_eq!(p["service"], "registry.io");
        assert_eq!(p["scope"], "repository:a/b:pull,push");
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn digest_format_is_strict() {
        assert!(sha256_hex(&format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(sha256_hex("sha256:../../etc").is_err());
        assert!(sha256_hex(&format!("sha512:{}", "a".repeat(64))).is_err());
    }

    #[test]
    fn sweeps_stale_tmp_dirs_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".tmp-a/rootfs")).unwrap();
        fs::create_dir_all(dir.path().join("sha256")).unwrap();
        sweep_tmp(dir.path(), Duration::from_secs(3600));
        assert!(dir.path().join(".tmp-a").exists());
        sweep_tmp(dir.path(), Duration::ZERO);
        assert!(!dir.path().join(".tmp-a").exists());
        assert!(dir.path().join("sha256").exists());
    }
}

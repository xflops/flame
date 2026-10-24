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

//! A node-local, content-addressed cache of unpacked OCI images.
//!
//! The registry protocol (authentication, manifests, index platform choice,
//! digest verification and blob transfer) is `oci-client`'s. This module adds
//! what it does not do: the size and layer caps, the hardened unpack into one
//! flattened read-only rootfs, and the cache keyed by the platform manifest
//! digest with a singleflight per digest.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use flame_rs::apis::FlameError;
use nix::fcntl::{Flock, FlockArg};
use oci_client::client::{Client, ClientConfig, ClientProtocol};
use oci_client::config::ConfigFile;
use oci_client::config::{Architecture, Os};
use oci_client::manifest::{ImageIndexEntry, OciDescriptor, OciImageManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::Reference;
use serde_derive::{Deserialize, Serialize};
use tokio::io::AsyncWrite;

use super::progress::{PullEvent, PullPrinter};
use super::unpack::unpack_layers;
use crate::config::FlameSandbox;

const IMAGE_JSON: &str = "image.json";
const ROOTFS_DIR: &str = "rootfs";
const TMP_PREFIX: &str = ".tmp-";
const STALE_TMP_AGE: Duration = Duration::from_secs(3600);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const MAX_LAYERS: usize = 256;
const MAX_CONFIG_BYTES: u64 = 4 << 20;

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
    /// The reference the image was first pulled by.
    #[serde(default)]
    reference: Option<String>,
    /// When the image entered the cache, seconds since the Unix epoch.
    #[serde(default)]
    created: Option<u64>,
}

fn unix_secs(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// An image in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// Digest of the platform manifest, `sha256:<hex>`.
    pub digest: String,
    /// The reference the image was first pulled by, when recorded.
    pub reference: Option<String>,
    /// Size of the unpacked root filesystem in bytes.
    pub size: u64,
    /// When the image entered the cache.
    pub created: Option<std::time::SystemTime>,
}

fn internal(msg: impl Into<String>) -> FlameError {
    FlameError::Internal(msg.into())
}

fn registry_error(what: &str, err: &dyn std::error::Error) -> FlameError {
    FlameError::Network(format!("{what}: {}", chain(err)))
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

/// The `linux/<host arch>` entry of an image index.
fn linux_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().is_some_and(|platform| {
                platform.os == Os::Linux && platform.architecture == Architecture::default()
            })
        })
        .map(|entry| entry.digest.clone())
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

/// Bytes of the regular files under `dir`; symlinks are not followed.
fn tree_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m)))
        .map(|(path, m)| {
            if m.is_dir() {
                tree_size(&path)
            } else if m.is_file() {
                m.len()
            } else {
                0
            }
        })
        .sum()
}

/// Counts what is written to a blob file, stops at the declared size and reports progress.
struct BlobWriter<F: Fn(u64) + Send + Unpin> {
    file: tokio::fs::File,
    written: u64,
    limit: u64,
    last: Instant,
    report: F,
}

impl<F: Fn(u64) + Send + Unpin> AsyncWrite for BlobWriter<F> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.written + buf.len() as u64 > this.limit {
            return Poll::Ready(Err(io::Error::other(format!(
                "blob is larger than its declared {} bytes",
                this.limit
            ))));
        }
        let polled = Pin::new(&mut this.file).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            this.written += *n as u64;
            if this.last.elapsed() >= PROGRESS_INTERVAL {
                this.last = Instant::now();
                (this.report)(this.written);
            }
        }
        polled
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_shutdown(cx)
    }
}

/// Pulls, unpacks and caches OCI images.
pub struct ImageStore {
    images_dir: PathBuf,
    max_bytes: u64,
    client: Client,
    progress: Option<PullPrinter>,
}

impl ImageStore {
    /// Creates a store under `<state_dir>/images` and sweeps stale `.tmp-*`
    /// directories (older than one hour, so a concurrent pull is not hurt).
    pub fn new(config: &FlameSandbox) -> Self {
        Self::with_protocol(config, ClientProtocol::Https)
    }

    /// Like [`new`](Self::new) with another transport, so tests can use an
    /// in-process registry over plain HTTP.
    pub(super) fn with_protocol(config: &FlameSandbox, protocol: ClientProtocol) -> Self {
        let images_dir = config.state_dir.join("images");
        sweep_tmp(&images_dir, STALE_TMP_AGE);
        // The registry client needs a TLS provider and does not pick one. `ring`
        // is the one the workspace uses; `aws-lc-rs` must stay out of the build,
        // since rustls cannot choose when both are enabled. Already installed is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::new(ClientConfig {
            protocol,
            platform_resolver: Some(Box::new(linux_resolver)),
            connect_timeout: Some(Duration::from_secs(30)),
            read_timeout: Some(Duration::from_secs(120)),
            ..ClientConfig::default()
        });
        Self {
            images_dir,
            max_bytes: config.images.max_bytes,
            client,
            progress: None,
        }
    }

    /// Prints the steps of every pull to stderr.
    pub fn with_progress(mut self) -> Self {
        self.progress = Some(PullPrinter::new());
        self
    }

    fn emit(&self, event: PullEvent) {
        if let Some(progress) = &self.progress {
            progress.report(event);
        }
    }

    /// Makes sure the image is in the cache and returns it. A tag costs the
    /// manifest requests to resolve the digest; a cached digest reference costs none.
    pub async fn pull_image(&self, reference: &str) -> Result<ImageRoot, FlameError> {
        let reference = parse_reference(reference)?;
        if let Some(digest) = reference.digest() {
            if let Some(root) = self.cached(digest)? {
                self.record_reference(digest, &reference);
                self.emit(PullEvent::Cached {
                    digest: digest.to_string(),
                });
                return Ok(root);
            }
        }

        let (manifest, digest) = self
            .client
            .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
            .await
            .map_err(|e| registry_error(&format!("pull {reference}"), &e))?;
        sha256_hex(&digest)?;

        // Held until the end of the pull: other executors of the node wait here.
        let _lock = self.lock(&digest).await?;
        if let Some(root) = self.cached(&digest)? {
            self.record_reference(&digest, &reference);
            self.emit(PullEvent::Cached { digest });
            return Ok(root);
        }

        if manifest.layers.len() > MAX_LAYERS {
            return Err(internal(format!(
                "image has too many layers ({})",
                manifest.layers.len()
            )));
        }
        if manifest.config.size < 0 || manifest.config.size as u64 > MAX_CONFIG_BYTES {
            return Err(internal("image config has an invalid size"));
        }
        sha256_hex(&manifest.config.digest)?;
        let mut total = 0u64;
        for layer in &manifest.layers {
            sha256_hex(&layer.digest)?;
            let size = u64::try_from(layer.size)
                .map_err(|_| internal(format!("layer {} has a negative size", layer.digest)))?;
            total = total.saturating_add(size);
        }
        if total > self.max_bytes {
            return Err(FlameError::InvalidConfig(format!(
                "image is {total} bytes, over the {} byte limit",
                self.max_bytes
            )));
        }
        self.pull(&reference, digest, manifest, total).await
    }

    /// Records `reference` for a cached image that has none, for `list`. Best effort.
    fn record_reference(&self, digest: &str, reference: &Reference) {
        let Ok(path) = self.final_dir(digest).map(|d| d.join(IMAGE_JSON)) else {
            return;
        };
        let Ok(json) = fs::read(&path) else { return };
        let Ok(mut meta) = serde_json::from_slice::<ImageMeta>(&json) else {
            return;
        };
        if meta.reference.is_some() {
            return;
        }
        meta.reference = Some(reference.to_string());
        // Keep the age: the file's own time is the best record for an old entry.
        if meta.created.is_none() {
            meta.created = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(unix_secs);
        }
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_vec_pretty(&meta)
            .map_err(io::Error::other)
            .and_then(|json| fs::write(&tmp, json))
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = written {
            tracing::debug!("could not record the reference of {digest}: {e}");
        }
    }

    /// The images in the cache, newest first.
    pub fn list(&self) -> Result<Vec<ImageInfo>, FlameError> {
        let dir = self.images_dir.join("sha256");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(internal(format!("read {}: {e}", dir.display()))),
        };
        let mut images = Vec::new();
        for entry in entries.flatten() {
            let json_path = entry.path().join(IMAGE_JSON);
            let Ok(json) = fs::read(&json_path) else {
                continue; // Not published (yet).
            };
            let Ok(meta) = serde_json::from_slice::<ImageMeta>(&json) else {
                tracing::warn!("skipping corrupt {}", json_path.display());
                continue;
            };
            images.push(ImageInfo {
                digest: meta.digest,
                reference: meta.reference,
                size: tree_size(&entry.path().join(ROOTFS_DIR)),
                created: meta
                    .created
                    .map(|secs| std::time::UNIX_EPOCH + Duration::from_secs(secs))
                    .or_else(|| fs::metadata(&json_path).and_then(|m| m.modified()).ok()),
            });
        }
        images.sort_by(|a, b| b.created.cmp(&a.created).then(a.digest.cmp(&b.digest)));
        Ok(images)
    }

    /// Takes the exclusive lock of one image: an advisory `flock` on
    /// `<images>/locks/<hex>.lock`, so that the processes of several
    /// executors on a node pull an image once. It is released when the guard
    /// is dropped or the process dies. The lock files stay, empty.
    async fn lock(&self, digest: &str) -> Result<Flock<fs::File>, FlameError> {
        let dir = self.images_dir.join("locks");
        let path = dir.join(format!("{}.lock", sha256_hex(digest)?));
        tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&dir)
                .map_err(|e| internal(format!("create {}: {e}", dir.display())))?;
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .map_err(|e| internal(format!("open {}: {e}", path.display())))?;
            Flock::lock(file, FlockArg::LockExclusive)
                .map_err(|(_, e)| internal(format!("lock {}: {e}", path.display())))
        })
        .await
        .map_err(|e| internal(format!("lock task failed: {e}")))?
    }

    fn final_dir(&self, digest: &str) -> Result<PathBuf, FlameError> {
        Ok(self.images_dir.join("sha256").join(sha256_hex(digest)?))
    }

    fn cached(&self, digest: &str) -> Result<Option<ImageRoot>, FlameError> {
        let dir = self.final_dir(digest)?;
        let json = match fs::read(dir.join(IMAGE_JSON)) {
            Ok(json) => json,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
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
        reference: &Reference,
        digest: String,
        manifest: OciImageManifest,
        total: u64,
    ) -> Result<ImageRoot, FlameError> {
        let layers = manifest.layers;
        self.emit(PullEvent::Resolved {
            digest: digest.clone(),
            blobs: layers.len(),
            bytes: total,
        });

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
        self.download_blob(reference, &manifest.config, &config_path, None)
            .await?;
        let config_json = fs::read_to_string(&config_path)
            .map_err(|e| internal(format!("read image config: {e}")))?;
        let config = parse_image_config(&config_json)?;

        let mut layer_files = Vec::new();
        for (i, layer) in layers.iter().enumerate() {
            let path = blobs.join(format!("layer-{i}"));
            let name = format!("layer {}/{}", i + 1, layers.len());
            self.download_blob(reference, layer, &path, Some(&name))
                .await?;
            layer_files.push(path);
        }

        self.emit(PullEvent::Unpacking);
        let (root, max) = (rootfs.clone(), self.max_bytes);
        tokio::task::spawn_blocking(move || unpack_layers(&root, &layer_files, max))
            .await
            .map_err(|e| internal(format!("unpack task failed: {e}")))??;

        let meta = ImageMeta {
            digest: digest.clone(),
            config,
            reference: Some(reference.to_string()),
            created: unix_secs(std::time::SystemTime::now()),
        };
        let json = serde_json::to_vec_pretty(&meta).map_err(|e| internal(e.to_string()))?;
        let json_path = tmp.path().join(IMAGE_JSON);
        fs::write(&json_path, json)
            .map_err(|e| internal(format!("write {}: {e}", json_path.display())))?;
        fs::remove_dir_all(&blobs)
            .map_err(|e| internal(format!("remove {}: {e}", blobs.display())))?;

        let final_dir = self.final_dir(&digest)?;
        if let Some(parent) = final_dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| internal(format!("create {}: {e}", parent.display())))?;
        }
        if let Err(e) = fs::rename(tmp.path(), &final_dir) {
            // Another process may have published the same digest.
            if final_dir.join(IMAGE_JSON).exists() {
                tracing::debug!("image {digest} was published concurrently");
            } else {
                return Err(internal(format!("publish {}: {e}", final_dir.display())));
            }
        }

        self.emit(PullEvent::Pulled {
            digest: digest.clone(),
        });
        tracing::info!("pulled image {digest}");
        self.cached(&digest)?
            .ok_or_else(|| internal(format!("image {digest} missing after publish")))
    }

    /// Downloads one blob to `dest`; `oci-client` verifies its digest. A named
    /// blob reports its progress.
    async fn download_blob(
        &self,
        reference: &Reference,
        layer: &OciDescriptor,
        dest: &Path,
        name: Option<&str>,
    ) -> Result<(), FlameError> {
        let size = u64::try_from(layer.size).unwrap_or_default();
        let file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| internal(format!("create {}: {e}", dest.display())))?;
        let progress = self.progress.clone();
        let report = |done: u64, finished: bool, name: Option<&str>| {
            if let (Some(progress), Some(name)) = (&progress, name) {
                progress.report(PullEvent::Blob {
                    name: name.to_string(),
                    done,
                    size,
                    finished,
                });
            }
        };
        let live_name = name.map(str::to_string);
        let writer = BlobWriter {
            file,
            written: 0,
            limit: size,
            last: Instant::now(),
            report: |done| report(done, false, live_name.as_deref()),
        };
        self.client
            .pull_blob(reference, layer, writer)
            .await
            .map_err(|e| registry_error(&format!("download {}", layer.digest), &e))?;
        let written = fs::metadata(dest)
            .map_err(|e| internal(format!("stat {}: {e}", dest.display())))?
            .len();
        if written != size {
            return Err(internal(format!(
                "blob {} is {written} bytes, declared {size}",
                layer.digest
            )));
        }
        report(written, true, name);
        Ok(())
    }
}

fn parse_image_config(json: &str) -> Result<ImageConfig, FlameError> {
    let doc: ConfigFile =
        serde_json::from_str(json).map_err(|e| internal(format!("parse image config: {e}")))?;
    let non_empty = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    Ok(match &doc.config {
        Some(c) => ImageConfig {
            env: c.env.clone().unwrap_or_default(),
            cmd: c.cmd.clone().unwrap_or_default(),
            entrypoint: c.entrypoint.clone().unwrap_or_default(),
            working_dir: non_empty(&c.working_dir),
            user: non_empty(&c.user),
        },
        None => ImageConfig::default(),
    })
}

/// Parses an image reference (`ubuntu:26.04`, `ghcr.io/org/img:tag`,
/// `localhost:5000/img`, `name@sha256:<hex>`).
pub fn parse_reference(input: &str) -> Result<Reference, FlameError> {
    input
        .parse()
        .map_err(|e| FlameError::InvalidConfig(format!("invalid image reference '{input}': {e}")))
}

/// An error with its sources, as `a: b: c`; a top-level message alone often hides the cause.
fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use oci_client::manifest::{ImageIndexEntry, Platform};

    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use flame_rs::apis::FlameError;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use sha2::{Digest, Sha256};
    use tar::{EntryType, Header};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn entry(os: Os, arch: Architecture, digest: &str) -> ImageIndexEntry {
        ImageIndexEntry {
            media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
            digest: digest.to_string(),
            size: 1,
            platform: Some(Platform {
                architecture: arch,
                os,
                os_version: None,
                os_features: None,
                variant: None,
                features: None,
            }),
            annotations: None,
            artifact_type: None,
        }
    }

    #[test]
    fn picks_the_linux_manifest_of_the_host_arch() {
        let host = Architecture::default();
        let other = if host == Architecture::Amd64 {
            Architecture::ARM64
        } else {
            Architecture::Amd64
        };
        let list = [
            entry(Os::Windows, host.clone(), "sha256:win"),
            entry(Os::Linux, other, "sha256:other"),
            entry(Os::Linux, host, "sha256:want"),
        ];
        assert_eq!(linux_resolver(&list).as_deref(), Some("sha256:want"));
        assert_eq!(linux_resolver(&list[..2]), None);
    }

    #[test]
    fn docker_and_oci_image_configs_parse() {
        let json = r#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},
            "config":{"Env":["A=1"],"Cmd":["sh"],"Entrypoint":null,"WorkingDir":"","User":"1000:1000"}}"#;
        let cfg = parse_image_config(json).unwrap();
        assert_eq!(cfg.env, ["A=1"]);
        assert_eq!(cfg.cmd, ["sh"]);
        assert!(cfg.entrypoint.is_empty());
        assert_eq!(cfg.working_dir, None);
        assert_eq!(cfg.user.as_deref(), Some("1000:1000"));
        assert!(parse_image_config("nope").is_err());
    }

    #[test]
    fn references_are_normalized_and_validated() {
        let r = parse_reference("ubuntu:26.04").unwrap();
        assert_eq!(r.repository(), "library/ubuntu");
        assert_eq!(r.tag(), Some("26.04"));
        let digest = format!("name@sha256:{}", "a".repeat(64));
        assert!(parse_reference(&digest).unwrap().digest().is_some());
        for bad in ["", "not a ref!", "a@sha256:short"] {
            assert!(
                matches!(parse_reference(bad), Err(FlameError::InvalidConfig(_))),
                "{bad}"
            );
        }
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

    const REPO: &str = "test/img";
    const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

    // ---- mock registry -------------------------------------------------------

    struct State {
        routes: Mutex<HashMap<String, (String, Vec<u8>)>>,
        log: Mutex<Vec<String>>,
        delay: Duration,
    }

    struct Mock {
        addr: SocketAddr,
        state: Arc<State>,
    }

    impl Mock {
        async fn start(delay: Duration) -> Mock {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let state = Arc::new(State {
                routes: Mutex::new(HashMap::new()),
                log: Mutex::new(Vec::new()),
                delay,
            });
            let st = state.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(handle(stream, st.clone()));
                }
            });
            Mock { addr, state }
        }

        fn put(&self, path: &str, content_type: &str, body: Vec<u8>) {
            self.state
                .routes
                .lock()
                .unwrap()
                .insert(path.to_string(), (content_type.to_string(), body));
        }

        fn requests(&self, needle: &str) -> usize {
            self.state
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.contains(needle))
                .count()
        }

        fn total(&self) -> usize {
            self.state.log.lock().unwrap().len()
        }

        fn reference(&self, suffix: &str) -> String {
            format!("{}/{REPO}{suffix}", self.addr)
        }
    }

    async fn handle(mut stream: TcpStream, st: Arc<State>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        let path = text
            .lines()
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .unwrap_or("/")
            .to_string();
        st.log.lock().unwrap().push(path.clone());

        let (status, headers, body): (&str, String, Vec<u8>) = {
            let route = st.routes.lock().unwrap().get(&path).cloned();
            match route {
                Some((ct, body)) => {
                    if path.contains("/blobs/") && !st.delay.is_zero() {
                        tokio::time::sleep(st.delay).await;
                    }
                    ("200 OK", format!("Content-Type: {ct}\r\n"), body)
                }
                None => ("404 Not Found", String::new(), Vec::new()),
            }
        };
        let head = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(&body).await;
        let _ = stream.shutdown().await;
    }

    // ---- image building ------------------------------------------------------

    enum Ent<'a> {
        File(&'a str, &'a [u8], u32),
        Dir(&'a str, u32),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
    }

    /// Builds a gzip layer; names are written raw so unsafe paths can be tested.
    fn layer(entries: &[Ent]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for ent in entries {
            let (name, kind, mode, link, data): (&str, EntryType, u32, Option<&str>, &[u8]) =
                match ent {
                    Ent::File(n, d, m) => (n, EntryType::Regular, *m, None, d),
                    Ent::Dir(n, m) => (n, EntryType::Directory, *m, None, &[]),
                    Ent::Symlink(n, t) => (n, EntryType::Symlink, 0o777, Some(t), &[]),
                    Ent::Hardlink(n, t) => (n, EntryType::Link, 0o644, Some(t), &[]),
                };
            let mut h = Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(mode);
            h.set_uid(0);
            h.set_gid(0);
            h.set_size(data.len() as u64);
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            if let Some(l) = link {
                h.as_old_mut().linkname[..l.len()].copy_from_slice(l.as_bytes());
            }
            h.set_cksum();
            builder.append(&h, data).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn config_json() -> Vec<u8> {
        br#"{"architecture":"amd64","os":"linux","config":{"Env":["PATH=/usr/bin","A=b"],"Cmd":["/bin/sh"],"Entrypoint":null,"WorkingDir":"/work","User":"1000"},"rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec()
    }

    /// Registers config, layers and the manifest; returns the manifest digest.
    fn add_image(mock: &Mock, layers: &[Vec<u8>]) -> String {
        let config = config_json();
        let config_digest = digest(&config);
        mock.put(
            &format!("/v2/{REPO}/blobs/{config_digest}"),
            "application/octet-stream",
            config.clone(),
        );
        let mut descs = Vec::new();
        for l in layers {
            let d = digest(l);
            mock.put(
                &format!("/v2/{REPO}/blobs/{d}"),
                "application/octet-stream",
                l.clone(),
            );
            descs.push(format!(
                r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{d}","size":{}}}"#,
                l.len()
            ));
        }
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{}]}}"#,
            config.len(),
            descs.join(",")
        )
        .into_bytes();
        let d = digest(&manifest);
        mock.put(&format!("/v2/{REPO}/manifests/{d}"), OCI_MANIFEST, manifest);
        d
    }

    /// Registers an index under `tag` that selects `manifest` for the host arch.
    fn add_index(mock: &Mock, tag: &str, manifest: &str) {
        let host = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let other = if host == "amd64" { "arm64" } else { "amd64" };
        let missing = format!("sha256:{}", "e".repeat(64));
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[
            {{"mediaType":"{OCI_MANIFEST}","digest":"{missing}","size":1,"platform":{{"architecture":"{other}","os":"linux"}}}},
            {{"mediaType":"{OCI_MANIFEST}","digest":"{missing}","size":1,"platform":{{"architecture":"{host}","os":"windows"}}}},
            {{"mediaType":"{OCI_MANIFEST}","digest":"{manifest}","size":1,"platform":{{"architecture":"{host}","os":"linux"}}}}]}}"#
        );
        mock.put(
            &format!("/v2/{REPO}/manifests/{tag}"),
            OCI_INDEX,
            index.into_bytes(),
        );
    }

    fn base_layers() -> Vec<Vec<u8>> {
        let l1 = layer(&[
            Ent::Dir("etc/", 0o755),
            Ent::File("etc/os-release", b"ID=v1\n", 0o644),
            Ent::Dir("bin/", 0o755),
            Ent::File("bin/tool", b"#!tool\n", 0o755),
            Ent::File("bin/su", b"su", 0o4755),
            Ent::Symlink("bin/sh", "tool"),
            Ent::Symlink("abs", "/etc"),
            Ent::Dir("data/", 0o755),
            Ent::File("data/old.txt", b"old", 0o644),
            Ent::File("data/keep.txt", b"keep", 0o644),
            Ent::Dir("opt/", 0o755),
            Ent::File("opt/a", b"a", 0o644),
            Ent::File("opt/b", b"b", 0o644),
            Ent::Dir("ro/", 0o555),
            Ent::File("ro/f", b"f", 0o444),
        ]);
        let l2 = layer(&[
            Ent::File("./etc/os-release", b"ID=v2\n", 0o644),
            Ent::File("data/.wh.old.txt", b"", 0o644),
            // The new file precedes the opaque marker and must survive it.
            Ent::File("opt/c", b"c", 0o644),
            Ent::File("opt/.wh..wh..opq", b"", 0o644),
            // Through an absolute symlink: lands inside the root's /etc.
            Ent::File("abs/extra.conf", b"x", 0o600),
            Ent::Hardlink("bin/tool2", "bin/tool"),
        ]);
        vec![l1, l2]
    }

    /// An [`ImageStore`] that reaches the mock registry over `http://127.0.0.1`.
    fn store(state: &Path, max: Option<&str>) -> ImageStore {
        let mut vars = HashMap::new();
        vars.insert("FLMBOX_STATE_DIR".to_string(), state.display().to_string());
        if let Some(max) = max {
            vars.insert("FLMBOX_MAX_IMAGE_BYTES".to_string(), max.to_string());
        }
        ImageStore::with_protocol(
            &FlameSandbox::from_map(&vars).unwrap(),
            ClientProtocol::Http,
        )
    }

    fn assert_clean(state: &Path) {
        let images = state.join("images");
        if let Ok(rd) = std::fs::read_dir(&images) {
            for e in rd.flatten() {
                assert!(
                    !e.file_name().to_string_lossy().starts_with(".tmp-"),
                    "left {:?}",
                    e.path()
                );
            }
        }
        let published = std::fs::read_dir(images.join("sha256"))
            .map(|rd| rd.count())
            .unwrap_or(0);
        assert_eq!(published, 0, "nothing should be published");
    }

    fn mode(path: impl AsRef<Path>) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    // ---- tests ---------------------------------------------------------------

    #[tokio::test]
    async fn flattens_layers() {
        let mock = Mock::start(Duration::ZERO).await;
        let manifest = add_image(&mock, &base_layers());
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), None);

        let root = store.pull_image(&mock.reference(":v1")).await.unwrap();
        let fs = &root.rootfs;

        assert_eq!(root.digest, manifest);
        assert_eq!(
            fs.parent().unwrap(),
            state
                .path()
                .join("images/sha256")
                .join(manifest.trim_start_matches("sha256:"))
        );
        assert_eq!(
            std::fs::read(fs.join("etc/os-release")).unwrap(),
            b"ID=v2\n"
        );
        assert_eq!(std::fs::read(fs.join("etc/extra.conf")).unwrap(), b"x");
        assert_eq!(std::fs::read(fs.join("data/keep.txt")).unwrap(), b"keep");
        assert!(!fs.join("data/old.txt").exists());
        assert!(!fs.join("data/.wh.old.txt").exists());
        assert!(!fs.join("opt/a").exists() && !fs.join("opt/b").exists());
        assert_eq!(std::fs::read(fs.join("opt/c")).unwrap(), b"c");
        assert!(!fs.join("opt/.wh..wh..opq").exists());
        assert_eq!(
            std::fs::read_link(fs.join("bin/sh")).unwrap().to_str(),
            Some("tool")
        );
        assert_eq!(mode(fs.join("bin/tool")), 0o755);
        assert_eq!(mode(fs.join("bin/su")), 0o4755);
        assert_eq!(mode(fs.join("etc/extra.conf")), 0o600);
        assert_eq!(mode(fs.join("ro")), 0o555);
        assert_eq!(mode(fs.join("ro/f")), 0o444);
        assert_eq!(
            std::fs::metadata(fs.join("bin/tool")).unwrap().ino(),
            std::fs::metadata(fs.join("bin/tool2")).unwrap().ino()
        );

        assert_eq!(root.config.env, ["PATH=/usr/bin", "A=b"]);
        assert_eq!(root.config.cmd, ["/bin/sh"]);
        assert!(root.config.entrypoint.is_empty());
        assert_eq!(root.config.working_dir.as_deref(), Some("/work"));
        assert_eq!(root.config.user.as_deref(), Some("1000"));
        assert!(state
            .path()
            .join("images/sha256")
            .join(manifest.trim_start_matches("sha256:"))
            .join("image.json")
            .exists());
        let leftovers = std::fs::read_dir(state.path().join("images"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);

        // Make the read-only directories removable for the tempdir cleanup.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(fs.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn second_call_hits_the_cache() {
        let mock = Mock::start(Duration::ZERO).await;
        let manifest = add_image(&mock, &base_layers());
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), None);

        let first = store.pull_image(&mock.reference(":v1")).await.unwrap();
        let blobs = mock.requests("/blobs/");
        assert_eq!(blobs, 3);
        let manifests = mock.requests("/manifests/");

        // A tag costs the manifest requests (index and platform manifest) only, never a blob.
        let second = store.pull_image(&mock.reference(":v1")).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(mock.requests("/blobs/"), blobs);
        assert_eq!(mock.requests("/manifests/"), manifests + 2);

        // The reference is recorded for `list`, also on an image cached without one.
        let images = store.list().unwrap();
        assert_eq!(images.len(), 1);
        assert!(images[0].reference.as_deref().unwrap().contains(":v1"));

        // A cached digest costs nothing.
        let before = mock.total();
        let third = store
            .pull_image(&format!("{}@{manifest}", mock.reference("")))
            .await
            .unwrap();
        assert_eq!(first, third);
        assert_eq!(mock.total(), before);
    }

    #[tokio::test]
    async fn concurrent_calls_pull_once() {
        let mock = Mock::start(Duration::from_millis(50)).await;
        let manifest = add_image(&mock, &base_layers());
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), None);
        let r = mock.reference(":v1");

        let (a, b, c, d) = tokio::join!(
            store.pull_image(&r),
            store.pull_image(&r),
            store.pull_image(&r),
            store.pull_image(&r)
        );
        for result in [a, b, c, d] {
            assert_eq!(result.unwrap().digest, manifest);
        }
        assert_eq!(mock.requests("/blobs/"), 3);
    }

    #[tokio::test]
    async fn stores_sharing_a_state_dir_pull_once() {
        // Each executor of a node has its own process and so its own store;
        // two stores stand in for them (flock conflicts between open files too).
        let mock = Mock::start(Duration::from_millis(50)).await;
        let manifest = add_image(&mock, &base_layers());
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let (a, b) = (store(state.path(), None), store(state.path(), None));
        let r = mock.reference(":v1");

        let (x, y) = tokio::join!(a.pull_image(&r), b.pull_image(&r));
        assert_eq!(x.unwrap(), y.unwrap());
        assert_eq!(mock.requests("/blobs/"), 3);
        assert_eq!(a.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn size_cap_is_enforced_before_download() {
        let mock = Mock::start(Duration::ZERO).await;
        let manifest = add_image(&mock, &base_layers());
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), Some("100"));

        let err = store.pull_image(&mock.reference(":v1")).await.unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)), "{err}");
        assert_eq!(mock.requests("/blobs/"), 0);
        assert_clean(state.path());
    }

    #[tokio::test]
    async fn unpacked_size_cap_is_enforced() {
        let mock = Mock::start(Duration::ZERO).await;
        // Compresses to almost nothing but expands past the cap.
        let big = vec![0u8; 64 * 1024];
        let manifest = add_image(&mock, &[layer(&[Ent::File("big", &big, 0o644)])]);
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), Some("16K"));

        let err = store.pull_image(&mock.reference(":v1")).await.unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
        assert_clean(state.path());
    }

    #[tokio::test]
    async fn unsafe_layers_are_rejected() {
        let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
            (
                "dotdot",
                vec![layer(&[Ent::File("a/../../escape", b"x", 0o644)])],
            ),
            (
                "absolute",
                vec![layer(&[Ent::File("/abs-escape", b"x", 0o644)])],
            ),
            (
                "symlink escape",
                vec![
                    layer(&[Ent::Symlink("link", "../../../tmp")]),
                    layer(&[Ent::File("link/escape", b"x", 0o644)]),
                ],
            ),
            (
                "hardlink escape via dotdot",
                vec![layer(&[Ent::Hardlink("h", "../../etc/passwd")])],
            ),
            (
                "hardlink absolute",
                vec![layer(&[Ent::Hardlink("h", "/etc/passwd")])],
            ),
            (
                "hardlink through escaping symlink",
                vec![layer(&[
                    Ent::Symlink("link", "../../.."),
                    Ent::Hardlink("h", "link/etc/passwd"),
                ])],
            ),
        ];
        for (name, layers) in cases {
            let mock = Mock::start(Duration::ZERO).await;
            let manifest = add_image(&mock, &layers);
            add_index(&mock, "v1", &manifest);
            let state = tempfile::tempdir().unwrap();
            let store = store(state.path(), None);

            let result = store.pull_image(&mock.reference(":v1")).await;
            assert!(result.is_err(), "{name} must be rejected");
            assert_clean(state.path());
            assert!(!state.path().join("escape").exists(), "{name}");
        }
    }

    #[tokio::test]
    async fn failed_pull_leaves_nothing() {
        let mock = Mock::start(Duration::ZERO).await;
        let layers = base_layers();
        let manifest = add_image(&mock, &layers);
        add_index(&mock, "v1", &manifest);
        mock.state
            .routes
            .lock()
            .unwrap()
            .remove(&format!("/v2/{REPO}/blobs/{}", digest(&layers[1])));
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), None);

        let err = store.pull_image(&mock.reference(":v1")).await.unwrap_err();
        assert!(matches!(err, FlameError::Network(_)), "{err}");
        assert!(err.to_string().contains("404"), "{err}");
        assert_clean(state.path());
    }

    #[tokio::test]
    async fn zstd_and_plain_layers_are_detected() {
        let mock = Mock::start(Duration::ZERO).await;
        let gz = layer(&[Ent::File("one", b"1", 0o644)]);
        let tar = {
            let mut d = flate2::read::GzDecoder::new(&gz[..]);
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut d, &mut out).unwrap();
            out
        };
        let zst = zstd::encode_all(&tar[..], 1).unwrap();
        let plain = {
            let mut b = tar::Builder::new(Vec::new());
            let mut h = Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_path("two").unwrap();
            h.set_cksum();
            b.append(&h, &b"2"[..]).unwrap();
            b.into_inner().unwrap()
        };
        let manifest = add_image(&mock, &[zst, plain]);
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), None);

        let root = store.pull_image(&mock.reference(":v1")).await.unwrap();
        assert_eq!(std::fs::read(root.rootfs.join("one")).unwrap(), b"1");
        assert_eq!(std::fs::read(root.rootfs.join("two")).unwrap(), b"2");
    }
}

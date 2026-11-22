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
use oci_client::client::{Client, ClientProtocol};
use oci_client::config::ConfigFile;
use oci_client::config::{Architecture, Os};
use oci_client::manifest::{ImageIndexEntry, OciDescriptor, OciImageManifest};
use oci_client::Reference;
use serde_derive::{Deserialize, Serialize};
use tokio::io::AsyncWrite;

use super::progress::{PullEvent, PullPrinter};
use super::registry::{client_config, PullSource};
use super::unpack::unpack_layers;
use crate::config::{FlameRegistry, FlameSandbox};
use std::collections::HashMap;

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

/// When to ask the registry for an image that is already in the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum PullPolicy {
    /// Use the cached image; the registry is asked only when there is none.
    #[default]
    #[value(name = "ifnotpresent")]
    IfNotPresent,
    /// Resolve a tag with the registry again, so that a tag that moved is followed.
    Always,
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
pub(super) fn linux_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
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
    registries: HashMap<String, FlameRegistry>,
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
        let client = Client::new(client_config(protocol));
        Self {
            images_dir,
            max_bytes: config.images.max_bytes,
            client,
            registries: config.images.registries.clone(),
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

    /// Makes sure the image is in the cache and returns it. With `IfNotPresent` a cached image
    /// costs no registry request, also when it is named by a tag: the digest a tag resolved to is
    /// remembered, and a tag that moved in the registry is not followed. With `Always` a tag is
    /// resolved with the registry again (a digest names its content, so it never is).
    pub async fn pull_image(
        &self,
        reference: &str,
        policy: PullPolicy,
    ) -> Result<ImageRoot, FlameError> {
        let reference = parse_reference(reference)?;
        // The alias of a digest names its content and never changes: only a tag is resolved again.
        let alias = if policy == PullPolicy::Always && reference.digest().is_none() {
            None
        } else {
            self.alias(&reference)
        };
        let known = alias.or_else(|| reference.digest().map(str::to_string));
        if let Some(digest) = known {
            if let Some(root) = self.cached(&digest)? {
                self.record_reference(&digest, &reference);
                self.emit(PullEvent::Cached { digest });
                return Ok(root);
            }
        }

        let mirrors = self
            .registries
            .get(reference.registry())
            .into_iter()
            .flat_map(|registry| &registry.mirrors);
        let attempts = mirrors.map(Some).chain(std::iter::once(None));
        let mut errors = Vec::new();
        let mut last_error = None;
        for mirror in attempts {
            let source = match mirror {
                Some(mirror) => PullSource::mirror(&reference, mirror),
                None => Ok(PullSource::upstream(&reference, &self.client)),
            };
            let source = match source {
                Ok(source) => source,
                Err(error) => {
                    errors.push(error.to_string());
                    last_error = Some(error);
                    continue;
                }
            };
            match self.pull_from(&source).await {
                Ok(root) => {
                    // Publish the canonical alias only after the entire image was verified.
                    if reference.digest() != Some(root.digest.as_str()) {
                        self.record_alias(&reference, &root.digest);
                    }
                    return Ok(root);
                }
                Err(error) => {
                    let message = source.error(&error);
                    tracing::warn!("image pull {reference} via {message}");
                    errors.push(message);
                    last_error = Some(error);
                }
            }
        }
        let message = format!("pull {reference} failed; attempted {}", errors.join("; "));
        Err(match last_error {
            Some(FlameError::InvalidConfig(_)) => FlameError::InvalidConfig(message),
            Some(FlameError::Internal(_)) => internal(message),
            _ => FlameError::Network(message),
        })
    }

    async fn pull_from(&self, source: &PullSource) -> Result<ImageRoot, FlameError> {
        let (manifest, digest) = source
            .client
            .pull_image_manifest(&source.remote, &source.auth)
            .await
            .map_err(|e| registry_error(&format!("resolve {}", source.remote), &e))?;
        sha256_hex(&digest)?;
        // Held until the end of the pull: other executors of the node wait here.
        let _lock = self.lock(&digest).await?;
        if let Some(root) = self.cached(&digest)? {
            self.record_reference(&digest, &source.reference);
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
        self.pull(source, digest, manifest, total).await
    }

    /// The cache is keyed by the digest of the platform manifest, which is not what a tag or the
    /// digest of a multi-platform index names. An alias maps such a reference to the platform
    /// digest, so that it finds its cached image without the registry. It is the file
    /// `refs/<registry>/<repository>/<name>`, with the name `:<tag>` or `@sha256-<hex>`: no
    /// component of a repository starts with `:` or `@`, so a name never clashes with a
    /// repository below this one.
    fn alias_path(&self, reference: &Reference) -> Result<PathBuf, FlameError> {
        let name = match reference.digest() {
            Some(digest) => format!("@sha256-{}", sha256_hex(digest)?),
            None => format!(":{}", reference.tag().unwrap_or("latest")),
        };
        Ok(self
            .images_dir
            .join("refs")
            .join(reference.registry())
            .join(reference.repository())
            .join(name))
    }

    /// The platform digest an earlier pull of `reference` resolved to. It is only a hint:
    /// `cached` says whether that image is still there.
    fn alias(&self, reference: &Reference) -> Option<String> {
        let digest = fs::read_to_string(self.alias_path(reference).ok()?).ok()?;
        sha256_hex(&digest).is_ok().then_some(digest)
    }

    /// Best effort: without an alias the next pull asks the registry again.
    fn record_alias(&self, reference: &Reference, digest: &str) {
        let Ok(path) = self.alias_path(reference) else {
            return;
        };
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&path, digest));
        if let Err(e) = written {
            tracing::debug!("could not record the alias of {reference}: {e}");
        }
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
        source: &PullSource,
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
        self.download_blob(source, &manifest.config, &config_path, None)
            .await?;
        let config_json = fs::read_to_string(&config_path)
            .map_err(|e| internal(format!("read image config: {e}")))?;
        let config = parse_image_config(&config_json)?;

        let mut layer_files = Vec::new();
        for (i, layer) in layers.iter().enumerate() {
            let path = blobs.join(format!("layer-{i}"));
            let name = format!("layer {}/{}", i + 1, layers.len());
            self.download_blob(source, layer, &path, Some(&name))
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
            reference: Some(source.reference.to_string()),
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
        source: &PullSource,
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
        source
            .client
            .pull_blob(&source.remote, layer, writer)
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
#[path = "tests/store.rs"]
mod tests;

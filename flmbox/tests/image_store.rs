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

//! End-to-end tests of the image store against an in-process mock registry.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flame_rs::apis::FlameError;
use flate2::write::GzEncoder;
use flate2::Compression;
use flmbox::config::Config;
use flmbox::image::ImageStore;
use sha2::{Digest, Sha256};
use tar::{EntryType, Header};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const REPO: &str = "test/img";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

// ---- mock registry -------------------------------------------------------

struct State {
    routes: Mutex<HashMap<String, (String, Vec<u8>)>>,
    log: Mutex<Vec<String>>,
    tokens: AtomicUsize,
    auth: bool,
    delay: Duration,
}

struct Mock {
    addr: SocketAddr,
    state: Arc<State>,
}

impl Mock {
    async fn start(auth: bool, delay: Duration) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State {
            routes: Mutex::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            tokens: AtomicUsize::new(0),
            auth,
            delay,
        });
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(handle(stream, st.clone(), addr));
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

async fn handle(mut stream: TcpStream, st: Arc<State>, addr: SocketAddr) {
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
    let auth = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("authorization: ")
                .or(l.strip_prefix("Authorization: "))
        })
        .map(str::to_string);
    st.log.lock().unwrap().push(path.clone());

    let (status, headers, body): (&str, String, Vec<u8>) = if path.starts_with("/token") {
        st.tokens.fetch_add(1, Ordering::SeqCst);
        (
            "200 OK",
            "Content-Type: application/json\r\n".into(),
            br#"{"token":"tok"}"#.to_vec(),
        )
    } else if st.auth && auth.as_deref() != Some("Bearer tok") {
        (
            "401 Unauthorized",
            format!(
                "WWW-Authenticate: Bearer realm=\"http://{addr}/token\",service=\"mock\",scope=\"repository:{REPO}:pull\"\r\n"
            ),
            Vec::new(),
        )
    } else {
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
        let (name, kind, mode, link, data): (&str, EntryType, u32, Option<&str>, &[u8]) = match ent
        {
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

const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// Registers a Docker schema 2 image (Docker media types, a config with the
/// legacy Docker fields) and a manifest list under `tag`; returns the digest
/// of the platform manifest.
fn add_docker_image(mock: &Mock, tag: &str, layers: &[Vec<u8>]) -> String {
    let config = br#"{"architecture":"amd64","os":"linux","created":"2024-01-01T00:00:00Z","docker_version":"24.0.0",
        "config":{"Hostname":"","ExposedPorts":null,"Env":["PATH=/usr/bin","A=b"],"Cmd":["/bin/sh"],"Image":"sha256:x",
        "Volumes":null,"WorkingDir":"/work","Entrypoint":null,"OnBuild":null,"Labels":null,"User":"1000"},
        "container_config":{"Hostname":"abc"},
        "history":[{"created":"2024-01-01T00:00:00Z","created_by":"ADD file"}],
        "rootfs":{"type":"layers","diff_ids":[]}}"#
        .to_vec();
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
            r#"{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":{},"digest":"{d}"}}"#,
            l.len()
        ));
    }
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","size":{},"digest":"{config_digest}"}},"layers":[{}]}}"#,
        config.len(),
        descs.join(",")
    )
    .into_bytes();
    let m = digest(&manifest);
    mock.put(
        &format!("/v2/{REPO}/manifests/{m}"),
        DOCKER_MANIFEST,
        manifest,
    );

    let host = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let list = format!(
        r#"{{"schemaVersion":2,"mediaType":"{DOCKER_LIST}","manifests":[
        {{"mediaType":"{DOCKER_MANIFEST}","size":1,"digest":"sha256:{}","platform":{{"architecture":"unknown","os":"unknown"}}}},
        {{"mediaType":"{DOCKER_MANIFEST}","size":1,"digest":"{m}","platform":{{"architecture":"{host}","os":"linux"}}}}]}}"#,
        "e".repeat(64)
    );
    mock.put(
        &format!("/v2/{REPO}/manifests/{tag}"),
        DOCKER_LIST,
        list.into_bytes(),
    );
    m
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

fn store(mock: &Mock, state: &Path, extra: &str, max: Option<&str>) -> ImageStore {
    let mut vars = HashMap::new();
    vars.insert("FLMBOX_STATE_DIR".to_string(), state.display().to_string());
    vars.insert(
        "FLMBOX_IMAGES".to_string(),
        format!("{}{extra}", mock.reference(":v1")),
    );
    if let Some(max) = max {
        vars.insert("FLMBOX_MAX_IMAGE_BYTES".to_string(), max.to_string());
    }
    ImageStore::new(&Config::from_map(&vars).unwrap())
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
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);

    let root = store.ensure_image(&mock.reference(":v1")).await.unwrap();
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
async fn docker_schema2_list_and_manifest_are_pulled() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_docker_image(&mock, "v1", &base_layers());
    let state = tempfile::tempdir().unwrap();
    let store = store(
        &mock,
        state.path(),
        &format!(",{}", mock.reference(":v2")),
        None,
    );

    let root = store.ensure_image(&mock.reference(":v1")).await.unwrap();
    assert_eq!(root.digest, manifest);
    assert_eq!(root.config.env, ["PATH=/usr/bin", "A=b"]);
    assert_eq!(root.config.cmd, ["/bin/sh"]);
    assert!(root.config.entrypoint.is_empty());
    assert_eq!(root.config.working_dir.as_deref(), Some("/work"));
    assert_eq!(root.config.user.as_deref(), Some("1000"));
    assert_eq!(
        std::fs::read_to_string(root.rootfs.join("etc/os-release")).unwrap(),
        "ID=v2\n"
    );

    // The plain Docker manifest, by tag, works as well.
    let body = mock
        .state
        .routes
        .lock()
        .unwrap()
        .get(&format!("/v2/{REPO}/manifests/{manifest}"))
        .cloned()
        .unwrap();
    mock.put(&format!("/v2/{REPO}/manifests/v2"), DOCKER_MANIFEST, body.1);
    let again = store.ensure_image(&mock.reference(":v2")).await.unwrap();
    assert_eq!(again.digest, manifest);
}

#[tokio::test]
async fn second_call_hits_the_cache() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(
        &mock,
        state.path(),
        &format!(",{}@{manifest}", mock.reference("")),
        None,
    );

    let first = store.ensure_image(&mock.reference(":v1")).await.unwrap();
    let blobs = mock.requests("/blobs/");
    assert_eq!(blobs, 3);
    let before = mock.total();

    // A tag costs the manifest request(s) only, never a blob.
    let second = store.ensure_image(&mock.reference(":v1")).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(mock.requests("/blobs/"), blobs);
    assert!(mock.total() - before <= 2);

    // A cached digest costs nothing.
    let before = mock.total();
    let third = store
        .ensure_image(&format!("{}@{manifest}", mock.reference("")))
        .await
        .unwrap();
    assert_eq!(first, third);
    assert_eq!(mock.total(), before);
}

#[tokio::test]
async fn concurrent_calls_pull_once() {
    let mock = Mock::start(false, Duration::from_millis(50)).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);
    let r = mock.reference(":v1");

    let (a, b, c, d) = tokio::join!(
        store.ensure_image(&r),
        store.ensure_image(&r),
        store.ensure_image(&r),
        store.ensure_image(&r)
    );
    for result in [a, b, c, d] {
        assert_eq!(result.unwrap().digest, manifest);
    }
    assert_eq!(mock.requests("/blobs/"), 3);
}

#[tokio::test]
async fn allowlist_rejects_before_any_request() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);

    for bad in [
        mock.reference(":v2"),
        format!("{}@{manifest}", mock.reference("")),
        "ubuntu:24.04".to_string(),
    ] {
        let err = store.ensure_image(&bad).await.unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)), "{bad}: {err}");
    }
    assert!(matches!(
        store.ensure_image("not a ref").await,
        Err(FlameError::InvalidConfig(_))
    ));
    assert_eq!(mock.total(), 0);
}

#[tokio::test]
async fn bearer_token_auth() {
    let mock = Mock::start(true, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);

    let root = store.ensure_image(&mock.reference(":v1")).await.unwrap();
    assert_eq!(root.digest, manifest);
    assert_eq!(mock.state.tokens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn plain_manifest_tag_works() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    let body = mock
        .state
        .routes
        .lock()
        .unwrap()
        .get(&format!("/v2/{REPO}/manifests/{manifest}"))
        .cloned()
        .unwrap();
    mock.put(&format!("/v2/{REPO}/manifests/v1"), OCI_MANIFEST, body.1);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);
    assert_eq!(
        store
            .ensure_image(&mock.reference(":v1"))
            .await
            .unwrap()
            .digest,
        manifest
    );
}

#[tokio::test]
async fn corrupt_layer_is_rejected() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let layers = base_layers();
    let manifest = add_image(&mock, &layers);
    add_index(&mock, "v1", &manifest);
    // Same length, different bytes.
    let mut tampered = layers[0].clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    mock.put(
        &format!("/v2/{REPO}/blobs/{}", digest(&layers[0])),
        "application/octet-stream",
        tampered,
    );
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);

    let err = store
        .ensure_image(&mock.reference(":v1"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("digest mismatch"), "{err}");
    assert_clean(state.path());
}

#[tokio::test]
async fn manifest_must_match_requested_digest() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    let other = add_image(&mock, &base_layers()[..1]);
    let body = mock
        .state
        .routes
        .lock()
        .unwrap()
        .get(&format!("/v2/{REPO}/manifests/{other}"))
        .cloned()
        .unwrap();
    // Serve a different manifest under the first digest.
    mock.put(
        &format!("/v2/{REPO}/manifests/{manifest}"),
        OCI_MANIFEST,
        body.1,
    );
    let state = tempfile::tempdir().unwrap();
    let store = store(
        &mock,
        state.path(),
        &format!(",{}@{manifest}", mock.reference("")),
        None,
    );

    let err = store
        .ensure_image(&format!("{}@{manifest}", mock.reference("")))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("manifest digest mismatch"),
        "{err}"
    );
    assert_clean(state.path());
}

#[tokio::test]
async fn size_cap_is_enforced_before_download() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", Some("100"));

    let err = store
        .ensure_image(&mock.reference(":v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, FlameError::InvalidConfig(_)), "{err}");
    assert_eq!(mock.requests("/blobs/"), 0);
    assert_clean(state.path());
}

#[tokio::test]
async fn unpacked_size_cap_is_enforced() {
    let mock = Mock::start(false, Duration::ZERO).await;
    // Compresses to almost nothing but expands past the cap.
    let big = vec![0u8; 64 * 1024];
    let manifest = add_image(&mock, &[layer(&[Ent::File("big", &big, 0o644)])]);
    add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", Some("16K"));

    let err = store
        .ensure_image(&mock.reference(":v1"))
        .await
        .unwrap_err();
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
        let mock = Mock::start(false, Duration::ZERO).await;
        let manifest = add_image(&mock, &layers);
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(&mock, state.path(), "", None);

        let result = store.ensure_image(&mock.reference(":v1")).await;
        assert!(result.is_err(), "{name} must be rejected");
        assert_clean(state.path());
        assert!(!state.path().join("escape").exists(), "{name}");
    }
}

#[tokio::test]
async fn failed_pull_leaves_nothing() {
    let mock = Mock::start(false, Duration::ZERO).await;
    let layers = base_layers();
    let manifest = add_image(&mock, &layers);
    add_index(&mock, "v1", &manifest);
    mock.state
        .routes
        .lock()
        .unwrap()
        .remove(&format!("/v2/{REPO}/blobs/{}", digest(&layers[1])));
    let state = tempfile::tempdir().unwrap();
    let store = store(&mock, state.path(), "", None);

    let err = store
        .ensure_image(&mock.reference(":v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, FlameError::NotFound(_)), "{err}");
    assert_clean(state.path());
}

#[tokio::test]
async fn zstd_and_plain_layers_are_detected() {
    let mock = Mock::start(false, Duration::ZERO).await;
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
    let store = store(&mock, state.path(), "", None);

    let root = store.ensure_image(&mock.reference(":v1")).await.unwrap();
    assert_eq!(std::fs::read(root.rootfs.join("one")).unwrap(), b"1");
    assert_eq!(std::fs::read(root.rootfs.join("two")).unwrap(), b"2");
}

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

use oci_client::manifest::{ImageIndexEntry, Platform};

use super::*;
use crate::config::{FlameRegistry, FlameRegistryMirror};
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
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{digest}")
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

/// Registers an index under `tag` and under its digest, which it returns, that selects
/// `manifest` for the host arch.
fn add_index(mock: &Mock, tag: &str, manifest: &str) -> String {
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
    let index = index.into_bytes();
    let index_digest = digest(&index);
    mock.put(
        &format!("/v2/{REPO}/manifests/{index_digest}"),
        OCI_INDEX,
        index.clone(),
    );
    mock.put(&format!("/v2/{REPO}/manifests/{tag}"), OCI_INDEX, index);
    index_digest
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

    let root = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap();
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

    let first = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap();
    let blobs = mock.requests("/blobs/");
    assert_eq!(blobs, 3);
    let manifests = mock.requests("/manifests/");

    // A cached tag costs no registry request, and `Always` the manifest requests (index
    // and platform manifest) only, never a blob.
    let second = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(mock.requests("/manifests/"), manifests);
    let always = store
        .pull_image(&mock.reference(":v1"), PullPolicy::Always)
        .await
        .unwrap();
    assert_eq!(first, always);
    assert_eq!(mock.requests("/blobs/"), blobs);
    assert_eq!(mock.requests("/manifests/"), manifests + 2);

    // The reference is recorded for `list`, also on an image cached without one.
    let images = store.list().unwrap();
    assert_eq!(images.len(), 1);
    assert!(images[0].reference.as_deref().unwrap().contains(":v1"));

    // A cached digest costs nothing.
    let before = mock.total();
    let third = store
        .pull_image(
            &format!("{}@{manifest}", mock.reference("")),
            PullPolicy::IfNotPresent,
        )
        .await
        .unwrap();
    assert_eq!(first, third);
    assert_eq!(mock.total(), before);
}

/// A cached image pulled by the digest of a multi-platform index costs no registry request,
/// though the cache is keyed by the digest of the platform manifest.
#[tokio::test]
async fn an_index_digest_costs_no_registry_request_once_cached() {
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    let by_index = format!(
        "{}@{}",
        mock.reference(""),
        add_index(&mock, "v1", &manifest)
    );
    let state = tempfile::tempdir().unwrap();
    let (first_store, other_store) = (store(state.path(), None), store(state.path(), None));

    let first = first_store
        .pull_image(&by_index, PullPolicy::IfNotPresent)
        .await
        .unwrap();

    let before = mock.total();
    assert_eq!(
        first_store
            .pull_image(&by_index, PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        first
    );
    // The alias is on disk: another store (another process) does not ask either.
    assert_eq!(
        other_store
            .pull_image(&by_index, PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        first
    );
    assert_eq!(mock.total(), before);
}

/// An alias is a hint: a wrong or damaged one falls back to the registry, which corrects it.
#[tokio::test]
async fn a_bad_alias_falls_back_to_the_registry_and_is_corrected() {
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    let index = add_index(&mock, "v1", &manifest);
    let by_index = format!("{}@{index}", mock.reference(""));
    let state = tempfile::tempdir().unwrap();
    let store = store(state.path(), None);
    let first = store
        .pull_image(&by_index, PullPolicy::IfNotPresent)
        .await
        .unwrap();
    let blobs = mock.requests("/blobs/");

    let not_cached = format!("sha256:{}", "d".repeat(64));
    for bad in [not_cached.as_str(), "../../etc"] {
        fs::write(
            store
                .alias_path(&parse_reference(&by_index).unwrap())
                .unwrap(),
            bad,
        )
        .unwrap();
        let before = mock.total();
        assert_eq!(
            store
                .pull_image(&by_index, PullPolicy::IfNotPresent)
                .await
                .unwrap(),
            first
        );
        assert!(mock.total() > before, "{bad}: it did not ask the registry");
        assert_eq!(
            mock.requests("/blobs/"),
            blobs,
            "{bad}: the image was pulled again"
        );
    }

    let before = mock.total();
    assert_eq!(
        store
            .pull_image(&by_index, PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        first
    );
    assert_eq!(mock.total(), before, "the alias was not corrected");
}

/// With `IfNotPresent` a cached tag is used as it is; `Always` follows a tag that moved.
#[tokio::test]
async fn a_moved_tag_is_followed_by_the_always_policy_only() {
    let mock = Mock::start(Duration::ZERO).await;
    let old = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &old);
    let state = tempfile::tempdir().unwrap();
    let store = store(state.path(), None);
    let first = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap();

    let new = add_image(
        &mock,
        &[layer(&[Ent::File("etc/os-release", b"ID=v2\n", 0o644)])],
    );
    assert_ne!(old, new);
    add_index(&mock, "v1", &new);

    let before = mock.total();
    assert_eq!(
        store
            .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        first
    );
    assert_eq!(mock.total(), before, "a cached tag asked the registry");

    let moved = store
        .pull_image(&mock.reference(":v1"), PullPolicy::Always)
        .await
        .unwrap();
    assert_ne!(moved, first);
    // The tag is what it resolved to now.
    assert_eq!(
        store
            .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        moved
    );

    // Another tag of the same repository has its own alias, and a digest is not resolved again.
    add_index(&mock, "v2", &old);
    assert_eq!(
        store
            .pull_image(&mock.reference(":v2"), PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        first
    );
    let by_digest = format!("{}@{old}", mock.reference(""));
    let before = mock.total();
    assert_eq!(
        store
            .pull_image(&by_digest, PullPolicy::Always)
            .await
            .unwrap(),
        first
    );
    assert_eq!(mock.total(), before);
}

/// Aliases are nested by image, and a tag never clashes with a repository below it.
#[test]
fn aliases_are_nested_by_image() {
    let state = tempfile::tempdir().unwrap();
    let store = store(state.path(), None);
    let (one, two) = (
        parse_reference("example.com/a/b:c").unwrap(),
        parse_reference("example.com/a/b/c:d").unwrap(),
    );
    let digest = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    store.record_alias(&one, &digest('1'));
    store.record_alias(&two, &digest('2'));

    assert_eq!(store.alias(&one), Some(digest('1')));
    assert_eq!(store.alias(&two), Some(digest('2')));
    let refs = state.path().join("images/refs/example.com/a/b");
    assert!(refs.join(":c").is_file());
    assert!(refs.join("c/:d").is_file());

    let by_digest = parse_reference(&format!("example.com/a/b@{}", digest('3'))).unwrap();
    store.record_alias(&by_digest, &digest('4'));
    assert!(refs.join(format!("@sha256-{}", "3".repeat(64))).is_file());
    assert_eq!(store.alias(&by_digest), Some(digest('4')));
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
        store.pull_image(&r, PullPolicy::IfNotPresent),
        store.pull_image(&r, PullPolicy::IfNotPresent),
        store.pull_image(&r, PullPolicy::IfNotPresent),
        store.pull_image(&r, PullPolicy::IfNotPresent)
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

    let (x, y) = tokio::join!(
        a.pull_image(&r, PullPolicy::IfNotPresent),
        b.pull_image(&r, PullPolicy::IfNotPresent)
    );
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

    let err = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap_err();
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

    let err = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
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
        let mock = Mock::start(Duration::ZERO).await;
        let manifest = add_image(&mock, &layers);
        add_index(&mock, "v1", &manifest);
        let state = tempfile::tempdir().unwrap();
        let store = store(state.path(), None);

        let result = store
            .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
            .await;
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

    let err = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap_err();
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

    let root = store
        .pull_image(&mock.reference(":v1"), PullPolicy::IfNotPresent)
        .await
        .unwrap();
    assert_eq!(std::fs::read(root.rootfs.join("one")).unwrap(), b"1");
    assert_eq!(std::fs::read(root.rootfs.join("two")).unwrap(), b"2");
}

fn mirror(endpoint: String) -> FlameRegistryMirror {
    FlameRegistryMirror {
        endpoint,
        ca_file: None,
        username_env: None,
        password_env: None,
    }
}

fn configured(state: &Path, upstream: &str, mirrors: Vec<FlameRegistryMirror>) -> ImageStore {
    let mut cfg = FlameSandbox::from_map(&HashMap::from([(
        "FLMBOX_STATE_DIR".into(),
        state.display().to_string(),
    )]))
    .unwrap();
    cfg.images
        .registries
        .insert(upstream.into(), FlameRegistry { mirrors });
    ImageStore::with_protocol(&cfg, ClientProtocol::Http)
}

#[tokio::test]
async fn canonical_tag_and_index_digest_use_mirror_and_canonical_cache_identity() {
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    let index = add_index(&mock, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        "docker.io",
        vec![mirror(format!("http://{}", mock.addr))],
    );
    let canonical = format!("docker.io/{REPO}:v1");
    let root = images
        .pull_image(&canonical, PullPolicy::IfNotPresent)
        .await
        .unwrap();
    assert_eq!(root.digest, manifest);
    assert_eq!(
        images.list().unwrap()[0].reference.as_deref(),
        Some(canonical.as_str())
    );
    let pinned = format!("docker.io/{REPO}@{index}");
    assert_eq!(
        images
            .pull_image(&pinned, PullPolicy::Always)
            .await
            .unwrap(),
        root
    );
    let requests = mock.total();
    assert_eq!(
        images
            .pull_image(&canonical, PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        root
    );
    assert_eq!(
        images
            .pull_image(&pinned, PullPolicy::IfNotPresent)
            .await
            .unwrap(),
        root
    );
    assert_eq!(
        mock.total(),
        requests,
        "canonical aliases must hit cache without transport"
    );
    images
        .pull_image(&canonical, PullPolicy::Always)
        .await
        .unwrap();
    assert!(
        mock.total() > requests,
        "Always must re-resolve the canonical tag"
    );
}

#[tokio::test]
async fn mirror_prefix_maps_repository_for_manifests_and_blobs() {
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    {
        let mut routes = mock.state.routes.lock().unwrap();
        *routes = routes
            .iter()
            .map(|(k, v)| (k.replacen("/v2/", "/v2/dockerhub/", 1), v.clone()))
            .collect();
    }
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        "docker.io",
        vec![mirror(format!("http://{}/dockerhub", mock.addr))],
    );
    images
        .pull_image(&format!("docker.io/{REPO}:v1"), PullPolicy::Always)
        .await
        .unwrap();
    assert!(mock.requests("/v2/dockerhub/test/img/blobs/") > 0);
}

#[tokio::test]
async fn failed_mirror_tries_next_mirror() {
    let failed = Mock::start(Duration::ZERO).await;
    let good = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&good, &base_layers());
    add_index(&good, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        "docker.io",
        vec![
            mirror(format!("http://{}", failed.addr)),
            mirror(format!("http://{}", good.addr)),
        ],
    );
    assert_eq!(
        images
            .pull_image(&format!("docker.io/{REPO}:v1"), PullPolicy::Always)
            .await
            .unwrap()
            .digest,
        manifest
    );
    assert!(failed.total() > 0);
    assert!(good.total() > 0);
}

#[tokio::test]
async fn mirror_blob_failure_retries_complete_pull_on_next_mirror() {
    let failed = Mock::start(Duration::ZERO).await;
    let good = Mock::start(Duration::ZERO).await;
    for mock in [&failed, &good] {
        let manifest = add_image(mock, &base_layers());
        add_index(mock, "v1", &manifest);
    }
    failed
        .state
        .routes
        .lock()
        .unwrap()
        .retain(|k, _| !k.contains("/blobs/"));
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        "docker.io",
        vec![
            mirror(format!("http://{}", failed.addr)),
            mirror(format!("http://{}", good.addr)),
        ],
    );
    images
        .pull_image(&format!("docker.io/{REPO}:v1"), PullPolicy::Always)
        .await
        .unwrap();
    assert!(failed.requests("/blobs/") > 0);
    assert!(good.requests("/manifests/v1") > 0);
    assert!(good.requests("/blobs/") > 0);
}

#[tokio::test]
async fn exhausted_mirrors_and_upstream_report_both_endpoints() {
    let failed = Mock::start(Duration::ZERO).await;
    let upstream = Mock::start(Duration::ZERO).await;
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![mirror(format!("http://{}", failed.addr))],
    );
    let err = images
        .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&failed.addr.to_string()), "{err}");
    assert!(err.contains("404"), "{err}");
    assert!(upstream.total() > 0);
    assert!(err.contains(&upstream.addr.to_string()), "{err}");
    assert_clean(state.path());
}

#[tokio::test]
async fn mirror_cannot_substitute_content_for_requested_digest() {
    let upstream = Mock::start(Duration::ZERO).await;
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    let fake = format!("sha256:{}", "f".repeat(64));
    let (ct, bytes) = mock
        .state
        .routes
        .lock()
        .unwrap()
        .get(&format!("/v2/{REPO}/manifests/{manifest}"))
        .unwrap()
        .clone();
    mock.put(&format!("/v2/{REPO}/manifests/{fake}"), &ct, bytes);
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![mirror(format!("http://{}", mock.addr))],
    );
    assert!(images
        .pull_image(&upstream.reference(&format!("@{fake}")), PullPolicy::Always)
        .await
        .is_err());
    assert_clean(state.path());
}

#[tokio::test]
async fn mirror_cannot_publish_corrupted_blob() {
    let upstream = Mock::start(Duration::ZERO).await;
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    {
        let mut routes = mock.state.routes.lock().unwrap();
        let (_, body) = routes
            .iter_mut()
            .find(|(k, _)| k.contains("/blobs/"))
            .unwrap()
            .1;
        body[0] ^= 1;
    }
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![mirror(format!("http://{}", mock.addr))],
    );
    assert!(images
        .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
        .await
        .is_err());
    assert_clean(state.path());
}

// Test-only CA and key; no deployment credentials. TLS terminates in process,
// then forwards bytes to the same OCI fixture used by the plain HTTP tests.
async fn tls_front(mock: &Mock, ip_san: bool) -> (SocketAddr, tempfile::NamedTempFile) {
    use rustls::pki_types::PrivatePkcs8KeyDer;
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Flame test registry CA");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    ca_params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    ca_params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(30);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let names: Vec<String> = if ip_san {
        vec!["localhost".into(), "127.0.0.1".into()]
    } else {
        vec!["wrong-host.example.com".into()]
    };
    let mut params = rcgen::CertificateParams::new(names).unwrap();
    params.not_before = ca_params.not_before;
    params.not_after = ca_params.not_after;
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server = params.signed_by(&server_key, &issuer).unwrap();
    let cert = server.der().clone();
    let key = PrivatePkcs8KeyDer::from(server_key.serialize_der()).into();
    let ca_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(ca_file.path(), ca.pem()).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = mock.addr;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let mut backend = TcpStream::connect(target).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut backend).await;
                }
            });
        }
    });
    (addr, ca_file)
}

#[tokio::test]
async fn private_ca_https_mirror_by_ip_requires_trust_and_works_without_authentication() {
    let upstream = Mock::start(Duration::ZERO).await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let (addr, ca_file) = tls_front(&mock, true).await;
    let state = tempfile::tempdir().unwrap();
    let endpoint = format!("https://{addr}");
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![mirror(endpoint.clone())],
    );
    let err = images
        .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&addr.to_string()), "{err}");
    assert_eq!(mock.total(), 0, "untrusted TLS must not reach the registry");
    let mut trusted = mirror(endpoint);
    trusted.ca_file = Some(ca_file.path().to_path_buf());
    let images = configured(state.path(), &upstream.addr.to_string(), vec![trusted]);
    assert_eq!(
        images
            .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
            .await
            .unwrap()
            .digest,
        manifest
    );
}

#[tokio::test]
async fn fallback_reaches_upstream_and_unconfigured_registry_keeps_direct_transport() {
    let failed = Mock::start(Duration::ZERO).await;
    let upstream = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&upstream, &base_layers());
    add_index(&upstream, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let mut cfg = FlameSandbox::from_map(&HashMap::from([(
        "FLMBOX_STATE_DIR".into(),
        state.path().display().to_string(),
    )]))
    .unwrap();
    cfg.images.registries.insert(
        upstream.addr.to_string(),
        FlameRegistry {
            mirrors: vec![mirror(format!("http://{}", failed.addr))],
        },
    );
    let images = ImageStore::with_protocol(&cfg, ClientProtocol::Http);
    assert_eq!(
        images
            .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
            .await
            .unwrap()
            .digest,
        manifest
    );
    assert!(failed.total() > 0);
    let previous = failed.total();
    // A different registry must not inherit the configured upstream's mirrors.
    let other = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&other, &base_layers());
    add_index(&other, "v2", &manifest);
    images
        .pull_image(&other.reference(":v2"), PullPolicy::Always)
        .await
        .unwrap();
    assert_eq!(failed.total(), previous);
    assert!(other.total() > 0);
}

#[tokio::test]
async fn mirror_authentication_uses_named_environment_without_disclosing_secrets() {
    let upstream = Mock::start(Duration::ZERO).await;
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = mock.addr;
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    request.push(byte[0]);
                    if request.len() > 65536 {
                        return;
                    }
                }
                let header = String::from_utf8_lossy(&request).to_ascii_lowercase();
                // base64("testuser:secret728")
                if !header.contains("authorization: basic dgvzdhvzzxi6c2vjcmv0nzi4\r\n") {
                    let _=stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                }
                if header.contains("/manifests/missing ") {
                    let body = br#"{"errors":[{"code":"DENIED","message":"secret728"}]}"#;
                    let response = format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    return;
                }
                let mut backend = TcpStream::connect(target).await.unwrap();
                backend.write_all(&request).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut backend).await;
            });
        }
    });
    let state = tempfile::tempdir().unwrap();
    let mut authenticated = mirror(format!("http://{addr}"));
    authenticated.username_env = Some("FLAME_TEST728_REGISTRY_USER".into());
    authenticated.password_env = Some("FLAME_TEST728_REGISTRY_PASSWORD".into());
    std::env::set_var("FLAME_TEST728_REGISTRY_USER", "testuser");
    std::env::set_var("FLAME_TEST728_REGISTRY_PASSWORD", "secret728");
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![authenticated],
    );
    let result = images
        .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
        .await;
    assert_eq!(result.unwrap().digest, manifest);
    let error = images
        .pull_image(&upstream.reference(":missing"), PullPolicy::Always)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret728"), "{error}");
    assert!(error.contains(&addr.to_string()), "{error}");
    assert!(error.contains("[REDACTED]"), "{error}");
    std::env::remove_var("FLAME_TEST728_REGISTRY_USER");
    std::env::remove_var("FLAME_TEST728_REGISTRY_PASSWORD");
    let error = images
        .pull_image(&upstream.reference(":missing"), PullPolicy::Always)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("FLAME_TEST728_REGISTRY_USER"), "{error}");
}

#[tokio::test]
async fn custom_ca_does_not_disable_hostname_verification() {
    let upstream = Mock::start(Duration::ZERO).await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&mock, &base_layers());
    add_index(&mock, "v1", &manifest);
    let (addr, ca_file) = tls_front(&mock, false).await;
    let state = tempfile::tempdir().unwrap();
    let mut endpoint = mirror(format!("https://{addr}"));
    endpoint.ca_file = Some(ca_file.path().to_path_buf());
    let images = configured(state.path(), &upstream.addr.to_string(), vec![endpoint]);
    let error = images
        .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(&addr.to_string()), "{error}");
    assert_eq!(
        mock.total(),
        0,
        "hostname verification must reject before HTTP"
    );
}

#[tokio::test]
async fn empty_mirror_list_pulls_directly_from_upstream() {
    let upstream = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&upstream, &base_layers());
    add_index(&upstream, "v1", &manifest);
    let state = tempfile::tempdir().unwrap();
    let images = configured(state.path(), &upstream.addr.to_string(), Vec::new());
    assert_eq!(
        images
            .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
            .await
            .unwrap()
            .digest,
        manifest
    );
    assert!(upstream.total() > 0);
}

#[tokio::test]
async fn source_initialization_failures_try_the_next_mirror() {
    let good = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&good, &base_layers());
    add_index(&good, "v1", &manifest);
    let mut bad_ca = mirror(format!("https://{}", good.addr));
    let missing = tempfile::tempdir().unwrap();
    bad_ca.ca_file = Some(missing.path().join("absent-ca.pem"));
    for invalid in [mirror("not a URL".into()), bad_ca] {
        let state = tempfile::tempdir().unwrap();
        let upstream = Mock::start(Duration::ZERO).await;
        let images = configured(
            state.path(),
            &upstream.addr.to_string(),
            vec![invalid, mirror(format!("http://{}", good.addr))],
        );
        assert_eq!(
            images
                .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
                .await
                .unwrap()
                .digest,
            manifest
        );
        assert_eq!(
            upstream.total(),
            0,
            "a successful later mirror ends attempts"
        );
    }
}

#[tokio::test]
async fn invalid_repository_prefix_is_tried_before_next_mirror() {
    let good = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&good, &base_layers());
    add_index(&good, "v1", &manifest);
    let upstream = Mock::start(Duration::ZERO).await;
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![
            mirror(format!("http://{}/bad:prefix", good.addr)),
            mirror(format!("http://{}", good.addr)),
        ],
    );
    assert_eq!(
        images
            .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
            .await
            .unwrap()
            .digest,
        manifest
    );
    assert!(good.requests("/v2/bad:prefix/") > 0);
    assert_eq!(upstream.total(), 0);
}

#[tokio::test]
async fn initialization_and_registry_errors_are_reported_only_after_all_sources_fail() {
    let upstream = Mock::start(Duration::ZERO).await;
    let failed = Mock::start(Duration::ZERO).await;
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![
            mirror("not a URL".into()),
            mirror(format!("http://user:secret728@{}/private", failed.addr)),
            mirror(format!("http://{}/wrong-prefix", failed.addr)),
        ],
    );
    let error = images
        .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
        .await
        .unwrap_err()
        .to_string();
    for endpoint in [
        "invalid mirror URL".into(),
        failed.addr.to_string(),
        upstream.addr.to_string(),
    ] {
        assert!(error.contains(&endpoint), "{error}");
    }
    assert!(!error.contains("secret728"), "{error}");
    assert!(failed.total() > 0);
    assert!(upstream.total() > 0);
}

#[tokio::test]
async fn invalid_remote_manifest_limits_try_the_next_mirror() {
    let bad = Mock::start(Duration::ZERO).await;
    let config = format!("sha256:{}", "f".repeat(64));
    let oversized = format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":{}}},"layers":[]}}"#,
        MAX_CONFIG_BYTES + 1
    );
    bad.put(
        &format!("/v2/{REPO}/manifests/v1"),
        OCI_MANIFEST,
        oversized.into_bytes(),
    );
    let good = Mock::start(Duration::ZERO).await;
    let manifest = add_image(&good, &base_layers());
    add_index(&good, "v1", &manifest);
    let upstream = Mock::start(Duration::ZERO).await;
    let state = tempfile::tempdir().unwrap();
    let images = configured(
        state.path(),
        &upstream.addr.to_string(),
        vec![
            mirror(format!("http://{}", bad.addr)),
            mirror(format!("http://{}", good.addr)),
        ],
    );
    assert_eq!(
        images
            .pull_image(&upstream.reference(":v1"), PullPolicy::Always)
            .await
            .unwrap()
            .digest,
        manifest
    );
    assert!(bad.total() > 0);
    assert_eq!(upstream.total(), 0);
}

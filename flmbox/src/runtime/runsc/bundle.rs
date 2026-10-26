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

//! OCI runtime bundle writer.
//!
//! The container's init process is `/bin/sh -c 'while :; do sleep 3600; done'`
//! so it stays alive without GNU coreutils. Images without `/bin/sh` are not
//! supported in phase 1. No `linux.resources` are written: limits are applied
//! by the host shim, not by `flmbox`.
//!
//! The spec is built with the typed API of the `oci-spec` crate. Its builders
//! start from a `Default` that adds mounts, masked paths, rlimits, resources
//! and more, so every builder result is explicitly cleared of what `flmbox`
//! does not write (see `minimal_spec`).

use std::fs;
use std::path::Path;

use std::path::PathBuf;

use flame_rs::apis::FlameError;
use oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxNamespace,
    LinuxNamespaceBuilder, LinuxNamespaceType, Mount, MountBuilder, ProcessBuilder, RootBuilder,
    Spec, SpecBuilder, UserBuilder,
};

use crate::config::FlameNetwork;
use crate::images::ImageConfig;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Command of the long-lived init process.
const INIT_SCRIPT: &str = "while :; do sleep 3600; done";

/// The default capability set of Docker.
const CAPABILITIES: [Capability; 14] = [
    Capability::Chown,
    Capability::DacOverride,
    Capability::Fsetid,
    Capability::Fowner,
    Capability::Mknod,
    Capability::NetRaw,
    Capability::Setgid,
    Capability::Setuid,
    Capability::Setfcap,
    Capability::Setpcap,
    Capability::NetBindService,
    Capability::SysChroot,
    Capability::Kill,
    Capability::AuditWrite,
];

/// Writes `<dir>/config.json` (OCI runtime spec 1.0.2) for a sandbox whose
/// root filesystem is `rootfs`, mounted read-only. With [`FlameNetwork::None`]
/// the sandbox has its own, empty network namespace; with [`FlameNetwork::Host`]
/// the spec names no network namespace, so the sandbox uses the network of the
/// process that runs `runsc`, and the host's `/etc/resolv.conf` is bind-mounted
/// read-only so names resolve. Also writes `<dir>/hosts`, bind-mounted read-only
/// at `/etc/hosts`.
pub fn write_bundle(
    dir: &Path,
    rootfs: &Path,
    image: &ImageConfig,
    id: &str,
    network: FlameNetwork,
) -> Result<(), FlameError> {
    let rootfs = std::path::absolute(rootfs)
        .map_err(|e| internal(format!("rootfs path {}: {e}", rootfs.display())))?;

    fs::create_dir_all(dir)
        .map_err(|e| internal(format!("create bundle dir {}: {e}", dir.display())))?;
    let hostname = hostname(id);
    let hosts = format!("127.0.0.1 localhost\n127.0.0.1 {hostname}\n");
    let hosts_path = write_file(dir, "hosts", hosts.as_bytes())?;

    let (uid, gid) = parse_user(image.user.as_deref());
    let cwd = image
        .working_dir
        .as_deref()
        .filter(|d| d.starts_with('/'))
        .unwrap_or("/");
    let mut mounts = vec![
        mount("/proc", "proc", "proc", &["nosuid", "noexec", "nodev"])?,
        mount(
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"],
        )?,
        mount(
            "/dev/shm",
            "tmpfs",
            "shm",
            &["nosuid", "noexec", "nodev", "mode=1777"],
        )?,
        mount("/run", "tmpfs", "tmpfs", &["nosuid", "nodev", "mode=755"])?,
        mount("/etc/hosts", "bind", hosts_path, &["ro", "rbind"])?,
    ];
    if network == FlameNetwork::Host {
        // The sandbox shares the host's network; give it the host's resolver
        // when it has one (a missing file leaves the image's own).
        if let Ok(resolv) = fs::read("/etc/resolv.conf") {
            let resolv_path = write_file(dir, "resolv.conf", &resolv)?;
            mounts.push(mount(
                "/etc/resolv.conf",
                "bind",
                resolv_path,
                &["ro", "rbind"],
            )?);
        }
    }
    let mut namespaces = [
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Ipc,
        LinuxNamespaceType::Uts,
        LinuxNamespaceType::Mount,
    ]
    .map(namespace)
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    if network == FlameNetwork::None {
        namespaces.push(namespace(LinuxNamespaceType::Network)?);
    }

    let spec = minimal_spec(
        &rootfs,
        &hostname,
        uid,
        gid,
        merge_env(&image.env),
        cwd,
        mounts,
        namespaces,
    )?;
    let path = dir.join("config.json");
    spec.save(&path)
        .map_err(|e| internal(format!("write {}: {e}", path.display())))
}

/// The runtime spec of a sandbox. Every `oci-spec` builder falls back to a
/// `Default` that is not minimal (rootfs path, default mounts, masked and
/// read-only paths, rlimits, resources), so what `flmbox` does not write is
/// cleared explicitly.
#[allow(clippy::too_many_arguments)]
fn minimal_spec(
    rootfs: &Path,
    hostname: &str,
    uid: u32,
    gid: u32,
    env: Vec<String>,
    cwd: &str,
    mounts: Vec<Mount>,
    namespaces: Vec<LinuxNamespace>,
) -> Result<Spec, FlameError> {
    let caps: Capabilities = CAPABILITIES.into_iter().collect();
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(caps.clone())
        .effective(caps.clone())
        .permitted(caps)
        .build()
        .map_err(build_err)?;
    // Not part of Docker's default set as written here: leave them unset.
    let mut capabilities = capabilities;
    capabilities.set_inheritable(None);
    capabilities.set_ambient(None);

    let user = UserBuilder::default()
        .uid(uid)
        .gid(gid)
        .build()
        .map_err(build_err)?;
    let mut process = ProcessBuilder::default()
        .terminal(false)
        .user(user)
        .args(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            INIT_SCRIPT.to_string(),
        ])
        .env(env)
        .cwd(cwd)
        .capabilities(capabilities)
        .no_new_privileges(false)
        .build()
        .map_err(build_err)?;
    process.set_rlimits(None);

    let root = RootBuilder::default()
        .path(rootfs)
        .readonly(true)
        .build()
        .map_err(build_err)?;

    let mut linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .build()
        .map_err(build_err)?;
    linux.set_resources(None);
    linux.set_masked_paths(None);
    linux.set_readonly_paths(None);

    let mut spec = SpecBuilder::default()
        .version("1.0.2")
        .process(process)
        .root(root)
        .hostname(hostname)
        .mounts(mounts)
        .linux(linux)
        .build()
        .map_err(build_err)?;
    spec.set_annotations(None);
    Ok(spec)
}

fn mount(
    destination: &str,
    typ: &str,
    source: impl Into<PathBuf>,
    options: &[&str],
) -> Result<Mount, FlameError> {
    MountBuilder::default()
        .destination(destination)
        .typ(typ)
        .source(source)
        .options(options.iter().map(|o| o.to_string()).collect::<Vec<_>>())
        .build()
        .map_err(build_err)
}

fn namespace(typ: LinuxNamespaceType) -> Result<LinuxNamespace, FlameError> {
    LinuxNamespaceBuilder::default()
        .typ(typ)
        .build()
        .map_err(build_err)
}

fn build_err(e: oci_spec::OciSpecError) -> FlameError {
    internal(format!("build bundle config: {e}"))
}

/// Writes `<dir>/<name>` and returns its absolute path.
fn write_file(dir: &Path, name: &str, content: &[u8]) -> Result<PathBuf, FlameError> {
    let path = std::path::absolute(dir.join(name))
        .map_err(|e| internal(format!("path of {name}: {e}")))?;
    fs::write(&path, content).map_err(|e| internal(format!("write {}: {e}", path.display())))?;
    Ok(path)
}

fn internal(msg: String) -> FlameError {
    FlameError::Internal(msg)
}

/// `uid[:gid]`; names cannot be resolved without reading the image's
/// `/etc/passwd`, so they fall back to root (0). The gid defaults to 0.
fn parse_user(user: Option<&str>) -> (u32, u32) {
    let user = user.map(str::trim).unwrap_or_default();
    let (uid, gid) = user.split_once(':').unwrap_or((user, ""));
    (uid.parse().unwrap_or(0), gid.parse().unwrap_or(0))
}

/// Default environment with the image environment merged over it.
fn merge_env(image_env: &[String]) -> Vec<String> {
    let mut env = vec![
        format!("PATH={DEFAULT_PATH}"),
        "TERM=xterm".to_string(),
        "HOME=/root".to_string(),
    ];
    for entry in image_env {
        let key = entry.split('=').next().unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        let same_key = |e: &String| e.split('=').next() == Some(key);
        match env.iter_mut().find(|e| same_key(e)) {
            Some(slot) => *slot = entry.clone(),
            None => env.push(entry.clone()),
        }
    }
    env
}

/// A hostname from the tail of the id (its unique part): at most 12 characters
/// of `[A-Za-z0-9-]`, never starting or ending with `-`.
fn hostname(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let tail = &clean[clean.len().saturating_sub(12)..];
    let tail = tail.trim_matches('-');
    if tail.is_empty() {
        "sandbox".to_string()
    } else {
        tail.to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn written(image: &ImageConfig, id: &str) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        write_bundle(
            &bundle,
            Path::new("/images/rootfs"),
            image,
            id,
            FlameNetwork::None,
        )
        .unwrap();
        serde_json::from_slice(&fs::read(bundle.join("config.json")).unwrap()).unwrap()
    }

    #[test]
    fn basic_shape() {
        let v = written(&ImageConfig::default(), "flmbox-e1.s1");
        assert_eq!(v["ociVersion"], "1.0.2");
        assert_eq!(v["process"]["terminal"], false);
        assert_eq!(v["process"]["noNewPrivileges"], false);
        assert_eq!(
            v["process"]["args"],
            json!(["/bin/sh", "-c", "while :; do sleep 3600; done"])
        );
        assert_eq!(v["process"]["cwd"], "/");
        assert_eq!(v["process"]["user"], json!({"uid": 0, "gid": 0}));
        assert_eq!(
            v["root"],
            json!({"path": "/images/rootfs", "readonly": true})
        );
        assert!(v.get("annotations").is_none());
    }

    #[test]
    fn config_parses_back_as_a_typed_runtime_spec() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            Path::new("/images/rootfs"),
            &ImageConfig::default(),
            "id",
            FlameNetwork::None,
        )
        .unwrap();
        let spec = Spec::load(dir.path().join("config.json")).unwrap();
        assert_eq!(spec.version(), "1.0.2");
        let process = spec.process().as_ref().unwrap();
        assert_eq!(process.no_new_privileges(), Some(false));
        let caps = process.capabilities().as_ref().unwrap();
        assert_eq!(caps.bounding().as_ref().unwrap().len(), 14);
        assert!(caps.inheritable().is_none() && caps.ambient().is_none());
        assert_eq!(spec.mounts().as_ref().unwrap().len(), 5);
        let linux = spec.linux().as_ref().unwrap();
        let namespaces = linux.namespaces().as_ref().unwrap();
        assert_eq!(namespaces.len(), 5);
        assert_eq!(namespaces[4].path(), &None);
    }

    #[test]
    fn oci_spec_defaults_are_not_written() {
        let v = written(&ImageConfig::default(), "id");
        let top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let mut top = top;
        top.sort_unstable();
        assert_eq!(
            top,
            [
                "hostname",
                "linux",
                "mounts",
                "ociVersion",
                "process",
                "root"
            ]
        );
        let linux: Vec<&str> = v["linux"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(linux, ["namespaces"]);
        for key in [
            "resources",
            "seccomp",
            "maskedPaths",
            "readonlyPaths",
            "rlimits",
            "hooks",
        ] {
            assert!(!serde_json::to_string(&v).unwrap().contains(key), "{key}");
        }
        let caps = v["process"]["capabilities"].as_object().unwrap();
        let mut sets: Vec<&str> = caps.keys().map(String::as_str).collect();
        sets.sort_unstable();
        assert_eq!(sets, ["bounding", "effective", "permitted"]);
        assert!(v["process"].get("apparmorProfile").is_none());
        assert!(v["process"].get("consoleSize").is_none());
    }

    #[test]
    fn no_resources_are_written() {
        let v = written(&ImageConfig::default(), "id");
        assert!(v["linux"].get("resources").is_none());
        assert!(!serde_json::to_string(&v).unwrap().contains("resources"));
    }

    #[test]
    fn capabilities_are_the_docker_default() {
        let v = written(&ImageConfig::default(), "id");
        for set in ["bounding", "effective", "permitted"] {
            let caps = v["process"]["capabilities"][set].as_array().unwrap();
            assert_eq!(caps.len(), 14, "{set}");
            assert!(caps.contains(&json!("CAP_NET_BIND_SERVICE")));
            assert!(!caps.contains(&json!("CAP_SYS_ADMIN")));
        }
    }

    #[test]
    fn env_merges_image_over_defaults() {
        let image = ImageConfig {
            env: vec![
                "PATH=/custom/bin".to_string(),
                "LANG=C.UTF-8".to_string(),
                "EMPTY=".to_string(),
                "=bad".to_string(),
            ],
            ..ImageConfig::default()
        };
        let v = written(&image, "id");
        assert_eq!(
            v["process"]["env"],
            json!([
                "PATH=/custom/bin",
                "TERM=xterm",
                "HOME=/root",
                "LANG=C.UTF-8",
                "EMPTY="
            ])
        );
        let v = written(&ImageConfig::default(), "id");
        assert_eq!(
            v["process"]["env"][0],
            format!("PATH={DEFAULT_PATH}").as_str()
        );
    }

    #[test]
    fn working_dir_comes_from_the_image() {
        let image = ImageConfig {
            working_dir: Some("/app".to_string()),
            ..ImageConfig::default()
        };
        assert_eq!(written(&image, "id")["process"]["cwd"], "/app");
        let relative = ImageConfig {
            working_dir: Some("app".to_string()),
            ..ImageConfig::default()
        };
        assert_eq!(written(&relative, "id")["process"]["cwd"], "/");
    }

    #[test]
    fn user_parsing() {
        assert_eq!(parse_user(None), (0, 0));
        assert_eq!(parse_user(Some("")), (0, 0));
        assert_eq!(parse_user(Some("root")), (0, 0));
        assert_eq!(parse_user(Some("1000")), (1000, 0));
        assert_eq!(parse_user(Some("1000:2000")), (1000, 2000));
        assert_eq!(parse_user(Some("nobody")), (0, 0));
        assert_eq!(parse_user(Some("nobody:nogroup")), (0, 0));
        assert_eq!(parse_user(Some("nobody:33")), (0, 33));

        let image = ImageConfig {
            user: Some("1000:2000".to_string()),
            ..ImageConfig::default()
        };
        let v = written(&image, "id");
        assert_eq!(v["process"]["user"], json!({"uid": 1000, "gid": 2000}));
    }

    #[test]
    fn mounts_and_namespaces() {
        let v = written(&ImageConfig::default(), "id");
        let mounts: Vec<(&str, &str)> = v["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["destination"].as_str().unwrap(),
                    m["type"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            mounts,
            vec![
                ("/proc", "proc"),
                ("/sys", "sysfs"),
                ("/dev/shm", "tmpfs"),
                ("/run", "tmpfs"),
                ("/etc/hosts", "bind")
            ]
        );
        assert!(v["mounts"][1]["options"]
            .as_array()
            .unwrap()
            .contains(&json!("ro")));

        let types: Vec<&str> = v["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["pid", "ipc", "uts", "mount", "network"]);
    }

    #[test]
    fn hosts_file_is_bind_mounted_read_only() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            Path::new("/images/rootfs"),
            &ImageConfig::default(),
            "flmbox-e.0123456789abcdef",
            FlameNetwork::None,
        )
        .unwrap();
        let v: Value =
            serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
        let hosts = dir.path().join("hosts");
        assert_eq!(
            fs::read_to_string(&hosts).unwrap(),
            "127.0.0.1 localhost\n127.0.0.1 456789abcdef\n"
        );
        let m = v["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["destination"] == "/etc/hosts")
            .unwrap();
        assert_eq!(m["type"], "bind");
        assert_eq!(m["source"], hosts.to_str().unwrap());
        assert_eq!(m["options"], json!(["ro", "rbind"]));
        assert!(!v["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["destination"] == "/etc/resolv.conf"));
    }

    #[test]
    fn sandbox_gets_its_own_empty_network_namespace() {
        let v = written(&ImageConfig::default(), "id");
        assert_eq!(v["linux"]["namespaces"][4], json!({"type": "network"}));
    }

    #[test]
    fn hostname_is_a_sanitised_short_id() {
        assert_eq!(hostname("flmbox-exec1.session-42"), "1-session-42");
        assert_eq!(hostname("a_b.c"), "a-b-c");
        assert_eq!(hostname("---"), "sandbox");
        assert_eq!(hostname("x.-"), "x");
        let v = written(&ImageConfig::default(), "flmbox-e.0123456789abcdef");
        assert_eq!(v["hostname"], "456789abcdef");
    }

    #[test]
    fn relative_rootfs_is_made_absolute() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            Path::new("rel/rootfs"),
            &ImageConfig::default(),
            "id",
            FlameNetwork::None,
        )
        .unwrap();
        let v: Value =
            serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
        assert!(Path::new(v["root"]["path"].as_str().unwrap()).is_absolute());
    }

    #[test]
    fn host_network_has_no_network_namespace_and_mounts_the_resolver() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            Path::new("/images/rootfs"),
            &ImageConfig::default(),
            "id",
            FlameNetwork::Host,
        )
        .unwrap();
        let v: Value =
            serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
        let types: Vec<_> = v["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["pid", "ipc", "uts", "mount"]);
        if Path::new("/etc/resolv.conf").is_file() {
            let resolv = v["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["destination"] == "/etc/resolv.conf")
                .expect("resolv.conf is mounted");
            assert_eq!(
                resolv["source"],
                dir.path().join("resolv.conf").to_str().unwrap()
            );
            assert_eq!(resolv["options"], json!(["ro", "rbind"]));
            assert_eq!(
                fs::read(dir.path().join("resolv.conf")).unwrap(),
                fs::read("/etc/resolv.conf").unwrap()
            );
        }
    }

    #[test]
    fn no_network_keeps_its_own_namespace_and_no_resolver() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            Path::new("/images/rootfs"),
            &ImageConfig::default(),
            "id",
            FlameNetwork::None,
        )
        .unwrap();
        let v: Value =
            serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
        assert!(v["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["type"] == "network"));
        assert!(!v["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["destination"] == "/etc/resolv.conf"));
        assert!(!dir.path().join("resolv.conf").exists());
    }
}

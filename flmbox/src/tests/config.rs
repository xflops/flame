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

use super::*;

fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn runsc(c: &FlameSandbox) -> &FlameRunsc {
    let FlameRuntime::Runsc(r) = &c.runtime;
    r
}

#[test]
fn network_defaults_to_none_and_follows_the_file_and_the_environment() {
    let c = FlameSandbox::from_map(&HashMap::new()).unwrap();
    assert_eq!(c.network, FlameNetwork::None);

    let vars = HashMap::new();
    let c = FlameSandbox::from_yaml("sandbox:\n  network: host\n", &vars).unwrap();
    assert_eq!(c.network, FlameNetwork::Host);
    let c = FlameSandbox::from_yaml("sandbox:\n  network: None\n", &vars).unwrap();
    assert_eq!(c.network, FlameNetwork::None);

    // The environment wins over the file.
    let c = FlameSandbox::from_yaml(
        "sandbox:\n  network: host\n",
        &map(&[(ENV_NETWORK, "none")]),
    )
    .unwrap();
    assert_eq!(c.network, FlameNetwork::None);
    let c = FlameSandbox::from_map(&map(&[(ENV_NETWORK, " HOST ")])).unwrap();
    assert_eq!(c.network, FlameNetwork::Host);
}

#[test]
fn unknown_network_mode_is_rejected() {
    let err = FlameSandbox::from_map(&map(&[(ENV_NETWORK, "bridge")])).unwrap_err();
    assert!(err.to_string().contains("FLMBOX_NETWORK"), "{err}");
    let err =
        FlameSandbox::from_yaml("sandbox:\n  network: sandbox\n", &HashMap::new()).unwrap_err();
    assert!(err.to_string().contains("sandbox.network"), "{err}");
}

#[test]
fn registry_mirrors_are_loaded_and_allow_direct_upstream() {
    let c = FlameSandbox::from_yaml(
            "sandbox:\n  images:\n    registries:\n      docker.io:\n        mirrors:\n          - endpoint: https://192.168.1.2:5000/hub\n            ca_file: /etc/flame/registry-ca.pem\n            username_env: REGISTRY_USER\n            password_env: REGISTRY_PASSWORD\n",
            &HashMap::new(),
        ).unwrap();
    let registry = &c.images.registries["docker.io"];
    assert_eq!(registry.mirrors[0].endpoint, "https://192.168.1.2:5000/hub");
    assert_eq!(c.images.max_bytes, 4 << 30);
    assert!(FlameSandbox::from_map(&HashMap::new())
        .unwrap()
        .images
        .registries
        .is_empty());
    let c = FlameSandbox::from_yaml(
        "sandbox:\n  images:\n    registries:\n      localhost:5000: {}\n",
        &HashMap::new(),
    )
    .unwrap();
    assert!(c.images.registries["localhost:5000"].mirrors.is_empty());
}

#[test]
fn mirror_settings_are_loaded_without_prevalidating_endpoints_or_prefixes() {
    for endpoint in [
        "not a URL",
        "https://unreachable.invalid:5000/unknown/prefix",
        "http://127.0.0.1:1/bad:prefix",
        "ftp://mirror.test",
        "https://mirror.test/a/../b",
    ] {
        let text = format!("sandbox:\n  images:\n    registries:\n      docker.io:\n        mirrors:\n          - endpoint: '{endpoint}'\n            ca_file: missing.pem\n            username_env: NOT_SET\n");
        let config = FlameSandbox::from_yaml(&text, &HashMap::new()).unwrap();
        let mirror = &config.images.registries["docker.io"].mirrors[0];
        assert_eq!(mirror.endpoint, endpoint);
        assert_eq!(mirror.ca_file.as_deref(), Some(Path::new("missing.pem")));
        assert_eq!(mirror.username_env.as_deref(), Some("NOT_SET"));
    }
}

#[test]
fn defaults() {
    let c = FlameSandbox::from_map(&HashMap::new()).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/var/lib/flmbox"));
    assert_eq!(runsc(&c).path, "runsc");
    assert_eq!(runsc(&c).overlay, "root:memory");
    assert_eq!(runsc(&c).platform, None);
    assert_eq!(c.images.max_bytes, 4 << 30);

    let c = FlameSandbox::from_map(&map(&[(ENV_FLAME_HOME, "/opt/flame")])).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/opt/flame/data/flmbox"));
    let c = FlameSandbox::from_map(&map(&[
        (ENV_FLAME_HOME, "/opt/flame"),
        (ENV_STATE_DIR, "/x"),
    ]))
    .unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/x"));
}

const FULL: &str = "
sandbox:
  state_dir: /data/flmbox
  runtime:
    type: runsc
    path: /usr/bin/runsc
    platform: kvm
    overlay: all:dir=/var/tmp
  images:
    max_size: 2GiB
";

#[test]
fn the_file_sets_every_value() {
    let c = FlameSandbox::from_yaml(FULL, &HashMap::new()).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/data/flmbox"));
    assert_eq!(runsc(&c).path, "/usr/bin/runsc");
    assert_eq!(runsc(&c).platform.as_deref(), Some("kvm"));
    assert_eq!(runsc(&c).overlay, "all:dir=/var/tmp");
    assert_eq!(c.images.max_bytes, 2 << 30);
}

#[test]
fn every_key_is_optional() {
    for text in ["", "sandbox: {}", "sandbox:\n  runtime: {}\n  images: {}\n"] {
        let c = FlameSandbox::from_yaml(text, &HashMap::new()).unwrap();
        assert_eq!(runsc(&c).path, "runsc");
        assert_eq!(c.images.max_bytes, 4 << 30);
    }
}

#[test]
fn overlays_that_disable_root_isolation_are_rejected() {
    for overlay in [
        "none",
        "root:none",
        "all:none",
        "none,size=1g",
        "root:none,size=1g",
        "all:none,size=1g",
    ] {
        let err = FlameSandbox::from_yaml(
            &format!("sandbox:\n  runtime:\n    overlay: {overlay}\n"),
            &HashMap::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, FlameError::InvalidConfig(_)),
            "{overlay}: {err}"
        );
        assert!(err.to_string().contains("private root overlay"), "{err}");

        let err = FlameSandbox::from_yaml(FULL, &map(&[(ENV_OVERLAY, overlay)])).unwrap_err();
        assert!(
            matches!(err, FlameError::InvalidConfig(_)),
            "{overlay}: {err}"
        );
        assert!(err.to_string().contains(ENV_OVERLAY), "{err}");
    }
}

#[test]
fn private_overlay_modes_are_forwarded_to_runsc() {
    for overlay in [
        "root:memory",
        "root:self",
        "root:dir=/var/tmp",
        "all:memory",
        "all:self",
        "all:dir=/var/tmp",
        "root:memory,size=1g",
        "all:dir=/var/tmp,size=1g",
    ] {
        let c = FlameSandbox::from_map(&map(&[(ENV_OVERLAY, overlay)])).unwrap();
        assert_eq!(runsc(&c).overlay, overlay);
    }
    // Validation applies after precedence: a safe environment wins over
    // an unsafe file setting, just as for the other runtime properties.
    let c = FlameSandbox::from_yaml(
        "sandbox:\n  runtime:\n    overlay: none\n",
        &map(&[(ENV_OVERLAY, "root:memory")]),
    )
    .unwrap();
    assert_eq!(runsc(&c).overlay, "root:memory");
}

#[test]
fn the_environment_overrides_the_file() {
    let vars = map(&[
        (ENV_STATE_DIR, "/env/state"),
        (ENV_RUNSC, "/env/runsc"),
        (ENV_PLATFORM, "ptrace"),
        (ENV_OVERLAY, "root:self"),
        (ENV_MAX_IMAGE_BYTES, "1G"),
    ]);
    let c = FlameSandbox::from_yaml(FULL, &vars).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/env/state"));
    assert_eq!(runsc(&c).path, "/env/runsc");
    assert_eq!(runsc(&c).platform.as_deref(), Some("ptrace"));
    assert_eq!(runsc(&c).overlay, "root:self");
    assert_eq!(c.images.max_bytes, 1 << 30);

    let c = FlameSandbox::from_yaml(FULL, &map(&[(ENV_RUNSC, " ")])).unwrap();
    assert_eq!(runsc(&c).path, "/usr/bin/runsc");
}

#[test]
fn bad_files_are_invalid_config() {
    for text in [
        "sandbox:\n  runtime:\n    type: firecracker\n",
        "sandbox:\n  images:\n    max_size: lots\n",
        "sandbox:\n  images:\n    max_size: 0\n",
        "sandbox:\n  typo: 1\n",
        "cluster:\n  name: x\n",
        "sandbox: [1]",
        "not: [yaml",
    ] {
        let err = FlameSandbox::from_yaml(text, &HashMap::new()).unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)), "{text}: {err}");
    }
    let err = FlameSandbox::from_map(&map(&[(ENV_MAX_IMAGE_BYTES, "lots")])).unwrap_err();
    assert!(err.to_string().contains(ENV_MAX_IMAGE_BYTES));
}

#[test]
fn loads_the_file_it_is_pointed_at() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sandbox.yaml");
    fs::write(&file, FULL).unwrap();
    let c = FlameSandbox::load_with(Some(&file), &HashMap::new()).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/data/flmbox"));

    let via_env = map(&[(ENV_CONF, file.to_str().unwrap())]);
    let c = FlameSandbox::load_with(None, &via_env).unwrap();
    assert_eq!(runsc(&c).platform.as_deref(), Some("kvm"));

    let missing = dir.path().join("missing.yaml");
    assert!(FlameSandbox::load_with(Some(&missing), &HashMap::new()).is_err());
}

#[test]
fn the_default_file_is_used_when_present() {
    let home = tempfile::tempdir().unwrap();
    let vars = map(&[(ENV_HOME, home.path().to_str().unwrap())]);
    let c = FlameSandbox::load_with(None, &vars).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/var/lib/flmbox"));

    fs::create_dir_all(home.path().join(".flame")).unwrap();
    fs::write(home.path().join(".flame").join(DEFAULT_CONF), FULL).unwrap();
    let c = FlameSandbox::load_with(None, &vars).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/data/flmbox"));

    // $FLAME_HOME/conf comes first.
    let flame = tempfile::tempdir().unwrap();
    fs::create_dir_all(flame.path().join("conf")).unwrap();
    fs::write(
        flame.path().join("conf").join(DEFAULT_CONF),
        "sandbox:\n  state_dir: /from/flame/home\n",
    )
    .unwrap();
    let vars = map(&[
        (ENV_HOME, home.path().to_str().unwrap()),
        (ENV_FLAME_HOME, flame.path().to_str().unwrap()),
    ]);
    let c = FlameSandbox::load_with(None, &vars).unwrap();
    assert_eq!(c.state_dir, PathBuf::from("/from/flame/home"));
}

#[test]
fn parses_sizes() {
    assert_eq!(parse_size("1024").unwrap(), 1024);
    assert_eq!(parse_size("1k").unwrap(), 1024);
    assert_eq!(parse_size("1KiB").unwrap(), 1024);
    assert_eq!(parse_size("512M").unwrap(), 512 << 20);
    assert_eq!(parse_size("4gb").unwrap(), 4 << 30);
    assert_eq!(parse_size("1T").unwrap(), 1 << 40);
    assert!(parse_size("").is_err());
    assert!(parse_size("b").is_err());
}

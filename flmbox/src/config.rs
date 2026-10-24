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

//! Configuration of `flmbox`: the `sandbox:` section of a YAML file, validated
//! into [`FlameSandbox`], the way `FlameCluster` is for the cluster.
//!
//! ```yaml
//! sandbox:
//!   state_dir: /var/lib/flmbox    # image cache and sandbox state
//!   runtime:
//!     type: runsc                 # the only runtime so far
//!     path: runsc
//!     platform: kvm               # optional, runsc --platform
//!     overlay: root:memory        # runsc --overlay2
//!   images:
//!     max_size: 4G                # largest image, packed and unpacked
//! ```
//!
//! Every key is optional. Environment variables override the file, and the
//! file overrides the defaults.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use flame_rs::apis::FlameError;
use serde_derive::{Deserialize, Serialize};

/// Path of the configuration file; an explicit path must exist.
pub const ENV_CONF: &str = "FLMBOX_CONF";
/// Directory holding the node-local image cache and sandbox state.
pub const ENV_STATE_DIR: &str = "FLMBOX_STATE_DIR";
/// Path of the `runsc` binary.
pub const ENV_RUNSC: &str = "FLMBOX_RUNSC";
/// `runsc --overlay2` value, default `root:memory`.
pub const ENV_OVERLAY: &str = "FLMBOX_OVERLAY";
/// `runsc --platform` value, unset by default.
pub const ENV_PLATFORM: &str = "FLMBOX_PLATFORM";
/// Maximum size of an image in bytes.
pub const ENV_MAX_IMAGE_BYTES: &str = "FLMBOX_MAX_IMAGE_BYTES";
/// Flame home directory, used for the default state directory and file.
pub const ENV_FLAME_HOME: &str = "FLAME_HOME";
const ENV_HOME: &str = "HOME";

const DEFAULT_CONF: &str = "flame-sandbox.yaml";
const DEFAULT_STATE_DIR: &str = "/var/lib/flmbox";
const DEFAULT_RUNSC: &str = "runsc";
const DEFAULT_OVERLAY: &str = "root:memory";
const DEFAULT_MAX_IMAGE_BYTES: u64 = 4 << 30;

fn invalid(msg: impl Into<String>) -> FlameError {
    FlameError::InvalidConfig(msg.into())
}

// ============================================================
// YAML layer
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlameSandboxContextYaml {
    sandbox: FlameSandboxYaml,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlameSandboxYaml {
    state_dir: Option<String>,
    runtime: Option<FlameRuntimeYaml>,
    images: Option<FlameImagesYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlameRuntimeYaml {
    /// The runtime: `runsc` (the default).
    #[serde(rename = "type")]
    kind: Option<String>,
    path: Option<String>,
    platform: Option<String>,
    overlay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlameImagesYaml {
    /// Size with units: `4G`, `512M`.
    max_size: Option<String>,
}

// ============================================================
// Runtime structs (validated)
// ============================================================

/// Configuration of `flmbox`.
#[derive(Debug, Clone)]
pub struct FlameSandbox {
    /// Root of the persistent state; the image cache lives in `images/`.
    pub state_dir: PathBuf,
    /// The sandbox runtime and its settings.
    pub runtime: FlameRuntime,
    /// Image limits.
    pub images: FlameImages,
}

/// The sandbox runtime. Another runtime, such as Firecracker, is another variant.
#[derive(Debug, Clone)]
pub enum FlameRuntime {
    /// gVisor.
    Runsc(FlameRunsc),
}

/// Settings of the gVisor runtime.
#[derive(Debug, Clone)]
pub struct FlameRunsc {
    /// Path or name of the `runsc` binary.
    pub path: String,
    /// Value of `runsc --platform`, when set.
    pub platform: Option<String>,
    /// Value of `runsc --overlay2` (default `root:memory`: writes stay in sandbox memory).
    pub overlay: String,
}

/// Image limits.
#[derive(Debug, Clone)]
pub struct FlameImages {
    /// Maximum image size in bytes.
    pub max_bytes: u64,
}

impl FlameSandbox {
    /// Loads the configuration of this process: the file named by `path`
    /// (it must exist), else `$FLMBOX_CONF` (it must exist), else
    /// `$FLAME_HOME/conf/flame-sandbox.yaml` or `~/.flame/flame-sandbox.yaml`
    /// when present, else the defaults; then the environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, FlameError> {
        let vars = [
            ENV_CONF,
            ENV_STATE_DIR,
            ENV_RUNSC,
            ENV_OVERLAY,
            ENV_PLATFORM,
            ENV_MAX_IMAGE_BYTES,
            ENV_FLAME_HOME,
            ENV_HOME,
        ]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
        Self::load_with(path, &vars)
    }

    fn load_with(path: Option<&Path>, vars: &HashMap<String, String>) -> Result<Self, FlameError> {
        let get = |key: &str| vars.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
        let explicit = path
            .map(Path::to_path_buf)
            .or_else(|| get(ENV_CONF).map(PathBuf::from));
        let file = match explicit {
            Some(path) => Some(path),
            None => [
                get(ENV_FLAME_HOME).map(|h| Path::new(h).join("conf").join(DEFAULT_CONF)),
                get(ENV_HOME).map(|h| Path::new(h).join(".flame").join(DEFAULT_CONF)),
            ]
            .into_iter()
            .flatten()
            .find(|p| p.is_file()),
        };
        let yaml = match file {
            Some(file) => Self::read_yaml(&file)?,
            None => FlameSandboxYaml::default(),
        };
        Self::build(yaml, vars)
    }

    fn read_yaml(file: &Path) -> Result<FlameSandboxYaml, FlameError> {
        if !file.is_file() {
            return Err(invalid(format!("<{}> is not a file", file.display())));
        }
        let contents = fs::read_to_string(file)
            .map_err(|e| FlameError::Internal(format!("read {}: {e}", file.display())))?;
        let yaml =
            parse_yaml(&contents).map_err(|e| invalid(format!("{}: {e}", file.display())))?;
        tracing::debug!("Load FlameSandbox from <{}>: {yaml:?}", file.display());
        Ok(yaml)
    }

    /// Builds the configuration from YAML text and the variables of the
    /// environment (see the `ENV_*` constants); the variables win.
    #[cfg(test)]
    fn from_yaml(text: &str, vars: &HashMap<String, String>) -> Result<Self, FlameError> {
        Self::build(parse_yaml(text).map_err(invalid)?, vars)
    }

    /// The defaults with the overrides of `vars` (see the `ENV_*` constants).
    #[cfg(test)]
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, FlameError> {
        Self::build(FlameSandboxYaml::default(), vars)
    }

    fn build(yaml: FlameSandboxYaml, vars: &HashMap<String, String>) -> Result<Self, FlameError> {
        let get = |key: &str| vars.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
        let filled = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());

        let state_dir = match (
            get(ENV_STATE_DIR),
            filled(yaml.state_dir),
            get(ENV_FLAME_HOME),
        ) {
            (Some(dir), _, _) => PathBuf::from(dir),
            (None, Some(dir), _) => PathBuf::from(dir),
            (None, None, Some(home)) => PathBuf::from(home).join("data").join("flmbox"),
            (None, None, None) => PathBuf::from(DEFAULT_STATE_DIR),
        };

        let runtime = yaml.runtime.unwrap_or(FlameRuntimeYaml {
            kind: None,
            path: None,
            platform: None,
            overlay: None,
        });
        let runtime = match filled(runtime.kind).as_deref().unwrap_or("runsc") {
            "runsc" => FlameRuntime::Runsc(FlameRunsc {
                path: get(ENV_RUNSC)
                    .map(str::to_string)
                    .or(filled(runtime.path))
                    .unwrap_or_else(|| DEFAULT_RUNSC.to_string()),
                platform: get(ENV_PLATFORM)
                    .map(str::to_string)
                    .or(filled(runtime.platform)),
                overlay: get(ENV_OVERLAY)
                    .map(str::to_string)
                    .or(filled(runtime.overlay))
                    .unwrap_or_else(|| DEFAULT_OVERLAY.to_string()),
            }),
            other => {
                return Err(invalid(format!(
                    "sandbox.runtime.type: '{other}' is not a known runtime (use runsc)"
                )))
            }
        };

        let max_bytes =
            match (get(ENV_MAX_IMAGE_BYTES).map(|v| (ENV_MAX_IMAGE_BYTES, v.to_string()))).or(yaml
                .images
                .and_then(|i| filled(i.max_size))
                .map(|v| ("sandbox.images.max_size", v)))
            {
                Some((key, v)) => parse_size(&v).map_err(|why| invalid(format!("{key}: {why}")))?,
                None => DEFAULT_MAX_IMAGE_BYTES,
            };

        Ok(Self {
            state_dir,
            runtime,
            images: FlameImages { max_bytes },
        })
    }
}

fn parse_yaml(text: &str) -> Result<FlameSandboxYaml, String> {
    if text.trim().is_empty() {
        return Ok(FlameSandboxYaml::default());
    }
    serde_yaml::from_str::<FlameSandboxContextYaml>(text)
        .map(|ctx| ctx.sandbox)
        .map_err(|e| e.to_string())
}

/// Parses a byte count with an optional `k`/`m`/`g`/`t` (1024 based) suffix,
/// optionally followed by `b` or `ib`.
fn parse_size(value: &str) -> Result<u64, String> {
    let lower = value.trim().to_ascii_lowercase();
    let digits = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower);
    let (number, shift) = match digits.chars().last() {
        Some('k') => (&digits[..digits.len() - 1], 10),
        Some('m') => (&digits[..digits.len() - 1], 20),
        Some('g') => (&digits[..digits.len() - 1], 30),
        Some('t') => (&digits[..digits.len() - 1], 40),
        _ => (digits, 0),
    };
    let bad = || format!("'{value}' is not a valid size");
    let n: u64 = number.trim().parse().map_err(|_| bad())?;
    let bytes = n.checked_mul(1 << shift).ok_or_else(bad)?;
    if bytes == 0 {
        return Err(bad());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
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
}

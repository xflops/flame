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
//!   network: none                # none (default) or host, see `FlameNetwork`;
//!                                #   `--network` and $FLMBOX_NETWORK override it
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
/// Sandbox network mode: `none` (default) or `host`.
pub const ENV_NETWORK: &str = "FLMBOX_NETWORK";
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
    network: Option<String>,
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
    #[serde(default)]
    registries: HashMap<String, FlameRegistry>,
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
    /// The network of the sandboxes.
    pub network: FlameNetwork,
    /// Image limits.
    pub images: FlameImages,
}

/// The network of a sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FlameNetwork {
    /// No interface: the sandbox cannot reach the control plane, the node or the
    /// internet. The default.
    #[default]
    None,
    /// The sandbox uses the network of the process that runs `flmbox`, so it has
    /// every egress that process has: the internet, but also the Flame control plane,
    /// the node's own services, other pods and the cloud metadata address. Use it only
    /// on clusters where the code that sandboxes run is trusted.
    Host,
}

impl std::str::FromStr for FlameNetwork {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "host" => Ok(Self::Host),
            other => Err(format!(
                "'{other}' is not a known network mode (use none or host)"
            )),
        }
    }
}

impl FlameNetwork {
    fn parse(key: &str, value: &str) -> Result<Self, FlameError> {
        value
            .parse()
            .map_err(|why| invalid(format!("{key}: {why}")))
    }
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
    /// Ordered transport mirrors keyed by the canonical upstream registry.
    pub registries: HashMap<String, FlameRegistry>,
}

/// Transport policy for one canonical upstream registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlameRegistry {
    /// Tried in order, before the upstream.
    #[serde(default)]
    pub mirrors: Vec<FlameRegistryMirror>,
}

/// A registry endpoint and optional repository prefix, without embedded credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlameRegistryMirror {
    pub endpoint: String,
    /// Additional PEM CA trust for this endpoint; normal TLS verification stays enabled.
    pub ca_file: Option<PathBuf>,
    /// Environment variable names holding this endpoint's Basic credentials.
    pub username_env: Option<String>,
    pub password_env: Option<String>,
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
            ENV_NETWORK,
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
        tracing::debug!("Load FlameSandbox from <{}>", file.display());
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
                    .map_or_else(|| Ok(DEFAULT_OVERLAY.to_string()), private_overlay)?,
            }),
            other => {
                return Err(invalid(format!(
                    "sandbox.runtime.type: '{other}' is not a known runtime (use runsc)"
                )))
            }
        };

        let images = yaml.images;
        let registries = images
            .as_ref()
            .map(|i| i.registries.clone())
            .unwrap_or_default();
        let max_bytes = match (get(ENV_MAX_IMAGE_BYTES)
            .map(|v| (ENV_MAX_IMAGE_BYTES, v.to_string())))
        .or(images
            .and_then(|i| filled(i.max_size))
            .map(|v| ("sandbox.images.max_size", v)))
        {
            Some((key, v)) => parse_size(&v).map_err(|why| invalid(format!("{key}: {why}")))?,
            None => DEFAULT_MAX_IMAGE_BYTES,
        };

        let network = match (get(ENV_NETWORK).map(|v| (ENV_NETWORK, v.to_string())))
            .or(filled(yaml.network).map(|v| ("sandbox.network", v)))
        {
            Some((key, v)) => FlameNetwork::parse(key, &v)?,
            None => FlameNetwork::None,
        };

        Ok(Self {
            state_dir,
            runtime,
            network,
            images: FlameImages {
                max_bytes,
                registries,
            },
        })
    }
}

/// A writable OCI root must never expose the shared image cache directly.
/// Leave the rest of runsc's overlay grammar to runsc itself.
fn private_overlay(value: String) -> Result<String, FlameError> {
    let mode = value.split(',').next().unwrap_or_default().trim();
    if matches!(mode, "none" | "root:none" | "all:none") {
        return Err(invalid(format!(
            "sandbox.runtime.overlay / {ENV_OVERLAY}: '{value}' disables the private root overlay; use root:memory, root:self or root:dir=/absolute/path"
        )));
    }
    Ok(value)
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
#[path = "tests/config.rs"]
mod tests;

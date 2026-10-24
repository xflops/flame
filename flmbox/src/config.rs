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

//! Configuration of the `flmbox` service and OCI image reference handling.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use flame_rs::apis::FlameError;

use crate::network::{parse_target, Target};

/// Directory holding the node-local image cache and sandbox state.
pub const ENV_STATE_DIR: &str = "FLMBOX_STATE_DIR";
/// Comma-separated image allowlist.
pub const ENV_IMAGES: &str = "FLMBOX_IMAGES";
/// Path of the `runsc` binary.
pub const ENV_RUNSC: &str = "FLMBOX_RUNSC";
/// `runsc --overlay2` value, default `root:memory`.
pub const ENV_OVERLAY: &str = "FLMBOX_OVERLAY";
/// `runsc --platform` value, unset by default.
pub const ENV_PLATFORM: &str = "FLMBOX_PLATFORM";
/// Network mode, `sandbox` or `none`.
pub const ENV_NETWORK: &str = "FLMBOX_NETWORK";
/// Comma-separated egress allowlist (CIDRs or hosts).
pub const ENV_EGRESS: &str = "FLMBOX_EGRESS";
/// Comma-separated extra CIDRs or hosts sandboxes can never reach.
pub const ENV_DENY: &str = "FLMBOX_DENY";
/// Comma-separated IPv4 addresses of the DNS servers written to the sandbox's `/etc/resolv.conf`.
pub const ENV_DNS: &str = "FLMBOX_DNS";
/// Name or path of the `ip` binary.
pub const ENV_IP_BIN: &str = "FLMBOX_IP_BIN";
/// Name or path of the `nft` binary.
pub const ENV_NFT_BIN: &str = "FLMBOX_NFT_BIN";
/// Maximum size of an image in bytes.
pub const ENV_MAX_IMAGE_BYTES: &str = "FLMBOX_MAX_IMAGE_BYTES";
/// Flame home directory, used for the default state directory.
pub const ENV_FLAME_HOME: &str = "FLAME_HOME";

/// Registry host used for references without an explicit registry.
pub const DOCKER_HUB_REGISTRY: &str = "registry-1.docker.io";

const DEFAULT_IMAGES: &str = "ubuntu:26.04";
const DEFAULT_DNS: &str = "1.1.1.1,8.8.8.8";
const DEFAULT_RUNSC: &str = "runsc";
const DEFAULT_OVERLAY: &str = "root:memory";
const DEFAULT_MAX_IMAGE_BYTES: u64 = 4 << 30;
const MAX_TAG_LEN: usize = 128;
const MAX_REPOSITORY_LEN: usize = 255;

fn invalid(msg: impl Into<String>) -> FlameError {
    FlameError::InvalidConfig(msg.into())
}

/// A normalized reference to an image in a registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageRef {
    /// Registry host with optional port, e.g. `registry-1.docker.io`.
    pub registry: String,
    /// Repository path, e.g. `library/ubuntu`.
    pub repository: String,
    /// Tag; `latest` when neither a tag nor a digest was given.
    pub tag: Option<String>,
    /// Content digest, `sha256:<64 hex>`.
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parses and normalizes a reference such as `ubuntu:26.04`,
    /// `ghcr.io/org/img:tag`, `localhost:5000/img` or `name@sha256:<hex>`.
    pub fn parse(input: &str) -> Result<Self, FlameError> {
        let bad = |why: &str| invalid(format!("invalid image reference '{input}': {why}"));

        if input.is_empty() {
            return Err(bad("empty reference"));
        }
        if input.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad("contains whitespace or control characters"));
        }
        if input.contains("..") {
            return Err(bad("contains '..'"));
        }

        let (name, digest) = match input.split_once('@') {
            Some((name, digest)) => {
                validate_digest(digest).map_err(|why| bad(&why))?;
                (name, Some(digest.to_string()))
            }
            None => (input, None),
        };

        let (registry, path) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first, rest)
            }
            _ => (DOCKER_HUB_REGISTRY, name),
        };
        let registry = registry.to_ascii_lowercase();
        validate_registry(&registry).map_err(|why| bad(&why))?;
        let hub = matches!(
            registry.as_str(),
            DOCKER_HUB_REGISTRY | "docker.io" | "index.docker.io"
        );
        let registry = if hub {
            DOCKER_HUB_REGISTRY.to_string()
        } else {
            registry
        };

        let (repository, tag) = match path.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo, Some(tag)),
            _ => (path, None),
        };
        if let Some(tag) = tag {
            validate_tag(tag).map_err(|why| bad(&why))?;
        }
        let repository = if hub && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository.to_string()
        };
        validate_repository(&repository).map_err(|why| bad(&why))?;

        let tag = match (tag, &digest) {
            (Some(tag), _) => Some(tag.to_string()),
            (None, None) => Some("latest".to_string()),
            (None, Some(_)) => None,
        };

        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Whether the registry is a loopback address, which is reached over plain http.
    pub fn is_loopback(&self) -> bool {
        let host = if let Some(rest) = self.registry.strip_prefix('[') {
            rest.split(']').next().unwrap_or(rest)
        } else {
            self.registry.split(':').next().unwrap_or(&self.registry)
        };
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }

    /// Base URL of the registry, `http` for loopback registries and `https` otherwise.
    pub fn base_url(&self) -> String {
        let scheme = if self.is_loopback() { "http" } else { "https" };
        format!("{scheme}://{}", self.registry)
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

fn validate_digest(digest: &str) -> Result<(), String> {
    match digest.strip_prefix("sha256:") {
        Some(hex)
            if hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(())
        }
        _ => Err("digest must be sha256:<64 lowercase hex characters>".to_string()),
    }
}

fn validate_registry(registry: &str) -> Result<(), String> {
    if registry.is_empty() {
        return Err("empty registry".to_string());
    }
    if !registry
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b))
    {
        return Err(format!("invalid registry '{registry}'"));
    }
    if !registry.starts_with('[') {
        if let Some((host, port)) = registry.split_once(':') {
            if host.is_empty() || port.parse::<u16>().is_err() {
                return Err(format!("invalid registry '{registry}'"));
            }
        }
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<(), String> {
    let mut bytes = tag.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !first_ok
        || tag.len() > MAX_TAG_LEN
        || !tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(format!("invalid tag '{tag}'"));
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<(), String> {
    if repository.is_empty() || repository.len() > MAX_REPOSITORY_LEN {
        return Err("invalid repository length".to_string());
    }
    if repository.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(format!("repository '{repository}' must be lowercase"));
    }
    for component in repository.split('/') {
        validate_component(component)
            .map_err(|_| format!("invalid repository component '{component}'"))?;
    }
    Ok(())
}

/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*` per the distribution spec.
fn validate_component(component: &str) -> Result<(), ()> {
    let bytes = component.as_bytes();
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !bytes.first().is_some_and(alnum) || !bytes.last().is_some_and(alnum) {
        return Err(());
    }
    let mut run_start = None;
    for (i, b) in bytes.iter().enumerate() {
        if alnum(b) {
            if let Some(start) = run_start.take() {
                let sep = &component[start..i];
                if !(sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|b| b == b'-')) {
                    return Err(());
                }
            }
        } else if b"._-".contains(b) {
            run_start.get_or_insert(i);
        } else {
            return Err(());
        }
    }
    Ok(())
}

/// The set of images the service may pull.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: Vec<ImageRef>,
}

impl Allowlist {
    /// Creates an allowlist from parsed entries.
    pub fn new(entries: Vec<ImageRef>) -> Self {
        Self { entries }
    }

    /// Parses a comma-separated list of entries: `name:tag`, `name` (= `:latest`)
    /// or `name@sha256:<hex>`.
    pub fn parse(list: &str) -> Result<Self, FlameError> {
        let entries = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ImageRef::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { entries })
    }

    /// The entries of the allowlist.
    pub fn entries(&self) -> &[ImageRef] {
        &self.entries
    }

    /// Checks that `image` is allowed. An entry matches when registry and
    /// repository are equal and either both carry the same digest or neither
    /// carries a digest and the tags are equal. A reference with a digest only
    /// matches a digest entry, and a digest entry never matches a tag reference.
    pub fn check(&self, image: &ImageRef) -> Result<(), FlameError> {
        let allowed = self.entries.iter().any(|e| {
            e.registry == image.registry
                && e.repository == image.repository
                && match (&e.digest, &image.digest) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => e.tag == image.tag,
                    _ => false,
                }
        });
        if allowed {
            return Ok(());
        }
        let list = self
            .entries
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        Err(invalid(format!(
            "image '{image}' is not allowed (allowed: [{list}])"
        )))
    }
}

/// Network mode of sandboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    /// Sandboxes get a network namespace with filtered egress.
    Sandbox,
    /// Sandboxes have no network.
    None,
}

/// Configuration of the `flmbox` service.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root of the persistent state; the image cache lives in `images/`.
    pub state_dir: PathBuf,
    /// Images that may be pulled.
    pub images: Allowlist,
    /// Path or name of the `runsc` binary.
    pub runsc: String,
    /// Network mode.
    pub network: NetworkMode,
    /// Value of `runsc --overlay2` (default `root:memory`: writes stay in sandbox memory).
    pub overlay: String,
    /// Value of `runsc --platform`, when set.
    pub platform: Option<String>,
    /// Egress allowlist entries (CIDRs or host names).
    pub egress: Vec<String>,
    /// Extra destinations (CIDRs or host names) sandboxes can never reach.
    pub deny: Vec<String>,
    /// DNS servers written to the sandbox's `/etc/resolv.conf` (IPv4 addresses).
    pub dns: Vec<String>,
    /// Name or path of the `ip` binary.
    pub ip_bin: String,
    /// Name or path of the `nft` binary.
    pub nft_bin: String,
    /// Maximum image size in bytes.
    pub max_image_bytes: u64,
}

impl Config {
    /// Builds the configuration from the process environment.
    pub fn from_env() -> Result<Self, FlameError> {
        let map = [
            ENV_STATE_DIR,
            ENV_IMAGES,
            ENV_RUNSC,
            ENV_NETWORK,
            ENV_OVERLAY,
            ENV_PLATFORM,
            ENV_EGRESS,
            ENV_DENY,
            ENV_DNS,
            ENV_IP_BIN,
            ENV_NFT_BIN,
            ENV_MAX_IMAGE_BYTES,
            ENV_FLAME_HOME,
        ]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
        Self::from_map(&map)
    }

    /// Builds the configuration from a variable map (see the `ENV_*` constants).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, FlameError> {
        let get = |key: &str| vars.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());

        let state_dir = match (get(ENV_STATE_DIR), get(ENV_FLAME_HOME)) {
            (Some(dir), _) => PathBuf::from(dir),
            (None, Some(home)) => PathBuf::from(home).join("data").join("flmbox"),
            (None, None) => PathBuf::from("/var/lib/flmbox"),
        };

        let images = Allowlist::parse(get(ENV_IMAGES).unwrap_or(DEFAULT_IMAGES))
            .map_err(|e| invalid(format!("{ENV_IMAGES}: {e}")))?;
        if images.entries().is_empty() {
            return Err(invalid(format!("{ENV_IMAGES}: no images listed")));
        }

        let network = match get(ENV_NETWORK).unwrap_or("sandbox") {
            "sandbox" => NetworkMode::Sandbox,
            "none" => NetworkMode::None,
            other => {
                return Err(invalid(format!(
                    "{ENV_NETWORK}: '{other}' is not 'sandbox' or 'none'"
                )))
            }
        };

        let list = |key: &str, allow_any: bool| {
            get(key)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|entry| {
                    match parse_target(entry) {
                        Ok(Target::Any) if !allow_any => Err("'*' is not allowed".to_string()),
                        Ok(_) => Ok(()),
                        Err(why) => Err(why),
                    }
                    .map_err(|why| invalid(format!("{key}: {why}")))?;
                    Ok(entry.to_string())
                })
                .collect::<Result<Vec<_>, FlameError>>()
        };
        let egress = list(ENV_EGRESS, true)?;
        let deny = list(ENV_DENY, false)?;

        let dns = get(ENV_DNS)
            .unwrap_or(DEFAULT_DNS)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|entry| {
                entry
                    .parse::<Ipv4Addr>()
                    .map(|_| entry.to_string())
                    .map_err(|_| invalid(format!("{ENV_DNS}: '{entry}' is not an IPv4 address")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if dns.is_empty() {
            return Err(invalid(format!("{ENV_DNS}: no addresses listed")));
        }

        let max_image_bytes = match get(ENV_MAX_IMAGE_BYTES) {
            Some(v) => {
                parse_size(v).map_err(|why| invalid(format!("{ENV_MAX_IMAGE_BYTES}: {why}")))?
            }
            None => DEFAULT_MAX_IMAGE_BYTES,
        };

        Ok(Self {
            state_dir,
            images,
            runsc: get(ENV_RUNSC).unwrap_or(DEFAULT_RUNSC).to_string(),
            network,
            overlay: get(ENV_OVERLAY).unwrap_or(DEFAULT_OVERLAY).to_string(),
            platform: get(ENV_PLATFORM).map(str::to_string),
            egress,
            deny,
            dns,
            ip_bin: get(ENV_IP_BIN).unwrap_or("ip").to_string(),
            nft_bin: get(ENV_NFT_BIN).unwrap_or("nft").to_string(),
            max_image_bytes,
        })
    }
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

    fn r(s: &str) -> ImageRef {
        ImageRef::parse(s).unwrap()
    }

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_docker_hub_refs() {
        let i = r("ubuntu:26.04");
        assert_eq!(i.registry, DOCKER_HUB_REGISTRY);
        assert_eq!(i.repository, "library/ubuntu");
        assert_eq!(i.tag.as_deref(), Some("26.04"));
        assert_eq!(i.digest, None);
        assert_eq!(i.base_url(), "https://registry-1.docker.io");

        assert_eq!(r("ubuntu").tag.as_deref(), Some("latest"));
        assert_eq!(r("user/repo:tag").repository, "user/repo");
        assert_eq!(
            r("docker.io/library/alpine:3").registry,
            DOCKER_HUB_REGISTRY
        );
        assert_eq!(r("docker.io/alpine:3").repository, "library/alpine");
    }

    #[test]
    fn parses_other_registries() {
        let i = r("ghcr.io/org/img:tag");
        assert_eq!(i.registry, "ghcr.io");
        assert_eq!(i.repository, "org/img");
        assert_eq!(i.base_url(), "https://ghcr.io");

        let i = r("localhost:5000/img:tag");
        assert_eq!(i.registry, "localhost:5000");
        assert_eq!(i.repository, "img");
        assert_eq!(i.base_url(), "http://localhost:5000");

        let i = r("127.0.0.1:8080/a/b");
        assert!(i.is_loopback());
        assert_eq!(i.base_url(), "http://127.0.0.1:8080");
        assert!(r("[::1]:5000/img").is_loopback());
        assert!(!r("ghcr.io/img").is_loopback());
    }

    #[test]
    fn parses_digest_refs() {
        let i = r(&format!("ubuntu@sha256:{HEX}"));
        assert_eq!(i.tag, None);
        assert_eq!(i.digest.as_deref(), Some(format!("sha256:{HEX}").as_str()));
        let i = r(&format!("ubuntu:26.04@sha256:{HEX}"));
        assert_eq!(i.tag.as_deref(), Some("26.04"));
        assert_eq!(
            r(&format!("localhost:5000/x@sha256:{HEX}")).to_string(),
            format!("localhost:5000/x@sha256:{HEX}")
        );
    }

    #[test]
    fn rejects_malformed_refs() {
        for bad in [
            "",
            " ubuntu",
            "ubuntu :1",
            "ubuntu\n",
            "a/../b",
            "ubuntu:",
            "Ubuntu:1",
            "user/Repo:1",
            "ubuntu@sha256:abc",
            "ubuntu@md5:abc",
            &format!("ubuntu@sha256:{}", HEX.to_uppercase()),
            "ubuntu:bad tag",
            "ubuntu:-x",
            "a//b",
            "/ubuntu",
            "ubuntu/",
            "a-/b",
            "a___b",
            "a.-b",
            "host:notaport/img",
            "ghcr.io/",
            "ghcr.io/a:b:c",
        ] {
            assert!(
                matches!(ImageRef::parse(bad), Err(FlameError::InvalidConfig(_))),
                "'{bad}' should be rejected"
            );
        }
    }

    #[test]
    fn accepts_valid_separators() {
        assert!(ImageRef::parse("a.b/c_d/e__f/g--h:t").is_ok());
    }

    #[test]
    fn allowlist_matches_tags_and_digests() {
        let list = Allowlist::parse(&format!(
            "ubuntu:26.04, alpine, ghcr.io/o/i@sha256:{HEX}, localhost:5000/x:1"
        ))
        .unwrap();
        assert!(list.check(&r("ubuntu:26.04")).is_ok());
        assert!(list.check(&r("alpine")).is_ok());
        assert!(list.check(&r("alpine:latest")).is_ok());
        assert!(list.check(&r("localhost:5000/x:1")).is_ok());
        assert!(list.check(&r(&format!("ghcr.io/o/i@sha256:{HEX}"))).is_ok());

        assert!(list.check(&r("ubuntu:24.04")).is_err());
        assert!(list.check(&r("alpine:3")).is_err());
        assert!(list.check(&r("ghcr.io/o/i:latest")).is_err());
        assert!(list.check(&r("localhost:5001/x:1")).is_err());
        assert!(list.check(&r("other/ubuntu:26.04")).is_err());
        // A digest reference needs a digest entry, even if the tag is allowed.
        assert!(list
            .check(&r(&format!("ubuntu:26.04@sha256:{HEX}")))
            .is_err());
        let other = "f".repeat(64);
        assert!(list
            .check(&r(&format!("ghcr.io/o/i@sha256:{other}")))
            .is_err());
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn overlay_and_platform_from_env() {
        let c = Config::from_map(&map(&[
            (ENV_OVERLAY, "all:dir=/var/tmp"),
            (ENV_PLATFORM, "kvm"),
        ]))
        .unwrap();
        assert_eq!(c.overlay, "all:dir=/var/tmp");
        assert_eq!(c.platform.as_deref(), Some("kvm"));

        let c = Config::from_map(&map(&[(ENV_OVERLAY, " "), (ENV_PLATFORM, "")])).unwrap();
        assert_eq!(c.overlay, "root:memory");
        assert_eq!(c.platform, None);
    }

    #[test]
    fn config_defaults() {
        let c = Config::from_map(&HashMap::new()).unwrap();
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/flmbox"));
        assert_eq!(c.images.entries(), &[r("ubuntu:26.04")]);
        assert_eq!(c.runsc, "runsc");
        assert_eq!(c.network, NetworkMode::Sandbox);
        assert_eq!(c.overlay, "root:memory");
        assert_eq!(c.platform, None);
        assert!(c.egress.is_empty());
        assert!(c.deny.is_empty());
        assert_eq!(c.dns, ["1.1.1.1", "8.8.8.8"]);
        assert_eq!((c.ip_bin.as_str(), c.nft_bin.as_str()), ("ip", "nft"));
        assert_eq!(c.max_image_bytes, 4 << 30);

        let c = Config::from_map(&map(&[(ENV_FLAME_HOME, "/opt/flame")])).unwrap();
        assert_eq!(c.state_dir, PathBuf::from("/opt/flame/data/flmbox"));
        let c = Config::from_map(&map(&[
            (ENV_FLAME_HOME, "/opt/flame"),
            (ENV_STATE_DIR, "/x"),
        ]))
        .unwrap();
        assert_eq!(c.state_dir, PathBuf::from("/x"));
    }

    #[test]
    fn config_overrides() {
        let c = Config::from_map(&map(&[
            (ENV_IMAGES, "alpine:3, ubuntu"),
            (ENV_RUNSC, "/usr/bin/runsc"),
            (ENV_NETWORK, "none"),
            (ENV_EGRESS, "10.0.0.0/8, example.com ,2001:db8::/32"),
            (ENV_DENY, "192.168.0.0/16, internal.example.com"),
            (ENV_DNS, "9.9.9.9, 10.0.0.2"),
            (ENV_IP_BIN, "/sbin/ip"),
            (ENV_NFT_BIN, "/usr/sbin/nft"),
            (ENV_MAX_IMAGE_BYTES, "2GiB"),
        ]))
        .unwrap();
        assert_eq!(c.dns, ["9.9.9.9", "10.0.0.2"]);
        assert_eq!(c.images.entries().len(), 2);
        assert_eq!(c.runsc, "/usr/bin/runsc");
        assert_eq!(c.network, NetworkMode::None);
        assert_eq!(c.egress, ["10.0.0.0/8", "example.com", "2001:db8::/32"]);
        assert_eq!(c.deny, ["192.168.0.0/16", "internal.example.com"]);
        assert_eq!(
            (c.ip_bin.as_str(), c.nft_bin.as_str()),
            ("/sbin/ip", "/usr/sbin/nft")
        );
        assert_eq!(c.max_image_bytes, 2 << 30);
    }

    #[test]
    fn config_rejects_bad_values() {
        for (k, v) in [
            (ENV_IMAGES, "Bad Image"),
            (ENV_IMAGES, ","),
            (ENV_NETWORK, "host"),
            (ENV_EGRESS, "10.0.0.0/33"),
            (ENV_EGRESS, "bad host!"),
            (ENV_EGRESS, "300.1.1.1/8"),
            (ENV_EGRESS, "1.2.3.4\"; drop"),
            (ENV_DNS, "dns.example.com"),
            (ENV_DNS, "2001:4860:4860::8888"),
            (ENV_DNS, "1.1.1.1\nnameserver 6.6.6.6"),
            (ENV_DNS, "1.1.1.256"),
            (ENV_DNS, ","),
            (ENV_DENY, "*"),
            (ENV_DENY, "10.0.0.0/8; flush ruleset"),
            (ENV_MAX_IMAGE_BYTES, "lots"),
            (ENV_MAX_IMAGE_BYTES, "0"),
            (ENV_MAX_IMAGE_BYTES, "99999999999T"),
        ] {
            let err = Config::from_map(&map(&[(k, v)])).unwrap_err();
            assert!(matches!(err, FlameError::InvalidConfig(_)), "{k}={v}");
        }
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

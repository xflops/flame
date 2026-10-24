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

//! Network setup seam. The sandbox lifecycle only depends on the [`Network`]
//! trait; [`IpNetwork`] gives every sandbox a network namespace joined to the
//! host by a veth pair on its own /30, with a per-sandbox nftables table that
//! filters its traffic. It only runs `ip`, `nft` and `sysctl` through a
//! [`CommandRunner`], so it compiles everywhere but only works on Linux.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use flame_rs::apis::FlameError;

use crate::config::{Config, NetworkMode};
use crate::runner::{CommandRunner, CommandSpec};

/// Prepares and removes the network of a sandbox.
#[async_trait]
pub trait Network: Send + Sync {
    /// Creates the network of a sandbox. Returns the path of the network
    /// namespace the sandbox must join, or `None` to let `runsc` decide.
    async fn setup(&self, sandbox_id: &str) -> Result<Option<PathBuf>, FlameError>;

    /// Removes the network of a sandbox; unknown sandboxes are not an error.
    async fn teardown(&self, sandbox_id: &str) -> Result<(), FlameError>;

    /// Removes leftovers of sandboxes whose id starts with `id_prefix`.
    async fn cleanup_stale(&self, id_prefix: &str) -> Result<(), FlameError>;
}

/// Sets up nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoNetwork;

#[async_trait]
impl Network for NoNetwork {
    async fn setup(&self, _sandbox_id: &str) -> Result<Option<PathBuf>, FlameError> {
        Ok(None)
    }

    async fn teardown(&self, _sandbox_id: &str) -> Result<(), FlameError> {
        Ok(())
    }

    async fn cleanup_stale(&self, _id_prefix: &str) -> Result<(), FlameError> {
        Ok(())
    }
}

/// Prefix of the pool all sandbox subnets are carved from: `10.201.0.0/16`.
const POOL: Cidr = Cidr {
    addr: u32::from_be_bytes([10, 201, 0, 0]),
    prefix: 16,
};
/// Number of /30 subnets in the pool.
const SLOTS: u16 = 16384;
/// Addresses sandboxes can never reach: link local (cloud metadata), loopback
/// and the pool itself (sandboxes must not reach each other).
const BASE_DENY: [Cidr; 3] = [
    Cidr {
        addr: u32::from_be_bytes([169, 254, 0, 0]),
        prefix: 16,
    },
    Cidr {
        addr: u32::from_be_bytes([127, 0, 0, 0]),
        prefix: 8,
    },
    POOL,
];
const NETNS_DIR: &str = "/var/run/netns";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_MAX_OUTPUT: usize = 64 << 10;

/// An IPv4 prefix with the host bits cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Cidr {
    addr: u32,
    prefix: u8,
}

impl Cidr {
    fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(prefix))
        };
        Self {
            addr: u32::from(addr) & mask,
            prefix,
        }
    }

    fn last(&self) -> u32 {
        self.addr | u32::MAX.checked_shr(u32::from(self.prefix)).unwrap_or(0)
    }

    fn contains(&self, other: &Cidr) -> bool {
        self.addr <= other.addr && other.last() <= self.last()
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.addr), self.prefix)
    }
}

/// Sorts and drops prefixes covered by another one: interval sets of nftables
/// reject overlapping elements.
fn merge(mut cidrs: Vec<Cidr>) -> Vec<Cidr> {
    cidrs.sort();
    let mut out: Vec<Cidr> = Vec::with_capacity(cidrs.len());
    for c in cidrs {
        if !out.last().is_some_and(|last| last.contains(&c)) {
            out.push(c);
        }
    }
    out
}

/// A validated entry of the egress allowlist or the deny list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// `*`: any destination.
    Any,
    /// An IPv4 address or CIDR.
    V4(Cidr),
    /// An IPv6 address or CIDR; sandboxes have no IPv6, so it is ignored.
    V6,
    /// A host name, resolved when the sandbox is set up.
    Host(String),
}

/// Validates an egress or deny entry: `*`, an IP address, a CIDR or a host
/// name. Everything else, in particular quotes, braces, semicolons and
/// whitespace, is rejected, so entries are safe to turn into rules.
pub(crate) fn parse_target(entry: &str) -> Result<Target, String> {
    let bad = || format!("'{entry}' is not a CIDR or host name");
    if entry.is_empty()
        || entry.len() > 253
        || !entry
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._:/*".contains(&b))
    {
        return Err(bad());
    }
    if entry == "*" {
        return Ok(Target::Any);
    }
    if entry.contains('*') {
        return Err(bad());
    }
    let (ip, prefix) = match entry.split_once('/') {
        Some((ip, prefix)) => (ip, Some(prefix)),
        None => (entry, None),
    };
    if let Ok(v4) = ip.parse::<Ipv4Addr>() {
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= 32)
                .ok_or_else(|| format!("'{entry}' has an invalid prefix length"))?,
            None => 32,
        };
        return Ok(Target::V4(Cidr::new(v4, prefix)));
    }
    if ip.parse::<Ipv6Addr>().is_ok() {
        return match prefix.map(str::parse::<u8>) {
            None | Some(Ok(0..=128)) => Ok(Target::V6),
            _ => Err(format!("'{entry}' has an invalid prefix length")),
        };
    }
    // Not an address: a host name (no slash, no colon, no empty label).
    if prefix.is_some()
        || entry.contains(':')
        || entry.starts_with(['.', '-'])
        || entry.contains("..")
    {
        return Err(bad());
    }
    Ok(Target::Host(entry.to_ascii_lowercase()))
}

/// Extracts the host of an endpoint such as `http://host:8080/path`,
/// `host:8080` or `[::1]:8080`.
fn endpoint_host(endpoint: &str) -> &str {
    let rest = endpoint.split_once("://").map_or(endpoint, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    if let Some(v6) = rest.strip_prefix('[') {
        return v6.split(']').next().unwrap_or(v6);
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => rest,
    }
}

/// Resolves a host name to its IPv4 addresses.
async fn resolve(host: &str) -> Result<Vec<Ipv4Addr>, String> {
    let name = host.to_string();
    let addrs = tokio::task::spawn_blocking(move || {
        (name.as_str(), 0)
            .to_socket_addrs()
            .map(|it| it.collect::<Vec<_>>())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(addrs
        .into_iter()
        .filter_map(|a| match a.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
        .collect())
}

/// The addressing of one sandbox, derived from its slot index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Addressing {
    n: u16,
}

impl Addressing {
    fn base(&self) -> [u8; 4] {
        [10, 201, (self.n / 64) as u8, ((self.n % 64) * 4) as u8]
    }

    /// Host end of the /30.
    fn host(&self) -> Ipv4Addr {
        let [a, b, c, d] = self.base();
        Ipv4Addr::new(a, b, c, d + 1)
    }

    /// Sandbox end of the /30.
    fn sandbox(&self) -> Ipv4Addr {
        let [a, b, c, d] = self.base();
        Ipv4Addr::new(a, b, c, d + 2)
    }

    fn host_if(&self) -> String {
        format!("fbh{}", self.n)
    }

    fn sandbox_if(&self) -> String {
        format!("fbs{}", self.n)
    }

    fn netns(&self) -> String {
        format!("flmbox{}", self.n)
    }

    fn table(&self) -> String {
        format!("flmbox_{}", self.n)
    }

    fn netns_path(&self) -> PathBuf {
        PathBuf::from(NETNS_DIR).join(self.netns())
    }
}

fn set_elements(cidrs: &[Cidr]) -> String {
    if cidrs.is_empty() {
        return String::new();
    }
    let list: Vec<String> = cidrs.iter().map(ToString::to_string).collect();
    format!("\n    elements = {{ {} }}", list.join(", "))
}

/// Renders the nftables table of one sandbox. `allow` is the egress allowlist;
/// `None` allows every destination that is not denied.
fn nft_script(a: &Addressing, deny: &[Cidr], allow: Option<&[Cidr]>) -> String {
    let table = a.table();
    let veth = a.host_if();
    let allow_rule = match allow {
        None => format!("iifname \"{veth}\" accept"),
        Some(_) => format!("iifname \"{veth}\" ip daddr @allow accept"),
    };
    let allow_set = merge(allow.unwrap_or_default().to_vec());
    format!(
        r#"table inet {table} {{
  set deny {{
    type ipv4_addr
    flags interval{deny_elements}
  }}
  set allow {{
    type ipv4_addr
    flags interval{allow_elements}
  }}
  chain forward {{
    type filter hook forward priority 0; policy accept;
    iifname "{veth}" ct state established,related accept
    oifname "{veth}" ct state established,related accept
    oifname "{veth}" drop
    iifname "{veth}" ip daddr @deny drop
    iifname "{veth}" udp dport 53 accept
    iifname "{veth}" tcp dport 53 accept
    {allow_rule}
    iifname "{veth}" drop
  }}
  chain input {{
    type filter hook input priority 0; policy accept;
    iifname "{veth}" drop
  }}
  chain postrouting {{
    type nat hook postrouting priority 100; policy accept;
    ip saddr {sandbox} oifname != "{veth}" masquerade
  }}
}}
"#,
        deny_elements = set_elements(&merge(deny.to_vec())),
        allow_elements = set_elements(&allow_set),
        sandbox = a.sandbox(),
    )
}

/// What `setup` has created so far, to undo on failure.
#[derive(Debug, Default, Clone, Copy)]
struct Created {
    netns: bool,
    link: bool,
    table: bool,
}

/// Gives sandboxes a network namespace, a veth pair on a /30 of `10.201.0.0/16`
/// and a per-sandbox nftables table: egress is limited to the allowlist, the
/// metadata service, loopback, other sandboxes and the Flame control plane are
/// unreachable, and the node itself cannot be reached at all.
pub struct IpNetwork {
    runner: Arc<dyn CommandRunner>,
    state_dir: PathBuf,
    egress: Vec<String>,
    deny: Vec<String>,
    control_plane: Vec<String>,
    ip_bin: String,
    nft_bin: String,
    sysctl_bin: String,
    forwarding: AtomicBool,
}

impl IpNetwork {
    /// Creates the network with the default binaries `ip`, `nft` and `sysctl`.
    /// `control_plane` holds hosts or `host:port`/URL endpoints that are denied.
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        state_dir: PathBuf,
        egress: Vec<String>,
        deny: Vec<String>,
        control_plane: Vec<String>,
    ) -> Self {
        Self {
            runner,
            state_dir,
            egress,
            deny,
            control_plane,
            ip_bin: "ip".to_string(),
            nft_bin: "nft".to_string(),
            sysctl_bin: "sysctl".to_string(),
            forwarding: AtomicBool::new(false),
        }
    }

    /// Overrides the names of the `ip` and `nft` binaries.
    pub fn with_bins(mut self, ip: impl Into<String>, nft: impl Into<String>) -> Self {
        self.ip_bin = ip.into();
        self.nft_bin = nft.into();
        self
    }

    fn slots_dir(&self) -> PathBuf {
        self.state_dir.join("net").join("slots")
    }

    /// Claims a free slot; `create_dir` is atomic, so concurrent callers and
    /// processes never share one.
    fn allocate(&self, id: &str) -> Result<Addressing, FlameError> {
        let dir = self.slots_dir();
        fs::create_dir_all(&dir).map_err(|e| {
            FlameError::Internal(format!("failed to create {}: {e}", dir.display()))
        })?;
        for n in 0..SLOTS {
            let slot = dir.join(n.to_string());
            match fs::create_dir(&slot) {
                Ok(()) => {
                    if let Err(e) = fs::write(slot.join("id"), id) {
                        let _ = fs::remove_dir_all(&slot);
                        return Err(FlameError::Internal(format!(
                            "failed to record slot {n}: {e}"
                        )));
                    }
                    return Ok(Addressing { n });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(FlameError::Internal(format!(
                        "failed to create {}: {e}",
                        slot.display()
                    )))
                }
            }
        }
        Err(FlameError::Internal(
            "no free sandbox network slot".to_string(),
        ))
    }

    fn release(&self, a: &Addressing) {
        let _ = fs::remove_dir_all(self.slots_dir().join(a.n.to_string()));
    }

    /// The slots whose recorded sandbox id satisfies `pred`.
    fn find(&self, pred: impl Fn(&str) -> bool) -> Vec<Addressing> {
        let Ok(entries) = fs::read_dir(self.slots_dir()) else {
            return Vec::new();
        };
        let mut found: Vec<Addressing> = entries
            .flatten()
            .filter_map(|e| {
                let n: u16 = e
                    .file_name()
                    .to_str()?
                    .parse()
                    .ok()
                    .filter(|n| *n < SLOTS)?;
                let id = fs::read_to_string(e.path().join("id")).ok()?;
                pred(&id).then_some(Addressing { n })
            })
            .collect();
        found.sort_by_key(|a| a.n);
        found
    }

    async fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<Vec<u8>>,
    ) -> Result<(), FlameError> {
        let shown = format!("{program} {}", args.join(" "));
        let out = self
            .runner
            .run(CommandSpec {
                program: program.to_string(),
                args: args.iter().map(ToString::to_string).collect(),
                stdin,
                timeout: Some(CONTROL_TIMEOUT),
                max_output: CONTROL_MAX_OUTPUT,
                ..CommandSpec::default()
            })
            .await?;
        if out.timed_out {
            return Err(FlameError::Network(format!("`{shown}` timed out")));
        }
        if out.exit_code != 0 {
            return Err(FlameError::Network(format!(
                "`{shown}` failed with exit code {}: {}",
                out.exit_code,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn ip(&self, args: &[&str]) -> Result<(), FlameError> {
        self.run(&self.ip_bin, args, None).await
    }

    /// Resolves the deny list and the allowlist into prefixes.
    async fn policy(&self) -> Result<(Vec<Cidr>, Option<Vec<Cidr>>), FlameError> {
        let mut deny = BASE_DENY.to_vec();
        for endpoint in &self.control_plane {
            let host = endpoint_host(endpoint);
            match parse_target(host) {
                Ok(Target::V4(c)) => deny.push(c),
                Ok(Target::Host(h)) => match resolve(&h).await {
                    Ok(addrs) => deny.extend(addrs.into_iter().map(|a| Cidr::new(a, 32))),
                    Err(e) => tracing::warn!("cannot resolve control plane host '{h}': {e}"),
                },
                Ok(_) => {}
                Err(e) => tracing::warn!("ignoring control plane endpoint '{endpoint}': {e}"),
            }
        }
        for entry in &self.deny {
            let invalid = |e: String| FlameError::InvalidConfig(format!("deny list: {e}"));
            match parse_target(entry).map_err(invalid)? {
                Target::V4(c) => deny.push(c),
                Target::Host(h) => match resolve(&h).await {
                    Ok(addrs) => deny.extend(addrs.into_iter().map(|a| Cidr::new(a, 32))),
                    Err(e) => tracing::warn!("cannot resolve denied host '{h}': {e}"),
                },
                Target::V6 => {}
                Target::Any => return Err(invalid("'*' cannot be denied".to_string())),
            }
        }

        let mut allow = Vec::new();
        for entry in &self.egress {
            let invalid = |e: String| FlameError::InvalidConfig(format!("egress list: {e}"));
            match parse_target(entry).map_err(invalid)? {
                Target::Any => return Ok((deny, None)),
                Target::V4(c) => allow.push(c),
                Target::Host(h) => match resolve(&h).await {
                    Ok(addrs) => allow.extend(addrs.into_iter().map(|a| Cidr::new(a, 32))),
                    Err(e) => tracing::warn!("cannot resolve egress host '{h}': {e}"),
                },
                Target::V6 => {}
            }
        }
        Ok((deny, Some(allow)))
    }

    async fn enable_forwarding(&self) -> Result<(), FlameError> {
        if self.forwarding.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.run(&self.sysctl_bin, &["-w", "net.ipv4.ip_forward=1"], None)
            .await?;
        self.forwarding.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn configure(
        &self,
        a: &Addressing,
        script: String,
        created: &mut Created,
    ) -> Result<(), FlameError> {
        let (ns, host_if, sb_if) = (a.netns(), a.host_if(), a.sandbox_if());
        let host = format!("{}/30", a.host());
        let sandbox = format!("{}/30", a.sandbox());
        let gateway = a.host().to_string();
        let ip_bin = self.ip_bin.as_str();

        self.enable_forwarding().await?;
        self.ip(&["netns", "add", &ns]).await?;
        created.netns = true;
        self.ip(&[
            "link", "add", &host_if, "type", "veth", "peer", "name", &sb_if,
        ])
        .await?;
        created.link = true;
        self.ip(&["link", "set", &sb_if, "netns", &ns]).await?;
        self.ip(&[
            "netns", "exec", &ns, ip_bin, "link", "set", &sb_if, "name", "eth0",
        ])
        .await?;
        self.ip(&["addr", "add", &host, "dev", &host_if]).await?;
        self.ip(&["link", "set", &host_if, "up"]).await?;
        for args in [
            &["addr", "add", &sandbox, "dev", "eth0"][..],
            &["link", "set", "eth0", "up"],
            &["link", "set", "lo", "up"],
            &["route", "add", "default", "via", &gateway],
        ] {
            let mut full = vec!["netns", "exec", ns.as_str(), ip_bin];
            full.extend_from_slice(args);
            self.ip(&full).await?;
        }
        self.run(&self.nft_bin, &["-f", "-"], Some(script.into_bytes()))
            .await?;
        created.table = true;
        Ok(())
    }

    /// Removes what exists of a sandbox network, newest first. Failures are
    /// logged and do not stop the rest.
    async fn undo(&self, a: &Addressing, created: Created) {
        let (ns, host_if, table) = (a.netns(), a.host_if(), a.table());
        if created.table {
            if let Err(e) = self
                .run(&self.nft_bin, &["delete", "table", "inet", &table], None)
                .await
            {
                tracing::warn!("{e}");
            }
        }
        if created.link {
            if let Err(e) = self.ip(&["link", "del", &host_if]).await {
                tracing::warn!("{e}");
            }
        }
        if created.netns {
            if let Err(e) = self.ip(&["netns", "del", &ns]).await {
                tracing::warn!("{e}");
            }
        }
        self.release(a);
    }
}

#[async_trait]
impl Network for IpNetwork {
    async fn setup(&self, sandbox_id: &str) -> Result<Option<PathBuf>, FlameError> {
        let (deny, allow) = self.policy().await?;
        let a = self.allocate(sandbox_id)?;
        let script = nft_script(&a, &deny, allow.as_deref());
        let mut created = Created::default();
        match self.configure(&a, script, &mut created).await {
            Ok(()) => Ok(Some(a.netns_path())),
            Err(e) => {
                self.undo(&a, created).await;
                Err(e)
            }
        }
    }

    async fn teardown(&self, sandbox_id: &str) -> Result<(), FlameError> {
        for a in self.find(|id| id == sandbox_id) {
            self.undo(
                &a,
                Created {
                    netns: true,
                    link: true,
                    table: true,
                },
            )
            .await;
        }
        Ok(())
    }

    /// Removes the networks recorded for sandboxes with the prefix. Interfaces
    /// and namespaces that have no slot record cannot be tied to an executor
    /// and are left alone.
    async fn cleanup_stale(&self, id_prefix: &str) -> Result<(), FlameError> {
        for a in self.find(|id| id.starts_with(id_prefix)) {
            tracing::info!("removing stale sandbox network {}", a.netns());
            self.undo(
                &a,
                Created {
                    netns: true,
                    link: true,
                    table: true,
                },
            )
            .await;
        }
        Ok(())
    }
}

/// Chooses the network of the sandboxes: none for [`NetworkMode::None`],
/// otherwise [`IpNetwork`] denying the Flame control plane named by
/// `FLAME_ENDPOINT` and `FLAME_CACHE_ENDPOINT`.
pub fn network_for(
    cfg: &Config,
    runner: Arc<dyn CommandRunner>,
    state_dir: PathBuf,
) -> Arc<dyn Network> {
    match cfg.network {
        NetworkMode::None => Arc::new(NoNetwork),
        NetworkMode::Sandbox => {
            let control_plane = ["FLAME_ENDPOINT", "FLAME_CACHE_ENDPOINT"]
                .into_iter()
                .filter_map(|k| std::env::var(k).ok())
                .filter(|v| !v.trim().is_empty())
                .collect();
            Arc::new(
                IpNetwork::new(
                    runner,
                    state_dir,
                    cfg.egress.clone(),
                    cfg.deny.clone(),
                    control_plane,
                )
                .with_bins(cfg.ip_bin.clone(), cfg.nft_bin.clone()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{out, FakeRunner};

    #[tokio::test]
    async fn no_network_does_nothing() {
        let n = NoNetwork;
        assert_eq!(n.setup("a").await.unwrap(), None);
        n.teardown("a").await.unwrap();
        n.cleanup_stale("a").await.unwrap();
    }

    struct Fixture {
        net: Arc<IpNetwork>,
        runner: Arc<FakeRunner>,
        dir: tempfile::TempDir,
    }

    fn fixture(egress: &[&str], deny: &[&str], control: &[&str]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeRunner::default());
        let strs = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        let net = Arc::new(IpNetwork::new(
            runner.clone(),
            dir.path().to_path_buf(),
            strs(egress),
            strs(deny),
            strs(control),
        ));
        Fixture { net, runner, dir }
    }

    impl Fixture {
        fn slots(&self) -> Vec<String> {
            let mut v: Vec<String> = fs::read_dir(self.dir.path().join("net/slots"))
                .map(|d| {
                    d.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        }
    }

    const SETUP_0: [&str; 12] = [
        "sysctl -w net.ipv4.ip_forward=1",
        "ip netns add flmbox0",
        "ip link add fbh0 type veth peer name fbs0",
        "ip link set fbs0 netns flmbox0",
        "ip netns exec flmbox0 ip link set fbs0 name eth0",
        "ip addr add 10.201.0.1/30 dev fbh0",
        "ip link set fbh0 up",
        "ip netns exec flmbox0 ip addr add 10.201.0.2/30 dev eth0",
        "ip netns exec flmbox0 ip link set eth0 up",
        "ip netns exec flmbox0 ip link set lo up",
        "ip netns exec flmbox0 ip route add default via 10.201.0.1",
        "nft -f -",
    ];

    #[tokio::test]
    async fn setup_runs_the_commands_in_order() {
        let f = fixture(&["*"], &[], &[]);
        let path = f.net.setup("flmbox-x.1").await.unwrap();
        assert_eq!(path, Some(PathBuf::from("/var/run/netns/flmbox0")));
        assert_eq!(f.runner.lines(), SETUP_0);
        assert_eq!(f.slots(), ["0"]);

        // Forwarding is enabled once.
        f.runner.clear();
        f.net.setup("flmbox-x.2").await.unwrap();
        let lines = f.runner.lines();
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[0], "ip netns add flmbox1");
        assert_eq!(lines[10], "nft -f -");
    }

    #[tokio::test]
    async fn the_script_is_sent_on_stdin() {
        let f = fixture(&["*"], &[], &[]);
        f.net.setup("a").await.unwrap();
        let specs = f.runner.specs();
        let nft = specs.last().unwrap();
        assert_eq!(nft.program, "nft");
        assert_eq!(nft.args, ["-f", "-"]);
        let script = String::from_utf8(nft.stdin.clone().unwrap()).unwrap();
        assert!(script.starts_with("table inet flmbox_0 {"));
        assert!(specs[..specs.len() - 1].iter().all(|s| s.stdin.is_none()));
    }

    #[test]
    fn script_denies_and_filters() {
        let a = Addressing { n: 5 };
        let deny = merge(
            BASE_DENY
                .iter()
                .copied()
                .chain([Cidr::new(Ipv4Addr::new(10, 1, 2, 3), 32)])
                .collect(),
        );
        let script = nft_script(&a, &deny, None);
        assert!(script
            .contains("elements = { 10.1.2.3/32, 10.201.0.0/16, 127.0.0.0/8, 169.254.0.0/16 }"));
        for line in [
            "iifname \"fbh5\" ct state established,related accept",
            "oifname \"fbh5\" ct state established,related accept",
            "iifname \"fbh5\" ip daddr @deny drop",
            "iifname \"fbh5\" udp dport 53 accept",
            "iifname \"fbh5\" tcp dport 53 accept",
            "type filter hook forward priority 0; policy accept;",
            "type filter hook input priority 0; policy accept;\n    iifname \"fbh5\" drop\n  }",
            "type nat hook postrouting priority 100;",
            "ip saddr 10.201.0.22 oifname != \"fbh5\" masquerade",
        ] {
            assert!(script.contains(line), "missing {line}\n{script}");
        }
        // Any destination: an unconditional accept, then the final drop.
        assert!(script.contains("    iifname \"fbh5\" accept\n    iifname \"fbh5\" drop\n"));
        assert!(!script.contains("@allow accept"));
    }

    #[test]
    fn script_with_an_allowlist() {
        let a = Addressing { n: 0 };
        let allow = [
            Cidr::new(Ipv4Addr::new(93, 184, 216, 34), 32),
            Cidr::new(Ipv4Addr::new(192, 168, 0, 0), 16),
        ];
        let script = nft_script(&a, &[], Some(&allow));
        assert!(script.contains("elements = { 93.184.216.34/32, 192.168.0.0/16 }"));
        assert!(script.contains("iifname \"fbh0\" ip daddr @allow accept"));
        assert!(!script.contains("    iifname \"fbh0\" accept\n"));
        // An empty allowlist still has DNS and the final drop.
        let script = nft_script(&a, &[], Some(&[]));
        assert!(script.contains("udp dport 53 accept"));
        assert!(script.contains("    iifname \"fbh0\" drop\n  }\n  chain input"));
    }

    #[tokio::test]
    async fn policy_resolves_control_plane_and_hosts() {
        let f = fixture(
            &["localhost", "203.0.113.0/24", "nonexistent.invalid"],
            &["198.51.100.0/24"],
            &[
                "http://192.0.2.7:8080/x",
                "192.0.2.8:9090",
                "[::1]:80",
                "nonexistent.invalid:1",
            ],
        );
        f.net.setup("a").await.unwrap();
        let script =
            String::from_utf8(f.runner.specs().last().unwrap().stdin.clone().unwrap()).unwrap();
        for c in ["192.0.2.7/32", "192.0.2.8/32", "198.51.100.0/24"] {
            assert!(script.contains(c), "{c}\n{script}");
        }
        assert!(script.contains("203.0.113.0/24"));
    }

    #[tokio::test]
    async fn invalid_entries_fail_setup_without_side_effects() {
        let f = fixture(&["10.0.0.1\"; flush ruleset; \""], &[], &[]);
        assert!(matches!(
            f.net.setup("a").await,
            Err(FlameError::InvalidConfig(_))
        ));
        let f = fixture(&[], &["*"], &[]);
        assert!(f.net.setup("a").await.is_err());
        assert!(f.runner.lines().is_empty());
        assert!(f.slots().is_empty());
    }

    #[test]
    fn validation_rejects_injection() {
        for bad in [
            "",
            "a\"b",
            "1.2.3.4\"; drop",
            "{ 1.2.3.4 }",
            "1.2.3.4; flush ruleset",
            "host\nname",
            "1.2.3.4 ",
            "a b",
            "10.0.0.0/33",
            "10.0.0.0/",
            "10.0.0.0/x",
            "*.example.com",
            "a/b",
            "host:80",
            "-host",
            "a..b",
            "::1/129",
            "$(id)",
            "a'b",
        ] {
            assert!(parse_target(bad).is_err(), "'{bad}' accepted");
        }
        assert_eq!(parse_target("*"), Ok(Target::Any));
        assert_eq!(
            parse_target("10.1.2.3/8"),
            Ok(Target::V4(Cidr::new(Ipv4Addr::new(10, 0, 0, 0), 8)))
        );
        assert_eq!(
            parse_target("1.2.3.4"),
            Ok(Target::V4(Cidr::new(Ipv4Addr::new(1, 2, 3, 4), 32)))
        );
        assert_eq!(parse_target("2001:db8::/32"), Ok(Target::V6));
        assert_eq!(
            parse_target("Pypi.org"),
            Ok(Target::Host("pypi.org".to_string()))
        );
    }

    #[test]
    fn address_math() {
        let cases = [
            (0, "10.201.0.1", "10.201.0.2"),
            (63, "10.201.0.253", "10.201.0.254"),
            (64, "10.201.1.1", "10.201.1.2"),
            (16383, "10.201.255.253", "10.201.255.254"),
        ];
        for (n, host, sandbox) in cases {
            let a = Addressing { n };
            assert_eq!(a.host().to_string(), host, "slot {n}");
            assert_eq!(a.sandbox().to_string(), sandbox, "slot {n}");
            assert_eq!(a.host_if(), format!("fbh{n}"));
            assert_eq!(a.netns(), format!("flmbox{n}"));
            assert!(POOL.contains(&Cidr::new(a.sandbox(), 32)));
        }
    }

    #[test]
    fn merge_drops_covered_prefixes() {
        let c = |a: [u8; 4], p| Cidr::new(Ipv4Addr::from(a), p);
        let merged = merge(vec![
            c([169, 254, 169, 254], 32),
            c([169, 254, 0, 0], 16),
            c([10, 0, 0, 0], 8),
            c([10, 0, 0, 0], 8),
        ]);
        assert_eq!(merged, [c([10, 0, 0, 0], 8), c([169, 254, 0, 0], 16)]);
        assert_eq!(
            merge(vec![c([0, 0, 0, 0], 0), c([1, 2, 3, 4], 32)]).len(),
            1
        );
    }

    #[tokio::test]
    async fn slots_are_unique_under_concurrency() {
        let f = fixture(&["*"], &[], &[]);
        let tasks: Vec<_> = (0..64)
            .map(|i| {
                let net = f.net.clone();
                tokio::spawn(async move { net.setup(&format!("id{i}")).await })
            })
            .collect();
        let mut paths = std::collections::BTreeSet::new();
        for t in tasks {
            paths.insert(t.await.unwrap().unwrap().unwrap());
        }
        assert_eq!(paths.len(), 64);
        assert_eq!(f.slots().len(), 64);
    }

    #[tokio::test]
    async fn teardown_releases_the_slot() {
        let f = fixture(&["*"], &[], &[]);
        f.net.setup("a").await.unwrap();
        f.net.setup("b").await.unwrap();
        f.runner.clear();
        f.net.teardown("a").await.unwrap();
        assert_eq!(
            f.runner.lines(),
            [
                "nft delete table inet flmbox_0",
                "ip link del fbh0",
                "ip netns del flmbox0"
            ]
        );
        assert_eq!(f.slots(), ["1"]);
        // The freed slot is reused.
        let path = f.net.setup("c").await.unwrap();
        assert_eq!(path, Some(PathBuf::from("/var/run/netns/flmbox0")));
        // Idempotent.
        f.net.teardown("a-gone").await.unwrap();
        f.net.teardown("a").await.unwrap();
    }

    #[tokio::test]
    async fn teardown_of_an_unknown_id_is_a_noop() {
        let f = fixture(&["*"], &[], &[]);
        f.net.teardown("nope").await.unwrap();
        f.net.cleanup_stale("nope").await.unwrap();
        assert!(f.runner.lines().is_empty());
    }

    #[tokio::test]
    async fn teardown_continues_past_failures() {
        let f = fixture(&["*"], &[], &[]);
        f.net.setup("a").await.unwrap();
        f.runner.clear();
        f.runner.respond(|s| {
            Ok(if s.program == "nft" {
                out(1, "", "no such table")
            } else {
                out(0, "", "")
            })
        });
        f.net.teardown("a").await.unwrap();
        assert_eq!(f.runner.lines().len(), 3);
        assert!(f.slots().is_empty());
    }

    #[tokio::test]
    async fn rollback_undoes_in_reverse() {
        // (failing command, expected undo commands)
        let cases: [(&str, &[&str]); 5] = [
            ("sysctl", &[]),
            ("ip netns add flmbox0", &[]),
            (
                "ip link add fbh0 type veth peer name fbs0",
                &["ip netns del flmbox0"],
            ),
            (
                "ip netns exec flmbox0 ip route add default via 10.201.0.1",
                &["ip link del fbh0", "ip netns del flmbox0"],
            ),
            ("nft -f -", &["ip link del fbh0", "ip netns del flmbox0"]),
        ];
        for (failing, undo) in cases {
            let f = fixture(&["*"], &[], &[]);
            let failing = failing.to_string();
            f.runner.respond(move |s| {
                let line = format!("{} {}", s.program, s.args.join(" "));
                Ok(if line.starts_with(&failing) {
                    out(2, "", "boom")
                } else {
                    out(0, "", "")
                })
            });
            let err = f.net.setup("a").await.unwrap_err();
            assert!(err.to_string().contains("boom"), "{err}");
            let lines = f.runner.lines();
            let tail: Vec<&str> = lines
                .iter()
                .rev()
                .take(undo.len())
                .rev()
                .map(String::as_str)
                .collect();
            assert_eq!(tail, undo, "failing at {lines:?}");
            assert!(f.slots().is_empty(), "slot released");
        }
    }

    #[tokio::test]
    async fn cleanup_removes_only_matching_slots() {
        let f = fixture(&["*"], &[], &[]);
        f.net.setup("flmbox-e1.1").await.unwrap();
        f.net.setup("flmbox-e2.1").await.unwrap();
        f.net.setup("flmbox-e1.2").await.unwrap();
        f.runner.clear();
        f.net.cleanup_stale("flmbox-e1.").await.unwrap();
        assert_eq!(f.slots(), ["1"]);
        assert_eq!(f.runner.lines().len(), 6);
        assert!(f
            .runner
            .lines()
            .contains(&"ip netns del flmbox2".to_string()));
    }

    #[tokio::test]
    async fn network_for_picks_the_implementation() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeRunner::default());
        let mut cfg = Config::from_map(&Default::default()).unwrap();
        cfg.network = NetworkMode::None;
        let net = network_for(&cfg, runner.clone(), dir.path().to_path_buf());
        assert_eq!(net.setup("a").await.unwrap(), None);
        net.teardown("a").await.unwrap();
        assert!(runner.lines().is_empty());

        cfg.network = NetworkMode::Sandbox;
        cfg.ip_bin = "/sbin/ip".to_string();
        let net = network_for(&cfg, runner.clone(), dir.path().to_path_buf());
        assert!(net.setup("a").await.unwrap().is_some());
        assert_eq!(runner.lines()[1], "/sbin/ip netns add flmbox0");
    }
}

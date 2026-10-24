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

//! The sandbox lifecycle: one `runsc` sandbox per session, owned by an
//! executor-scoped [`SandboxManager`].
//!
//! Phase 1 has no persistence: the sandbox is removed when the session leaves.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use flame_rs::apis::FlameError;
use sha2::{Digest, Sha256};

use crate::api::{CommandResult, Request, SandboxConfig};
use crate::bundle::write_bundle;
use crate::config::{Config, ImageRef};
use crate::images::Images;
use crate::network::Network;
use crate::runner::CommandRunner;
use crate::runsc::{ExecOptions, Runsc};

/// Image used when the session does not name one.
pub const DEFAULT_IMAGE: &str = "ubuntu:26.04";
/// Environment variable carrying the executor's endpoint (`<executor-id>.sock`).
pub const ENV_INSTANCE_ENDPOINT: &str = "FLAME_INSTANCE_ENDPOINT";

const MAX_ID_LEN: usize = 64;
const HASH_LEN: usize = 8;
/// Output cap per stream of one command.
const MAX_OUTPUT: usize = 16 << 20;
/// Exit code reported when a command timed out (as `timeout(1)` does).
const TIMEOUT_EXIT_CODE: i32 = 124;
const SHELL: &str = "/bin/sh";

#[derive(Debug, Clone)]
struct Active {
    id: String,
    bundle_dir: PathBuf,
}

/// Creates, runs commands in and removes the sandbox of the bound session.
pub struct SandboxManager {
    cfg: Config,
    executor_id: String,
    images: Arc<dyn Images>,
    runsc: Runsc,
    network: Arc<dyn Network>,
    active: tokio::sync::Mutex<Option<Active>>,
}

impl SandboxManager {
    /// Creates the manager of an executor. Build it once per executor and keep
    /// it across session bindings.
    pub fn new(
        cfg: Config,
        executor_id: &str,
        images: Arc<dyn Images>,
        runner: Arc<dyn CommandRunner>,
        network: Arc<dyn Network>,
    ) -> Self {
        let runsc = Runsc {
            bin: cfg.runsc.clone(),
            root: cfg.state_dir.join("runsc"),
            network: cfg.network,
            platform: cfg.platform.clone(),
            overlay: cfg.overlay.clone(),
            runner,
        };
        Self {
            cfg,
            executor_id: component(executor_id),
            images,
            runsc,
            network,
            active: tokio::sync::Mutex::new(None),
        }
    }

    /// Prefix shared by the ids of all sandboxes of this executor.
    pub fn id_prefix(&self) -> String {
        format!("flmbox-{}.", self.executor_id)
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.cfg.state_dir.join("sandboxes")
    }

    /// Creates and starts the sandbox of `session_id`, replacing any sandbox
    /// of this executor. The image must be on the allowlist.
    pub async fn enter(&self, session_id: &str, config: SandboxConfig) -> Result<(), FlameError> {
        let reference = config
            .image
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .unwrap_or(DEFAULT_IMAGE)
            .to_string();
        self.cfg.images.check(&ImageRef::parse(&reference)?)?;

        let id = sandbox_id(&self.executor_id, session_id);
        let mut active = self.active.lock().await;

        let mut stale: BTreeSet<String> = BTreeSet::new();
        stale.extend(active.take().map(|a| a.id));
        match self.runsc.list().await {
            Ok(ids) => stale.extend(ids.into_iter().filter(|i| i.starts_with(&self.id_prefix()))),
            Err(e) => tracing::warn!("failed to list sandboxes: {e}"),
        }
        stale.insert(id.clone());
        for old in &stale {
            self.remove(old).await;
        }

        match self.create(&id, &reference).await {
            Ok(bundle_dir) => {
                *active = Some(Active { id, bundle_dir });
                Ok(())
            }
            Err(e) => {
                self.remove(&id).await;
                Err(e)
            }
        }
    }

    async fn create(&self, id: &str, reference: &str) -> Result<PathBuf, FlameError> {
        let image = self.images.ensure_image(reference).await?;
        let netns = self.network.setup(id).await?;
        let bundle_dir = self.sandboxes_dir().join(id).join("bundle");
        write_bundle(
            &bundle_dir,
            &image.rootfs,
            &image.config,
            id,
            netns.as_deref(),
            &self.cfg.dns,
        )?;
        self.runsc.create(id, &bundle_dir).await?;
        self.runsc.start(id).await?;
        Ok(bundle_dir)
    }

    /// Kills and deletes a sandbox, tears down its network and removes its
    /// directory. Failures are logged: this runs on cleanup paths.
    async fn remove(&self, id: &str) {
        if let Err(e) = self.runsc.kill(id, "SIGKILL").await {
            tracing::warn!("failed to kill sandbox {id}: {e}");
        }
        if let Err(e) = self.runsc.delete(id).await {
            tracing::warn!("failed to delete sandbox {id}: {e}");
        }
        if let Err(e) = self.network.teardown(id).await {
            tracing::warn!("failed to tear down the network of sandbox {id}: {e}");
        }
        remove_dir(&self.sandboxes_dir().join(id));
    }

    /// Runs one request in the active sandbox. A nonzero exit code is a normal
    /// result; only infrastructure failures are errors.
    pub async fn execute(&self, request: Request) -> Result<CommandResult, FlameError> {
        let (argv, cwd, envs, stdin, timeout) = match request {
            Request::Run {
                cmd,
                cwd,
                envs,
                timeout,
            } => (
                vec![SHELL.to_string(), "-c".to_string(), cmd],
                cwd,
                envs.unwrap_or_default(),
                None,
                timeout,
            ),
            Request::RunCode {
                code,
                language,
                input,
                timeout,
            } => (
                code_argv(&language, code)?,
                None,
                BTreeMap::new(),
                input,
                timeout,
            ),
        };
        let limit = timeout.map(parse_timeout).transpose()?;

        let id = {
            let active = self.active.lock().await;
            match active.as_ref() {
                Some(a) => a.id.clone(),
                None => {
                    return Err(FlameError::InvalidConfig(
                        "invalid state: the session has no active sandbox".to_string(),
                    ))
                }
            }
        };

        let opts = ExecOptions {
            cwd,
            envs: envs.into_iter().collect(),
            stdin,
            timeout: limit.map(|(d, _)| d),
            max_output: MAX_OUTPUT,
        };
        let out = self.runsc.exec(&id, &argv, opts).await?;

        let mut stdout = out.stdout;
        let mut stderr = out.stderr;
        if out.truncated {
            mark_truncated(&mut stdout);
            mark_truncated(&mut stderr);
        }
        let mut exit_code = out.exit_code;
        if out.timed_out {
            exit_code = TIMEOUT_EXIT_CODE;
            if !stderr.is_empty() && !stderr.ends_with(b"\n") {
                stderr.push(b'\n');
            }
            let secs = limit.map(|(_, s)| s).unwrap_or_default();
            stderr.extend_from_slice(format!("timed out after {secs}s").as_bytes());
        }
        Ok(CommandResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Removes the sandbox of the session. Idempotent; failures are logged.
    pub async fn leave(&self) {
        let taken = self.active.lock().await.take();
        if let Some(active) = taken {
            self.remove(&active.id).await;
            remove_dir(&active.bundle_dir);
        }
    }

    /// Removes everything an earlier run of this executor left behind: its
    /// sandboxes known to `runsc`, its directories and its network state.
    pub async fn cleanup_stale(&self) -> Result<(), FlameError> {
        let prefix = self.id_prefix();
        let mut stale: BTreeSet<String> = BTreeSet::new();
        match self.runsc.list().await {
            Ok(ids) => stale.extend(ids.into_iter().filter(|i| i.starts_with(&prefix))),
            Err(e) => tracing::warn!("failed to list sandboxes: {e}"),
        }
        if let Ok(entries) = fs::read_dir(self.sandboxes_dir()) {
            stale.extend(
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.starts_with(&prefix)),
            );
        }
        for id in &stale {
            tracing::info!("removing stale sandbox {id}");
            self.remove(id).await;
        }
        self.network.cleanup_stale(&prefix).await
    }
}

fn remove_dir(dir: &Path) {
    if let Err(e) = fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("failed to remove {}: {e}", dir.display());
        }
    }
}

/// The executor id: the stem of the endpoint's file name (the host shim names
/// the socket `<executor-id>.sock`), or `local-<pid>`.
pub fn executor_id_from_endpoint(endpoint: Option<&str>) -> String {
    endpoint
        .and_then(|e| Path::new(e).file_stem())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("local-{}", std::process::id()))
}

/// The executor id of this process, from `FLAME_INSTANCE_ENDPOINT`.
pub fn executor_id_from_env() -> String {
    executor_id_from_endpoint(std::env::var(ENV_INSTANCE_ENDPOINT).ok().as_deref())
}

/// Maps to `[A-Za-z0-9_-]` (no dot: the dot separates executor and session).
fn component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `flmbox-<executor>.<session>`, characters `[A-Za-z0-9_.-]`, at most 64
/// long; a longer id is cut and ends in a hash of the full id.
pub fn sandbox_id(executor_id: &str, session_id: &str) -> String {
    let full = format!(
        "flmbox-{}.{}",
        component(executor_id),
        component(session_id)
    );
    if full.len() <= MAX_ID_LEN {
        return full;
    }
    let digest = Sha256::digest(full.as_bytes());
    let hash: String = digest
        .iter()
        .take(HASH_LEN / 2)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{}-{hash}", &full[..MAX_ID_LEN - HASH_LEN - 1])
}

fn code_argv(language: &str, code: String) -> Result<Vec<String>, FlameError> {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "python3" => Ok(vec!["python3".to_string(), "-c".to_string(), code]),
        "shell" | "sh" | "bash" => Ok(vec![SHELL.to_string(), "-c".to_string(), code]),
        other => Err(FlameError::InvalidConfig(format!(
            "unsupported language '{other}' (use python or shell)"
        ))),
    }
}

fn parse_timeout(secs: f64) -> Result<(Duration, f64), FlameError> {
    if !secs.is_finite() || secs <= 0.0 {
        return Err(FlameError::InvalidConfig(format!(
            "timeout must be a positive number of seconds, got {secs}"
        )));
    }
    Duration::try_from_secs_f64(secs)
        .map(|d| (d, secs))
        .map_err(|e| FlameError::InvalidConfig(format!("invalid timeout {secs}: {e}")))
}

fn mark_truncated(stream: &mut Vec<u8>) {
    if stream.len() >= MAX_OUTPUT {
        stream
            .extend_from_slice(format!("\n[output truncated at {MAX_OUTPUT} bytes]\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::ENV_IMAGES;
    use crate::runner::CommandOutput;
    use crate::testutil::{out, FakeImages, FakeNetwork, FakeRunner};

    struct Fixture {
        mgr: SandboxManager,
        runner: Arc<FakeRunner>,
        images: Arc<FakeImages>,
        network: Arc<FakeNetwork>,
        state: tempfile::TempDir,
        _rootfs: tempfile::TempDir,
    }

    fn fixture_with(network: FakeNetwork) -> Fixture {
        let state = tempfile::tempdir().unwrap();
        let rootfs = tempfile::tempdir().unwrap();
        let vars: HashMap<String, String> = HashMap::from([
            (
                "FLMBOX_STATE_DIR".to_string(),
                state.path().display().to_string(),
            ),
            (ENV_IMAGES.to_string(), "ubuntu:26.04,alpine:3".to_string()),
            ("FLMBOX_RUNSC".to_string(), "runsc".to_string()),
        ]);
        let cfg = Config::from_map(&vars).unwrap();
        let runner = Arc::new(FakeRunner::default());
        let images = FakeImages::new(rootfs.path().to_path_buf());
        let network = Arc::new(network);
        let mgr = SandboxManager::new(
            cfg,
            "exec1",
            images.clone(),
            runner.clone(),
            network.clone(),
        );
        Fixture {
            mgr,
            runner,
            images,
            network,
            state,
            _rootfs: rootfs,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(FakeNetwork::default())
    }

    const ID1: &str = "flmbox-exec1.s1";

    fn run(cmd: &str) -> Request {
        Request::Run {
            cmd: cmd.to_string(),
            cwd: None,
            envs: None,
            timeout: None,
        }
    }

    fn code(language: &str, code: &str) -> Request {
        Request::RunCode {
            code: code.to_string(),
            language: language.to_string(),
            input: None,
            timeout: None,
        }
    }

    fn sandbox_dir(f: &Fixture, id: &str) -> PathBuf {
        f.state.path().join("sandboxes").join(id)
    }

    #[test]
    fn sandbox_ids_are_sanitised_and_bounded() {
        assert_eq!(sandbox_id("exec1", "s1"), "flmbox-exec1.s1");
        assert_eq!(sandbox_id("e x", "a/b.c"), "flmbox-e_x.a_b_c");
        let long = "s".repeat(100);
        let id = sandbox_id("exec1", &long);
        assert_eq!(id.len(), 64);
        assert!(id.starts_with("flmbox-exec1.sss"));
        assert_eq!(id, sandbox_id("exec1", &long));
        assert_ne!(id, sandbox_id("exec1", &format!("{long}x")));
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)));
    }

    #[test]
    fn executor_id_comes_from_the_socket_name() {
        assert_eq!(
            executor_id_from_endpoint(Some("/run/flame/exec-42.sock")),
            "exec-42"
        );
        assert_eq!(executor_id_from_endpoint(Some("exec-7.sock")), "exec-7");
        let fallback = format!("local-{}", std::process::id());
        assert_eq!(executor_id_from_endpoint(None), fallback);
        assert_eq!(executor_id_from_endpoint(Some("")), fallback);
    }

    #[tokio::test]
    async fn enter_exec_leave_runs_the_expected_runsc_calls() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        assert_eq!(f.images.calls(), vec!["ubuntu:26.04"]);

        let bundle = sandbox_dir(&f, ID1).join("bundle");
        assert!(bundle.join("config.json").is_file());
        let b = bundle.display();
        assert_eq!(
            f.runner.subcommands(),
            vec![
                "list".to_string(),
                format!("kill {ID1} SIGKILL"),
                format!("delete --force {ID1}"),
                format!("create --bundle {b} {ID1}"),
                format!("start {ID1}"),
            ]
        );
        assert_eq!(
            f.network.events(),
            vec![format!("teardown {ID1}"), format!("setup {ID1}")]
        );

        f.runner.clear();
        let res = f.mgr.execute(run("echo hi")).await.unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(
            f.runner.subcommands(),
            vec![format!("exec {ID1} /bin/sh -c echo hi")]
        );

        f.runner.clear();
        f.mgr.leave().await;
        assert_eq!(
            f.runner.subcommands(),
            vec![
                format!("kill {ID1} SIGKILL"),
                format!("delete --force {ID1}")
            ]
        );
        assert!(!sandbox_dir(&f, ID1).exists());
        assert!(f.network.events().last().unwrap().starts_with("teardown"));
        assert!(matches!(
            f.mgr.execute(run("x")).await,
            Err(FlameError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn the_network_namespace_goes_into_the_bundle() {
        let f = fixture_with(FakeNetwork {
            netns: Some(PathBuf::from("/var/run/netns/sb")),
            ..FakeNetwork::default()
        });
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        let text = fs::read_to_string(sandbox_dir(&f, ID1).join("bundle/config.json")).unwrap();
        assert!(text.contains("/var/run/netns/sb"));
    }

    #[tokio::test]
    async fn custom_image_is_used_and_checked() {
        let f = fixture();
        let cfg = SandboxConfig {
            image: Some("alpine:3".to_string()),
        };
        f.mgr.enter("s1", cfg).await.unwrap();
        assert_eq!(f.images.calls(), vec!["alpine:3"]);
    }

    #[tokio::test]
    async fn allowlist_rejection_has_no_side_effects() {
        let f = fixture();
        let cfg = SandboxConfig {
            image: Some("evil/image:1".to_string()),
        };
        let err = f.mgr.enter("s1", cfg).await.unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
        let bad = SandboxConfig {
            image: Some("not a ref!".to_string()),
        };
        assert!(matches!(
            f.mgr.enter("s1", bad).await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(f.runner.specs().is_empty());
        assert!(f.network.events().is_empty());
        assert!(f.images.calls().is_empty());
        assert!(!f.state.path().join("sandboxes").exists());
    }

    #[tokio::test]
    async fn reenter_replaces_the_sandbox() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.clear();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        let subs = f.runner.subcommands();
        assert!(subs.contains(&format!("kill {ID1} SIGKILL")));
        assert!(subs.contains(&format!("delete --force {ID1}")));
        assert_eq!(subs.iter().filter(|s| s.starts_with("create")).count(), 1);
        assert!(subs.last().unwrap().starts_with("start"));
    }

    #[tokio::test]
    async fn other_sessions_of_this_executor_are_killed() {
        let f = fixture();
        f.runner.respond(|spec| {
            if spec.args.iter().any(|a| a == "list") {
                Ok(out(
                    0,
                    "ID PID STATUS\nflmbox-exec1.s0 1 running\nflmbox-exec2.s9 2 running\nother 3 running\n",
                    "",
                ))
            } else {
                Ok(CommandOutput::default())
            }
        });
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        let subs = f.runner.subcommands();
        assert!(subs.contains(&"kill flmbox-exec1.s0 SIGKILL".to_string()));
        assert!(subs.contains(&"delete --force flmbox-exec1.s0".to_string()));
        assert!(!subs.iter().any(|s| s.contains("flmbox-exec2.s9")));
        assert!(!subs.iter().any(|s| s.contains("other")));
        assert!(f
            .network
            .events()
            .contains(&"teardown flmbox-exec1.s0".to_string()));
    }

    #[tokio::test]
    async fn a_failed_create_rolls_everything_back() {
        let f = fixture();
        f.runner.respond(|spec| {
            if spec.args.iter().any(|a| a == "create") {
                Ok(out(1, "", "bad bundle"))
            } else {
                Ok(CommandOutput::default())
            }
        });
        let err = f
            .mgr
            .enter("s1", SandboxConfig::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad bundle"));
        let subs = f.runner.subcommands();
        let n = subs.len();
        assert_eq!(subs[n - 2], format!("kill {ID1} SIGKILL"));
        assert_eq!(subs[n - 1], format!("delete --force {ID1}"));
        assert_eq!(
            f.network.events().last().unwrap(),
            &format!("teardown {ID1}")
        );
        assert!(!sandbox_dir(&f, ID1).exists());
        assert!(matches!(
            f.mgr.execute(run("x")).await,
            Err(FlameError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn a_failed_start_rolls_back() {
        let f = fixture();
        f.runner.respond(|spec| {
            if spec.args.iter().any(|a| a == "start") {
                Ok(out(1, "", "no init"))
            } else {
                Ok(CommandOutput::default())
            }
        });
        assert!(f.mgr.enter("s1", SandboxConfig::default()).await.is_err());
        assert!(!sandbox_dir(&f, ID1).exists());
    }

    #[tokio::test]
    async fn image_and_network_failures_roll_back() {
        let f = fixture();
        *f.images.fail.lock().unwrap() = Some("registry down".to_string());
        let err = f
            .mgr
            .enter("s1", SandboxConfig::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("registry down"));
        assert!(!f
            .runner
            .subcommands()
            .iter()
            .any(|s| s.starts_with("create")));

        let f = fixture_with(FakeNetwork {
            fail_setup: true,
            ..FakeNetwork::default()
        });
        assert!(f.mgr.enter("s1", SandboxConfig::default()).await.is_err());
        assert!(!f
            .runner
            .subcommands()
            .iter()
            .any(|s| s.starts_with("create")));
        assert_eq!(
            f.network.events().last().unwrap(),
            &format!("teardown {ID1}")
        );
    }

    #[tokio::test]
    async fn leave_is_idempotent_and_survives_runsc_errors() {
        let f = fixture();
        f.mgr.leave().await;
        assert!(f.runner.specs().is_empty());

        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.respond(|_| Ok(out(1, "", "permission denied")));
        f.runner.clear();
        f.mgr.leave().await;
        assert_eq!(f.runner.subcommands().len(), 2);
        assert!(!sandbox_dir(&f, ID1).exists());

        f.runner.clear();
        f.mgr.leave().await;
        assert!(f.runner.specs().is_empty());
        assert!(f.mgr.execute(run("x")).await.is_err());
    }

    #[tokio::test]
    async fn stale_cleanup_removes_only_this_executors_sandboxes() {
        let f = fixture();
        let stale = sandbox_dir(&f, "flmbox-exec1.old");
        fs::create_dir_all(&stale).unwrap();
        let foreign = sandbox_dir(&f, "flmbox-exec2.keep");
        fs::create_dir_all(&foreign).unwrap();
        f.runner.respond(|_| {
            Ok(out(
                0,
                r#"[{"id":"flmbox-exec1.listed"},{"id":"flmbox-exec2.live"}]"#,
                "",
            ))
        });
        f.mgr.cleanup_stale().await.unwrap();
        let subs = f.runner.subcommands();
        assert!(subs.contains(&"delete --force flmbox-exec1.listed".to_string()));
        assert!(subs.contains(&"delete --force flmbox-exec1.old".to_string()));
        assert!(!subs.iter().any(|s| s.contains("exec2")));
        assert!(!stale.exists());
        assert!(foreign.exists());
        assert!(f
            .network
            .events()
            .contains(&"cleanup flmbox-exec1.".to_string()));
    }

    #[tokio::test]
    async fn run_passes_cwd_envs_and_the_timeout() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.clear();
        let req = Request::Run {
            cmd: "pwd".to_string(),
            cwd: Some("/work".to_string()),
            envs: Some(BTreeMap::from([
                ("B".to_string(), "2".to_string()),
                ("A".to_string(), "1".to_string()),
            ])),
            timeout: Some(2.5),
        };
        f.mgr.execute(req).await.unwrap();
        assert_eq!(
            f.runner.subcommands(),
            vec![format!(
                "exec --cwd /work --env A=1 --env B=2 {ID1} /bin/sh -c pwd"
            )]
        );
        let spec = &f.runner.specs()[0];
        assert_eq!(spec.timeout, Some(Duration::from_millis(2500)));
        assert_eq!(spec.max_output, 16 << 20);
    }

    #[tokio::test]
    async fn run_code_picks_the_interpreter_and_forwards_stdin() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.clear();
        let req = Request::RunCode {
            code: "print(input())".to_string(),
            language: "Python".to_string(),
            input: Some(b"data".to_vec()),
            timeout: None,
        };
        f.mgr.execute(req).await.unwrap();
        f.mgr.execute(code("python3", "x=1")).await.unwrap();
        f.mgr.execute(code("shell", "echo a")).await.unwrap();
        f.mgr.execute(code("sh", "echo b")).await.unwrap();
        f.mgr.execute(code("bash", "echo c")).await.unwrap();
        assert_eq!(
            f.runner.subcommands(),
            vec![
                format!("exec {ID1} python3 -c print(input())"),
                format!("exec {ID1} python3 -c x=1"),
                format!("exec {ID1} /bin/sh -c echo a"),
                format!("exec {ID1} /bin/sh -c echo b"),
                format!("exec {ID1} /bin/sh -c echo c"),
            ]
        );
        assert_eq!(f.runner.specs()[0].stdin.as_deref(), Some(&b"data"[..]));
        assert_eq!(f.runner.specs()[1].stdin, None);
    }

    #[tokio::test]
    async fn unknown_language_and_bad_timeouts_are_rejected() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.clear();
        assert!(matches!(
            f.mgr.execute(code("ruby", "puts 1")).await,
            Err(FlameError::InvalidConfig(_))
        ));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let req = Request::Run {
                cmd: "x".to_string(),
                cwd: None,
                envs: None,
                timeout: Some(bad),
            };
            assert!(matches!(
                f.mgr.execute(req).await,
                Err(FlameError::InvalidConfig(_))
            ));
        }
        assert!(f.runner.specs().is_empty());
    }

    #[tokio::test]
    async fn task_without_sandbox_is_an_invalid_state() {
        let f = fixture();
        let err = f.mgr.execute(run("ls")).await.unwrap_err();
        assert!(err.to_string().contains("no active sandbox"));
        assert!(f.runner.specs().is_empty());
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_normal_result() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.respond(|_| Ok(out(3, "o", "e")));
        let res = f.mgr.execute(run("exit 3")).await.unwrap();
        assert_eq!(
            res,
            CommandResult {
                exit_code: 3,
                stdout: b"o".to_vec(),
                stderr: b"e".to_vec()
            }
        );
    }

    #[tokio::test]
    async fn timeout_reports_124_and_a_message() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.respond(|_| {
            Ok(CommandOutput {
                exit_code: 137,
                stdout: b"partial".to_vec(),
                stderr: b"warn".to_vec(),
                timed_out: true,
                truncated: false,
            })
        });
        let req = Request::Run {
            cmd: "sleep 9".to_string(),
            cwd: None,
            envs: None,
            timeout: Some(2.0),
        };
        let res = f.mgr.execute(req).await.unwrap();
        assert_eq!(res.exit_code, 124);
        assert_eq!(res.stdout, b"partial");
        assert_eq!(res.stderr, b"warn\ntimed out after 2s");
    }

    #[tokio::test]
    async fn truncated_streams_get_a_marker() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner.respond(|_| {
            Ok(CommandOutput {
                exit_code: 0,
                stdout: vec![b'a'; MAX_OUTPUT],
                stderr: b"small".to_vec(),
                timed_out: false,
                truncated: true,
            })
        });
        let res = f.mgr.execute(run("yes")).await.unwrap();
        assert!(res.stdout.len() > MAX_OUTPUT);
        assert!(res.stdout.ends_with(b"truncated at 16777216 bytes]\n"));
        assert_eq!(res.stderr, b"small");
    }

    #[tokio::test]
    async fn exec_infrastructure_failure_is_an_error() {
        let f = fixture();
        f.mgr.enter("s1", SandboxConfig::default()).await.unwrap();
        f.runner
            .respond(|_| Err(FlameError::Internal("spawn failed".to_string())));
        assert!(matches!(
            f.mgr.execute(run("x")).await,
            Err(FlameError::Internal(_))
        ));
    }
}

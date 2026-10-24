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

//! The sandbox lifecycle: create, exec in and kill sandboxes by id.
//!
//! A [`Sandbox`] keeps no state of its own. The sandboxes live in the runtime's
//! state directory and the bundles on disk, so the CLI and the service reach
//! the same sandboxes through any number of processes.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use flame_rs::apis::FlameError;

use crate::api::{CommandRequest, CommandResult};

use crate::config::FlameSandbox;
use crate::images::{parse_reference, ImageInfo, ImageRoot, Images};
use crate::runtime::{runtime_for, ExecOptions, Runtime, SandboxInfo, SandboxSpec};

/// Image used when none is named.
pub const DEFAULT_IMAGE: &str = "ubuntu:26.04";

/// Longest sandbox id; ids name directories and `runsc` containers.
pub const MAX_ID_LEN: usize = 64;
/// Output cap per stream of one command.
const MAX_OUTPUT: usize = 16 << 20;
/// Exit code reported when a command timed out (as `timeout(1)` does).
const TIMEOUT_EXIT_CODE: i32 = 124;
const SHELL: &str = "/bin/sh";
/// File of a sandbox directory naming the image it was created from.
const IMAGE_FILE: &str = "image";

/// A sandbox as `ps` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRow {
    /// What the runtime reports.
    pub info: SandboxInfo,
    /// The image reference the sandbox was created from, when recorded.
    pub image: Option<String>,
}

/// Creates, runs commands in and removes sandboxes.
pub struct Sandbox {
    cfg: FlameSandbox,
    images: Arc<dyn Images>,
    runtime: Arc<dyn Runtime>,
}

impl Sandbox {
    /// Creates the lifecycle manager over the node's image cache and the
    /// runtime the configuration names.
    pub fn new(cfg: FlameSandbox, images: Arc<dyn Images>) -> Self {
        let runtime = runtime_for(&cfg);
        Self {
            cfg,
            images,
            runtime,
        }
    }

    /// Like [`new`](Self::new) with another runtime, so tests can use a fake.
    #[cfg(test)]
    pub fn with_runtime(
        cfg: FlameSandbox,
        images: Arc<dyn Images>,
        runtime: Arc<dyn Runtime>,
    ) -> Self {
        Self {
            cfg,
            images,
            runtime,
        }
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.cfg.state_dir.join("sandboxes")
    }

    /// Downloads and unpacks an image into the node-local cache.
    pub async fn pull(&self, reference: &str) -> Result<ImageRoot, FlameError> {
        parse_reference(reference)?;
        self.images.pull_image(reference).await
    }

    /// Pulls the image, then creates and starts sandbox `id` on it. Nothing is
    /// left behind on failure. An existing sandbox of the same id is replaced.
    pub async fn create(&self, id: &str, reference: &str) -> Result<(), FlameError> {
        validate_id(id)?;
        parse_reference(reference)?;
        if let Err(e) = self.kill(id).await {
            tracing::warn!("failed to remove the previous sandbox {id}: {e}");
        }
        match self.start(id, reference).await {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Err(cleanup) = self.kill(id).await {
                    tracing::warn!("failed to roll back sandbox {id}: {cleanup}");
                }
                Err(e)
            }
        }
    }

    async fn start(&self, id: &str, reference: &str) -> Result<(), FlameError> {
        let image = self.images.pull_image(reference).await?;
        let dir = self.sandboxes_dir().join(id);
        fs::create_dir_all(&dir)
            .map_err(|e| FlameError::Internal(format!("create {}: {e}", dir.display())))?;
        // Best effort: only `ps` reads it.
        let _ = fs::write(dir.join(IMAGE_FILE), reference);
        let spec = SandboxSpec {
            id,
            dir: &dir,
            image: &image,
        };
        self.runtime.start(&spec).await
    }

    /// The images in the node cache, newest first.
    pub async fn images(&self) -> Result<Vec<ImageInfo>, FlameError> {
        self.images.list_images().await
    }

    /// The sandboxes the runtime knows, with the image of each.
    pub async fn ps(&self) -> Result<Vec<SandboxRow>, FlameError> {
        let rows = self.runtime.list().await?;
        Ok(rows
            .into_iter()
            .map(|info| {
                let image =
                    fs::read_to_string(self.sandboxes_dir().join(&info.id).join(IMAGE_FILE))
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                SandboxRow { info, image }
            })
            .collect())
    }

    /// Whether the runtime knows sandbox `id`.
    pub async fn exists(&self, id: &str) -> Result<bool, FlameError> {
        Ok(self
            .runtime
            .list()
            .await?
            .iter()
            .any(|known| known.id == id))
    }

    /// Kills sandbox `id`, deletes it and removes its directory. Idempotent:
    /// an unknown sandbox is success. The directory is removed even when the
    /// runtime fails; that failure is returned.
    pub async fn kill(&self, id: &str) -> Result<(), FlameError> {
        validate_id(id)?;
        let killed = self.runtime.kill(id).await;
        remove_dir(&self.sandboxes_dir().join(id));
        killed
    }

    /// Runs one request in sandbox `id`. A nonzero exit code is a normal
    /// result; only infrastructure failures are errors.
    pub async fn exec(
        &self,
        id: &str,
        request: CommandRequest,
    ) -> Result<CommandResult, FlameError> {
        validate_id(id)?;
        let CommandRequest {
            code,
            language,
            cwd,
            envs,
            input: stdin,
            timeout,
        } = request;
        let argv = code_argv(language.as_deref().unwrap_or("shell"), code)?;
        let envs = envs.unwrap_or_default();
        let limit = timeout.map(parse_timeout).transpose()?;

        let opts = ExecOptions {
            cwd,
            envs: envs.into_iter().collect(),
            stdin,
            timeout: limit.map(|(d, _)| d),
            max_output: MAX_OUTPUT,
        };
        let out = self.runtime.exec(id, &argv, opts).await?;

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

    /// Kills every sandbox whose id starts with `prefix`: the ones `runsc`
    /// knows and the directories an earlier run left behind. Failures are
    /// logged; this runs on cleanup paths.
    pub async fn kill_prefixed(&self, prefix: &str) {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        match self.runtime.list().await {
            Ok(known) => ids.extend(
                known
                    .into_iter()
                    .map(|s| s.id)
                    .filter(|i| i.starts_with(prefix)),
            ),
            Err(e) => tracing::warn!("failed to list sandboxes: {e}"),
        }
        if let Ok(entries) = fs::read_dir(self.sandboxes_dir()) {
            ids.extend(
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.starts_with(prefix)),
            );
        }
        for id in &ids {
            tracing::info!("removing sandbox {id}");
            if let Err(e) = self.kill(id).await {
                tracing::warn!("failed to remove sandbox {id}: {e}");
            }
        }
    }
}

/// An id is a directory name and a `runsc` container id: `[A-Za-z0-9_.-]`,
/// at most [`MAX_ID_LEN`] long, not starting with a dot.
pub fn validate_id(id: &str) -> Result<(), FlameError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(FlameError::InvalidConfig(format!(
            "invalid sandbox id '{id}': use up to {MAX_ID_LEN} of [A-Za-z0-9_.-], not starting with '.'"
        )))
    }
}

fn remove_dir(dir: &Path) {
    if let Err(e) = fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("failed to remove {}: {e}", dir.display());
        }
    }
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
    use std::collections::{BTreeMap, HashMap};

    use super::*;
    use crate::runtime::CommandOutput;
    use crate::testutil::{out, runsc_runtime, FakeImages, FakeRunner};

    struct Fixture {
        sb: Sandbox,
        runner: Arc<FakeRunner>,
        images: Arc<FakeImages>,
        state: tempfile::TempDir,
        _rootfs: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let state = tempfile::tempdir().unwrap();
        let rootfs = tempfile::tempdir().unwrap();
        let vars: HashMap<String, String> = HashMap::from([
            (
                "FLMBOX_STATE_DIR".to_string(),
                state.path().display().to_string(),
            ),
            ("FLMBOX_RUNSC".to_string(), "runsc".to_string()),
        ]);
        let cfg = FlameSandbox::from_map(&vars).unwrap();
        let runner = Arc::new(FakeRunner::default());
        let images = FakeImages::new(rootfs.path().to_path_buf());
        let sb = Sandbox::with_runtime(
            cfg.clone(),
            images.clone(),
            runsc_runtime(&cfg, runner.clone()),
        );
        Fixture {
            sb,
            runner,
            images,
            state,
            _rootfs: rootfs,
        }
    }

    const ID: &str = "box1";

    fn run(cmd: &str) -> CommandRequest {
        CommandRequest {
            code: cmd.to_string(),
            ..Default::default()
        }
    }

    fn code(language: &str, code: &str) -> CommandRequest {
        CommandRequest {
            code: code.to_string(),
            language: Some(language.to_string()),
            ..Default::default()
        }
    }

    fn sandbox_dir(f: &Fixture, id: &str) -> PathBuf {
        f.state.path().join("sandboxes").join(id)
    }

    #[test]
    fn ids_are_validated() {
        for ok in ["a", "box1", "flmbox-e.s1", "a_b-c.d", &"x".repeat(64)] {
            assert!(validate_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "..", "a/b", "a b", "ü", &"x".repeat(65)] {
            assert!(
                matches!(validate_id(bad), Err(FlameError::InvalidConfig(_))),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn create_exec_kill_run_the_expected_runsc_calls() {
        let f = fixture();
        f.sb.create(ID, DEFAULT_IMAGE).await.unwrap();
        assert_eq!(f.images.calls(), vec![DEFAULT_IMAGE]);

        let bundle = sandbox_dir(&f, ID).join("bundle");
        assert!(bundle.join("config.json").is_file());
        let b = bundle.display();
        assert_eq!(
            f.runner.subcommands(),
            vec![
                format!("kill {ID} SIGKILL"),
                format!("delete --force {ID}"),
                format!("create --bundle {b} {ID}"),
                format!("start {ID}"),
            ]
        );

        f.runner.clear();
        let res = f.sb.exec(ID, run("echo hi")).await.unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(
            f.runner.subcommands(),
            vec![format!("exec {ID} /bin/sh -c echo hi")]
        );

        f.runner.clear();
        f.sb.kill(ID).await.unwrap();
        assert_eq!(
            f.runner.subcommands(),
            vec![format!("kill {ID} SIGKILL"), format!("delete --force {ID}")]
        );
        assert!(!sandbox_dir(&f, ID).exists());
    }

    #[tokio::test]
    async fn create_replaces_an_existing_sandbox_of_the_same_id() {
        let f = fixture();
        f.sb.create(ID, DEFAULT_IMAGE).await.unwrap();
        f.runner.clear();
        f.sb.create(ID, "alpine:3").await.unwrap();
        let subs = f.runner.subcommands();
        assert_eq!(subs[0], format!("kill {ID} SIGKILL"));
        assert_eq!(subs.iter().filter(|s| s.starts_with("create")).count(), 1);
        assert!(subs.last().unwrap().starts_with("start"));
        assert_eq!(f.images.calls(), vec![DEFAULT_IMAGE, "alpine:3"]);
    }

    #[tokio::test]
    async fn invalid_id_or_image_has_no_side_effects() {
        let f = fixture();
        assert!(matches!(
            f.sb.create("a/b", DEFAULT_IMAGE).await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(matches!(
            f.sb.create(ID, "not a ref!").await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(matches!(
            f.sb.pull("not a ref!").await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(matches!(
            f.sb.exec("../x", run("ls")).await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(matches!(
            f.sb.kill("../x").await,
            Err(FlameError::InvalidConfig(_))
        ));
        assert!(f.runner.specs().is_empty());
        assert!(f.images.calls().is_empty());
        assert!(!f.state.path().join("sandboxes").exists());
    }

    #[tokio::test]
    async fn pull_only_fetches_the_image() {
        let f = fixture();
        let root = f.sb.pull("alpine:3").await.unwrap();
        assert_eq!(root.digest, "sha256:fake");
        assert_eq!(f.images.calls(), vec!["alpine:3"]);
        assert!(f.runner.specs().is_empty());
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
        let err = f.sb.create(ID, DEFAULT_IMAGE).await.unwrap_err();
        assert!(err.to_string().contains("bad bundle"));
        let subs = f.runner.subcommands();
        let n = subs.len();
        assert_eq!(subs[n - 2], format!("kill {ID} SIGKILL"));
        assert_eq!(subs[n - 1], format!("delete --force {ID}"));
        assert!(!sandbox_dir(&f, ID).exists());
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
        assert!(f.sb.create(ID, DEFAULT_IMAGE).await.is_err());
        assert!(!sandbox_dir(&f, ID).exists());
    }

    #[tokio::test]
    async fn an_image_failure_rolls_back_before_runsc_create() {
        let f = fixture();
        *f.images.fail.lock().unwrap() = Some("registry down".to_string());
        let err = f.sb.create(ID, DEFAULT_IMAGE).await.unwrap_err();
        assert!(err.to_string().contains("registry down"));
        assert!(!f
            .runner
            .subcommands()
            .iter()
            .any(|s| s.starts_with("create")));
        assert!(!sandbox_dir(&f, ID).exists());
    }

    #[tokio::test]
    async fn kill_is_idempotent_and_still_removes_the_directory_on_errors() {
        let f = fixture();
        f.sb.create(ID, DEFAULT_IMAGE).await.unwrap();
        f.runner.respond(|_| Ok(out(1, "", "permission denied")));
        f.runner.clear();
        assert!(f.sb.kill(ID).await.is_err());
        assert_eq!(f.runner.subcommands().len(), 2);
        assert!(!sandbox_dir(&f, ID).exists());

        f.runner
            .respond(|_| Ok(out(1, "", "container does not exist")));
        f.sb.kill(ID).await.unwrap();
    }

    #[tokio::test]
    async fn exists_follows_runsc() {
        let f = fixture();
        f.runner.respond(|_| {
            Ok(out(
                0,
                "ID PID STATUS\nbox1 1 running\nbox2 2 running\n",
                "",
            ))
        });
        assert!(f.sb.exists("box2").await.unwrap());
        assert!(!f.sb.exists("box").await.unwrap());
    }

    #[tokio::test]
    async fn ps_lists_the_runtime_sandboxes_with_their_images() {
        let f = fixture();
        f.sb.create("box1", "alpine:3").await.unwrap();
        f.runner.respond(|_| {
            Ok(out(
                0,
                "ID PID STATUS BUNDLE CREATED OWNER\nbox1 7 running /b 2026-01-01T00:00:00Z root\nother 8 created /c 2026-01-02T00:00:00Z root\n",
                "",
            ))
        });
        let rows = f.sb.ps().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].info.id, "box1");
        assert_eq!(rows[0].info.status, "running");
        assert_eq!(rows[0].image.as_deref(), Some("alpine:3"));
        assert_eq!(rows[1].image, None);
    }

    #[tokio::test]
    async fn images_come_from_the_image_cache() {
        let f = fixture();
        f.sb.pull("alpine:3").await.unwrap();
        let images = f.sb.images().await.unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference.as_deref(), Some("alpine:3"));
    }

    #[tokio::test]
    async fn kill_prefixed_removes_only_matching_sandboxes() {
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
        f.sb.kill_prefixed("flmbox-exec1.").await;
        let subs = f.runner.subcommands();
        assert!(subs.contains(&"delete --force flmbox-exec1.listed".to_string()));
        assert!(subs.contains(&"delete --force flmbox-exec1.old".to_string()));
        assert!(!subs.iter().any(|s| s.contains("exec2")));
        assert!(!stale.exists());
        assert!(foreign.exists());
    }

    #[tokio::test]
    async fn exec_passes_cwd_envs_and_the_timeout() {
        let f = fixture();
        let req = CommandRequest {
            code: "pwd".to_string(),
            cwd: Some("/work".to_string()),
            envs: Some(BTreeMap::from([
                ("B".to_string(), "2".to_string()),
                ("A".to_string(), "1".to_string()),
            ])),
            timeout: Some(2.5),
            ..Default::default()
        };
        f.sb.exec(ID, req).await.unwrap();
        assert_eq!(
            f.runner.subcommands(),
            vec![format!(
                "exec --cwd /work --env A=1 --env B=2 {ID} /bin/sh -c pwd"
            )]
        );
        let spec = &f.runner.specs()[0];
        assert_eq!(spec.timeout, Some(Duration::from_millis(2500)));
        assert_eq!(spec.max_output, 16 << 20);
    }

    #[tokio::test]
    async fn exec_picks_the_interpreter_and_forwards_stdin() {
        let f = fixture();
        let req = CommandRequest {
            code: "print(input())".to_string(),
            language: Some("Python".to_string()),
            input: Some(b"data".to_vec()),
            ..Default::default()
        };
        f.sb.exec(ID, req).await.unwrap();
        f.sb.exec(ID, code("python3", "x=1")).await.unwrap();
        f.sb.exec(ID, code("shell", "echo a")).await.unwrap();
        f.sb.exec(ID, code("sh", "echo b")).await.unwrap();
        f.sb.exec(ID, code("bash", "echo c")).await.unwrap();
        assert_eq!(
            f.runner.subcommands(),
            vec![
                format!("exec {ID} python3 -c print(input())"),
                format!("exec {ID} python3 -c x=1"),
                format!("exec {ID} /bin/sh -c echo a"),
                format!("exec {ID} /bin/sh -c echo b"),
                format!("exec {ID} /bin/sh -c echo c"),
            ]
        );
        assert_eq!(f.runner.specs()[0].stdin.as_deref(), Some(&b"data"[..]));
        assert_eq!(f.runner.specs()[1].stdin, None);
    }

    #[tokio::test]
    async fn unknown_language_and_bad_timeouts_are_rejected() {
        let f = fixture();
        assert!(matches!(
            f.sb.exec(ID, code("ruby", "puts 1")).await,
            Err(FlameError::InvalidConfig(_))
        ));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let req = CommandRequest {
                code: "x".to_string(),
                timeout: Some(bad),
                ..Default::default()
            };
            assert!(matches!(
                f.sb.exec(ID, req).await,
                Err(FlameError::InvalidConfig(_))
            ));
        }
        assert!(f.runner.specs().is_empty());
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_normal_result() {
        let f = fixture();
        f.runner.respond(|_| Ok(out(3, "o", "e")));
        let res = f.sb.exec(ID, run("exit 3")).await.unwrap();
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
        f.runner.respond(|_| {
            Ok(CommandOutput {
                exit_code: 137,
                stdout: b"partial".to_vec(),
                stderr: b"warn".to_vec(),
                timed_out: true,
                truncated: false,
            })
        });
        let req = CommandRequest {
            code: "sleep 9".to_string(),
            timeout: Some(2.0),
            ..Default::default()
        };
        let res = f.sb.exec(ID, req).await.unwrap();
        assert_eq!(res.exit_code, 124);
        assert_eq!(res.stdout, b"partial");
        assert_eq!(res.stderr, b"warn\ntimed out after 2s");
    }

    #[tokio::test]
    async fn truncated_streams_get_a_marker() {
        let f = fixture();
        f.runner.respond(|_| {
            Ok(CommandOutput {
                exit_code: 0,
                stdout: vec![b'a'; MAX_OUTPUT],
                stderr: b"small".to_vec(),
                timed_out: false,
                truncated: true,
            })
        });
        let res = f.sb.exec(ID, run("yes")).await.unwrap();
        assert!(res.stdout.len() > MAX_OUTPUT);
        assert!(res.stdout.ends_with(b"truncated at 16777216 bytes]\n"));
        assert_eq!(res.stderr, b"small");
    }

    #[tokio::test]
    async fn exec_infrastructure_failure_is_an_error() {
        let f = fixture();
        f.runner
            .respond(|_| Err(FlameError::Internal("spawn failed".to_string())));
        assert!(matches!(
            f.sb.exec(ID, run("x")).await,
            Err(FlameError::Internal(_))
        ));
    }
}

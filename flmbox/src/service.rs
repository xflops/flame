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

//! The `flmbox` [`FlameService`]: binds one sandbox to the session an executor
//! serves.
//!
//! The sandbox is removed when the session leaves, so there is no persistence
//! yet.

use flame_rs::apis::{FlameError, TaskOutput};
use flame_rs::service::{async_trait, FlameService, Publisher, SessionContext, TaskContext};
use flame_rs::FlameMessage;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::api::{CommandRequest, SandboxConfig};
use crate::images::parse_reference;
use crate::sandbox::{Sandbox, DEFAULT_IMAGE, MAX_ID_LEN};

/// Environment variable carrying the executor's endpoint (`<executor-id>.sock`).
pub const ENV_INSTANCE_ENDPOINT: &str = "FLAME_INSTANCE_ENDPOINT";

const HASH_LEN: usize = 8;

/// Runs commands in a per-session sandbox. One instance serves an executor
/// for its whole life; the sandbox belongs to the bound session.
pub struct FlmboxService {
    publisher: Publisher,
    sandbox: Sandbox,
    executor_id: String,
    active: Mutex<Option<String>>,
}

impl FlmboxService {
    /// Wraps the lifecycle manager of the executor `executor_id`.
    pub fn new(sandbox: Sandbox, executor_id: &str) -> Self {
        Self {
            publisher: Publisher::default(),
            sandbox,
            executor_id: component(executor_id),
            active: Mutex::new(None),
        }
    }

    /// Prefix shared by the ids of all sandboxes of this executor.
    fn id_prefix(&self) -> String {
        format!("flmbox-{}.", self.executor_id)
    }

    /// Removes everything an earlier run of this executor left behind.
    pub async fn cleanup_stale(&self) {
        self.sandbox.kill_prefixed(&self.id_prefix()).await;
    }

    /// Creates and starts the sandbox of `session_id`, replacing any sandbox
    /// of this executor.
    async fn enter(&self, session_id: &str, config: SandboxConfig) -> Result<(), FlameError> {
        let reference = config
            .image
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .unwrap_or(DEFAULT_IMAGE)
            .to_string();
        parse_reference(&reference)?;

        let id = sandbox_id(&self.executor_id, session_id);
        let mut active = self.active.lock().await;
        *active = None;
        self.cleanup_stale().await;
        self.sandbox.create(&id, &reference).await?;
        *active = Some(id);
        Ok(())
    }

    async fn execute(
        &self,
        request: CommandRequest,
    ) -> Result<crate::api::CommandResult, FlameError> {
        let id = self.active.lock().await.clone().ok_or_else(|| {
            FlameError::InvalidConfig(
                "invalid state: the session has no active sandbox".to_string(),
            )
        })?;
        self.sandbox.exec(&id, request).await
    }

    async fn leave(&self) {
        let taken = self.active.lock().await.take();
        if let Some(id) = taken {
            if let Err(e) = self.sandbox.kill(&id).await {
                tracing::warn!("failed to remove sandbox {id}: {e}");
            }
        }
    }
}

/// The executor id: the stem of the endpoint's file name (the host shim names
/// the socket `<executor-id>.sock`), or `local-<pid>`.
pub fn executor_id_from_endpoint(endpoint: Option<&str>) -> String {
    endpoint
        .and_then(|e| std::path::Path::new(e).file_stem())
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
fn sandbox_id(executor_id: &str, session_id: &str) -> String {
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

fn decode_config(data: Option<&[u8]>) -> Result<SandboxConfig, FlameError> {
    match data {
        Some(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => serde_json::from_slice(bytes)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid session common data: {e}"))),
        _ => Ok(SandboxConfig::default()),
    }
}

#[async_trait]
impl FlameService for FlmboxService {
    fn publisher(&self) -> &Publisher {
        &self.publisher
    }

    async fn on_session_enter(&self, ctx: SessionContext) -> Result<(), FlameError> {
        let config = decode_config(ctx.common_data.as_deref())?;
        self.enter(&ctx.session_id, config).await
    }

    async fn on_task_invoke(&self, ctx: TaskContext) -> Result<Option<TaskOutput>, FlameError> {
        let input = ctx
            .input
            .ok_or_else(|| FlameError::InvalidConfig("task has no input".to_string()))?;
        let request: CommandRequest = serde_json::from_slice(&input)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid task input: {e}")))?;
        let result = self.execute(request).await?;
        Ok(Some(result.encode()?))
    }

    async fn on_session_leave(&self) -> Result<(), FlameError> {
        self.leave().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use flame_rs::__private::bytes::Bytes;
    use flame_rs::service::ApplicationContext;

    use super::*;
    use crate::api::CommandResult;
    use crate::config::{FlameSandbox, ENV_STATE_DIR};
    use crate::runtime::CommandOutput;
    use crate::testutil::{out, runsc_runtime, FakeImages, FakeRunner};

    struct Fixture {
        svc: FlmboxService,
        runner: Arc<FakeRunner>,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    fn fixture() -> Fixture {
        let state = tempfile::tempdir().unwrap();
        let rootfs = tempfile::tempdir().unwrap();
        let vars = HashMap::from([(
            ENV_STATE_DIR.to_string(),
            state.path().display().to_string(),
        )]);
        let runner = Arc::new(FakeRunner::default());
        let cfg = FlameSandbox::from_map(&vars).unwrap();
        let sandbox = Sandbox::with_runtime(
            cfg.clone(),
            FakeImages::new(rootfs.path().to_path_buf()),
            runsc_runtime(&cfg, runner.clone()),
        );
        Fixture {
            svc: FlmboxService::new(sandbox, "exec1"),
            runner,
            _dirs: (state, rootfs),
        }
    }

    fn session(id: &str, common: Option<&str>) -> SessionContext {
        SessionContext::new(
            id.to_string(),
            ApplicationContext {
                name: "flmbox".to_string(),
                image: None,
                command: None,
            },
            common.map(|c| Bytes::from(c.as_bytes().to_vec())),
        )
    }

    fn task(input: Option<&str>) -> TaskContext {
        TaskContext {
            task_id: "t1".to_string(),
            session_id: "s1".to_string(),
            input: input.map(|i| Bytes::from(i.as_bytes().to_vec())),
        }
    }

    #[tokio::test]
    async fn full_flow_through_the_service_trait() {
        let f = fixture();
        f.svc.on_session_enter(session("s1", None)).await.unwrap();
        f.runner.respond(|_| Ok(out(0, "hello\n", "")));
        let output = f
            .svc
            .on_task_invoke(task(Some(r#"{"code":"echo hello"}"#)))
            .await
            .unwrap()
            .unwrap();
        let result = CommandResult::decode(&output).unwrap();
        assert_eq!(
            result,
            CommandResult {
                exit_code: 0,
                stdout: b"hello\n".to_vec(),
                stderr: vec![]
            }
        );
        assert!(String::from_utf8(output.to_vec())
            .unwrap()
            .contains(r#""stdout":[104,101"#));
        assert!(f
            .runner
            .subcommands()
            .contains(&"exec flmbox-exec1.s1 /bin/sh -c echo hello".to_string()));

        f.svc.on_session_leave().await.unwrap();
        f.svc.on_session_leave().await.unwrap();
        assert!(f
            .svc
            .on_task_invoke(task(Some(r#"{"code":"x"}"#)))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn run_code_with_stdin_bytes() {
        let f = fixture();
        f.svc
            .on_session_enter(session("s1", Some("{}")))
            .await
            .unwrap();
        let input = r#"{"code":"import sys","language":"python","input":[104,105]}"#;
        f.svc.on_task_invoke(task(Some(input))).await.unwrap();
        let specs = f.runner.specs();
        assert_eq!(specs.last().unwrap().stdin.as_deref(), Some(&b"hi"[..]));
    }

    #[tokio::test]
    async fn common_data_selects_the_image() {
        let f = fixture();
        f.svc
            .on_session_enter(session("s1", Some(r#"{"image":"ubuntu:26.04","x":1}"#)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn empty_or_absent_common_data_means_defaults() {
        for common in [None, Some(""), Some("  \n")] {
            let f = fixture();
            f.svc.on_session_enter(session("s1", common)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_common_data_is_invalid_config() {
        let f = fixture();
        for bad in ["not json", "[1]", r#"{"image":5}"#] {
            let err = f
                .svc
                .on_session_enter(session("s1", Some(bad)))
                .await
                .unwrap_err();
            assert!(matches!(err, FlameError::InvalidConfig(_)), "{bad}");
        }
        assert!(f.runner.specs().is_empty());
    }

    #[tokio::test]
    async fn invalid_task_input_is_invalid_config() {
        let f = fixture();
        f.svc.on_session_enter(session("s1", None)).await.unwrap();
        for bad in [None, Some("nope"), Some(r#"{"language":"python"}"#)] {
            let err = f.svc.on_task_invoke(task(bad)).await.unwrap_err();
            assert!(matches!(err, FlameError::InvalidConfig(_)), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn nonzero_exit_and_timeout_are_ok_outputs() {
        let f = fixture();
        f.svc.on_session_enter(session("s1", None)).await.unwrap();
        f.runner.respond(|_| {
            Ok(CommandOutput {
                exit_code: 137,
                timed_out: true,
                ..CommandOutput::default()
            })
        });
        let output = f
            .svc
            .on_task_invoke(task(Some(r#"{"code":"sleep 9","timeout":1}"#)))
            .await
            .unwrap()
            .unwrap();
        let result = CommandResult::decode(&output).unwrap();
        assert_eq!(result.exit_code, 124);
        assert_eq!(result.stderr, b"timed out after 1s");
    }

    const ID1: &str = "flmbox-exec1.s1";

    #[test]
    fn sandbox_ids_are_sanitised_and_bounded() {
        assert_eq!(sandbox_id("exec1", "s1"), ID1);
        assert_eq!(sandbox_id("e x", "a/b.c"), "flmbox-e_x.a_b_c");
        let long = "s".repeat(100);
        let id = sandbox_id("exec1", &long);
        assert_eq!(id.len(), 64);
        assert!(id.starts_with("flmbox-exec1.sss"));
        assert_eq!(id, sandbox_id("exec1", &long));
        assert_ne!(id, sandbox_id("exec1", &format!("{long}x")));
        assert!(crate::sandbox::validate_id(&id).is_ok());
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
    async fn enter_creates_the_sandbox_and_leave_removes_it() {
        let f = fixture();
        f.svc.on_session_enter(session("s1", None)).await.unwrap();
        let subs = f.runner.subcommands();
        assert!(subs.iter().any(|s| s.starts_with("create")));
        assert_eq!(subs.last().unwrap(), &format!("start {ID1}"));

        f.runner.clear();
        f.svc.on_session_leave().await.unwrap();
        assert_eq!(
            f.runner.subcommands(),
            vec![
                format!("kill {ID1} SIGKILL"),
                format!("delete --force {ID1}")
            ]
        );

        f.runner.clear();
        f.svc.on_session_leave().await.unwrap();
        assert!(f.runner.specs().is_empty());
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
        f.svc.on_session_enter(session("s1", None)).await.unwrap();
        let subs = f.runner.subcommands();
        assert!(subs.contains(&"kill flmbox-exec1.s0 SIGKILL".to_string()));
        assert!(subs.contains(&"delete --force flmbox-exec1.s0".to_string()));
        assert!(!subs.iter().any(|s| s.contains("flmbox-exec2.s9")));
        assert!(!subs.iter().any(|s| s.contains("other")));
    }

    #[tokio::test]
    async fn a_failed_enter_leaves_no_active_sandbox() {
        let f = fixture();
        f.runner.respond(|spec| {
            if spec.args.iter().any(|a| a == "create") {
                Ok(out(1, "", "bad bundle"))
            } else {
                Ok(CommandOutput::default())
            }
        });
        let err = f
            .svc
            .on_session_enter(session("s1", None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad bundle"));
        let err = f
            .svc
            .on_task_invoke(task(Some(r#"{"code":"x"}"#)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no active sandbox"));
    }

    #[tokio::test]
    async fn invalid_image_has_no_side_effects() {
        let f = fixture();
        let err = f
            .svc
            .on_session_enter(session("s1", Some(r#"{"image":"not a ref!"}"#)))
            .await
            .unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
        assert!(f.runner.specs().is_empty());
    }

    #[tokio::test]
    async fn task_without_sandbox_is_an_invalid_state() {
        let f = fixture();
        let err = f
            .svc
            .on_task_invoke(task(Some(r#"{"code":"ls"}"#)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no active sandbox"));
        assert!(f.runner.specs().is_empty());
    }
}

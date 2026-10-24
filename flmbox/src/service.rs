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

//! The `flmbox` [`FlameService`].

use flame_rs::apis::{FlameError, TaskOutput};
use flame_rs::service::{async_trait, FlameService, Publisher, SessionContext, TaskContext};
use flame_rs::FlameMessage;

use crate::api::{Request, SandboxConfig};
use crate::sandbox::SandboxManager;

/// Runs commands in a per-session sandbox. One instance serves an executor
/// for its whole life; the sandbox belongs to the bound session.
pub struct FlmboxService {
    publisher: Publisher,
    manager: SandboxManager,
}

impl FlmboxService {
    /// Wraps the executor's sandbox manager.
    pub fn new(manager: SandboxManager) -> Self {
        Self {
            publisher: Publisher::default(),
            manager,
        }
    }
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
        self.manager.enter(&ctx.session_id, config).await
    }

    async fn on_task_invoke(&self, ctx: TaskContext) -> Result<Option<TaskOutput>, FlameError> {
        let input = ctx
            .input
            .ok_or_else(|| FlameError::InvalidConfig("task has no input".to_string()))?;
        let request: Request = serde_json::from_slice(&input)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid task input: {e}")))?;
        let result = self.manager.execute(request).await?;
        Ok(Some(result.encode()?))
    }

    async fn on_session_leave(&self) -> Result<(), FlameError> {
        self.manager.leave().await;
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
    use crate::config::{Config, ENV_IMAGES, ENV_STATE_DIR};
    use crate::runner::CommandOutput;
    use crate::testutil::{out, FakeImages, FakeNetwork, FakeRunner};

    struct Fixture {
        svc: FlmboxService,
        runner: Arc<FakeRunner>,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    fn fixture() -> Fixture {
        let state = tempfile::tempdir().unwrap();
        let rootfs = tempfile::tempdir().unwrap();
        let vars = HashMap::from([
            (
                ENV_STATE_DIR.to_string(),
                state.path().display().to_string(),
            ),
            (ENV_IMAGES.to_string(), "ubuntu:26.04".to_string()),
        ]);
        let runner = Arc::new(FakeRunner::default());
        let manager = SandboxManager::new(
            Config::from_map(&vars).unwrap(),
            "exec1",
            FakeImages::new(rootfs.path().to_path_buf()),
            runner.clone(),
            Arc::new(FakeNetwork::default()),
        );
        Fixture {
            svc: FlmboxService::new(manager),
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
            .on_task_invoke(task(Some(r#"{"kind":"run","cmd":"echo hello"}"#)))
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
            .on_task_invoke(task(Some(r#"{"kind":"run","cmd":"x"}"#)))
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
        let input =
            r#"{"kind":"run_code","code":"import sys","language":"python","input":[104,105]}"#;
        f.svc.on_task_invoke(task(Some(input))).await.unwrap();
        let specs = f.runner.specs();
        assert_eq!(specs.last().unwrap().stdin.as_deref(), Some(&b"hi"[..]));
    }

    #[tokio::test]
    async fn common_data_selects_the_image() {
        let f = fixture();
        let err = f
            .svc
            .on_session_enter(session("s1", Some(r#"{"image":"alpine:3"}"#)))
            .await
            .unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
        assert!(err.to_string().contains("not allowed"));

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
        for bad in [None, Some("nope"), Some(r#"{"kind":"nope"}"#)] {
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
            .on_task_invoke(task(Some(r#"{"kind":"run","cmd":"sleep 9","timeout":1}"#)))
            .await
            .unwrap()
            .unwrap();
        let result = CommandResult::decode(&output).unwrap();
        assert_eq!(result.exit_code, 124);
        assert_eq!(result.stderr, b"timed out after 1s");
    }
}

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

//! Thin wrapper over the `runsc` command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use flame_rs::apis::FlameError;

use crate::config::NetworkMode;
use crate::runner::{CommandOutput, CommandRunner, CommandSpec};

/// Upper bound of `create`, `start`, `kill`, `delete`, `state` and `list`.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
/// Output cap of the control commands.
const CONTROL_MAX_OUTPUT: usize = 1 << 20;

/// Messages of `runsc` that mean the container is already gone.
const GONE_MARKERS: [&str; 5] = [
    "does not exist",
    "not found",
    "no such",
    "not running",
    "stopped",
];

/// A `runsc` installation and the global flags every command gets.
#[derive(Clone)]
pub struct Runsc {
    /// Path or name of the binary.
    pub bin: String,
    /// `--root`: where `runsc` keeps container state.
    pub root: PathBuf,
    /// `--network`.
    pub network: NetworkMode,
    /// `--platform`, when set.
    pub platform: Option<String>,
    /// `--overlay2` value, e.g. `root:memory`.
    pub overlay: String,
    /// Runs the commands.
    pub runner: Arc<dyn CommandRunner>,
}

impl Runsc {
    /// Global flags placed before the subcommand.
    fn global_args(&self) -> Vec<String> {
        let network = match self.network {
            NetworkMode::Sandbox => "sandbox",
            NetworkMode::None => "none",
        };
        let mut args = vec![
            "--root".to_string(),
            self.root.display().to_string(),
            format!("--network={network}"),
        ];
        if let Some(platform) = &self.platform {
            args.push(format!("--platform={platform}"));
        }
        args.push(format!("--overlay2={}", self.overlay));
        args
    }

    fn spec(&self, sub: Vec<String>) -> CommandSpec {
        let mut args = self.global_args();
        args.extend(sub);
        CommandSpec {
            program: self.bin.clone(),
            args,
            timeout: Some(CONTROL_TIMEOUT),
            max_output: CONTROL_MAX_OUTPUT,
            ..CommandSpec::default()
        }
    }

    async fn control(&self, what: &str, sub: Vec<String>) -> Result<CommandOutput, FlameError> {
        let out = self.runner.run(self.spec(sub)).await?;
        if out.timed_out {
            return Err(FlameError::Internal(format!("runsc {what} timed out")));
        }
        if out.exit_code != 0 {
            return Err(failure(what, &out));
        }
        Ok(out)
    }

    /// `runsc create --bundle <dir> <id>`.
    pub async fn create(&self, id: &str, bundle: &Path) -> Result<(), FlameError> {
        let sub = vec![
            "create".to_string(),
            "--bundle".to_string(),
            bundle.display().to_string(),
            id.to_string(),
        ];
        self.control("create", sub).await.map(|_| ())
    }

    /// `runsc start <id>`.
    pub async fn start(&self, id: &str) -> Result<(), FlameError> {
        let sub = vec!["start".to_string(), id.to_string()];
        self.control("start", sub).await.map(|_| ())
    }

    /// `runsc exec [--cwd C] [--env K=V]... <id> <argv...>`. The returned exit
    /// code is the one of the command inside the sandbox.
    pub async fn exec(
        &self,
        id: &str,
        argv: &[String],
        opts: ExecOptions,
    ) -> Result<CommandOutput, FlameError> {
        let mut sub = vec!["exec".to_string()];
        if let Some(cwd) = opts.cwd {
            sub.push("--cwd".to_string());
            sub.push(cwd);
        }
        for (k, v) in opts.envs {
            sub.push("--env".to_string());
            sub.push(format!("{k}={v}"));
        }
        sub.push(id.to_string());
        sub.extend(argv.iter().cloned());

        let mut spec = self.spec(sub);
        spec.stdin = opts.stdin;
        spec.timeout = opts.timeout;
        spec.max_output = opts.max_output;
        self.runner.run(spec).await
    }

    /// `runsc kill <id> <signal>`; an unknown or stopped container is success.
    pub async fn kill(&self, id: &str, signal: &str) -> Result<(), FlameError> {
        let sub = vec!["kill".to_string(), id.to_string(), signal.to_string()];
        self.idempotent("kill", sub).await
    }

    /// `runsc delete --force <id>`; an unknown container is success.
    pub async fn delete(&self, id: &str) -> Result<(), FlameError> {
        let sub = vec!["delete".to_string(), "--force".to_string(), id.to_string()];
        self.idempotent("delete", sub).await
    }

    async fn idempotent(&self, what: &str, sub: Vec<String>) -> Result<(), FlameError> {
        let out = self.runner.run(self.spec(sub)).await?;
        if out.timed_out {
            return Err(FlameError::Internal(format!("runsc {what} timed out")));
        }
        if out.exit_code == 0 {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if GONE_MARKERS.iter().any(|m| stderr.contains(m)) {
            return Ok(());
        }
        Err(failure(what, &out))
    }

    /// `runsc state <id>`: the `status` of the container (`created`,
    /// `running`, `stopped`, ...).
    pub async fn state(&self, id: &str) -> Result<String, FlameError> {
        let out = self
            .control("state", vec!["state".to_string(), id.to_string()])
            .await?;
        let value: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| FlameError::Internal(format!("runsc state: invalid JSON: {e}")))?;
        value
            .get("status")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| FlameError::Internal("runsc state: no status field".to_string()))
    }

    /// `runsc list`: the ids of the containers under `--root`.
    pub async fn list(&self) -> Result<Vec<String>, FlameError> {
        let out = self.control("list", vec!["list".to_string()]).await?;
        Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// Options of [`Runsc::exec`].
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Working directory inside the sandbox.
    pub cwd: Option<String>,
    /// Extra environment variables.
    pub envs: Vec<(String, String)>,
    /// Standard input of the command.
    pub stdin: Option<Vec<u8>>,
    /// Kill the `runsc exec` process after this duration.
    pub timeout: Option<Duration>,
    /// Maximum bytes kept per output stream.
    pub max_output: usize,
}

fn failure(what: &str, out: &CommandOutput) -> FlameError {
    let stderr = String::from_utf8_lossy(&out.stderr);
    FlameError::Internal(format!(
        "runsc {what} failed (exit {}): {}",
        out.exit_code,
        stderr.trim()
    ))
}

/// Parses `runsc list` output: a JSON array of objects with an `id` field, or
/// the plain table (`ID PID STATUS BUNDLE CREATED OWNER`) with a header line.
fn parse_list(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if text.starts_with('[') || text == "null" {
        if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(text) {
            return items
                .iter()
                .filter_map(|item| match item {
                    serde_json::Value::String(id) => Some(id.clone()),
                    _ => item
                        .get("id")
                        .or_else(|| item.get("ID"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
                .collect();
        }
        return Vec::new();
    }
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|first| *first != "ID")
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{out, FakeRunner};

    fn runsc(runner: &Arc<FakeRunner>) -> Runsc {
        Runsc {
            bin: "/usr/bin/runsc".to_string(),
            root: PathBuf::from("/state/runsc"),
            network: NetworkMode::Sandbox,
            platform: None,
            overlay: "root:memory".to_string(),
            runner: runner.clone(),
        }
    }

    const G: &str = "--root /state/runsc --network=sandbox --overlay2=root:memory";

    #[tokio::test]
    async fn create_start_kill_delete_arguments() {
        let fake = Arc::new(FakeRunner::default());
        let r = runsc(&fake);
        r.create("sb1", Path::new("/b/bundle")).await.unwrap();
        r.start("sb1").await.unwrap();
        r.kill("sb1", "SIGKILL").await.unwrap();
        r.delete("sb1").await.unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                format!("/usr/bin/runsc {G} create --bundle /b/bundle sb1"),
                format!("/usr/bin/runsc {G} start sb1"),
                format!("/usr/bin/runsc {G} kill sb1 SIGKILL"),
                format!("/usr/bin/runsc {G} delete --force sb1"),
            ]
        );
    }

    #[tokio::test]
    async fn global_flags_follow_the_configuration() {
        let fake = Arc::new(FakeRunner::default());
        let mut r = runsc(&fake);
        r.network = NetworkMode::None;
        r.platform = Some("kvm".to_string());
        r.overlay = "all:dir=/var/tmp".to_string();
        r.start("x").await.unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                "/usr/bin/runsc --root /state/runsc --network=none --platform=kvm \
                 --overlay2=all:dir=/var/tmp start x"
            ]
        );
    }

    #[tokio::test]
    async fn exec_arguments_stdin_and_timeout() {
        let fake = Arc::new(FakeRunner::default());
        fake.respond(|_| Ok(out(3, "o", "e")));
        let r = runsc(&fake);
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), "ls /".to_string()];
        let res = r
            .exec(
                "sb1",
                &argv,
                ExecOptions {
                    cwd: Some("/work".to_string()),
                    envs: vec![("A".to_string(), "1".to_string())],
                    stdin: Some(b"in".to_vec()),
                    timeout: Some(Duration::from_secs(5)),
                    max_output: 77,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.exit_code, 3);
        assert_eq!(res.stdout, b"o");
        assert_eq!(
            fake.lines(),
            vec![format!(
                "/usr/bin/runsc {G} exec --cwd /work --env A=1 sb1 /bin/sh -c ls /"
            )]
        );
        let spec = &fake.specs()[0];
        assert_eq!(spec.stdin.as_deref(), Some(&b"in"[..]));
        assert_eq!(spec.timeout, Some(Duration::from_secs(5)));
        assert_eq!(spec.max_output, 77);
    }

    #[tokio::test]
    async fn exec_without_options_has_no_flags() {
        let fake = Arc::new(FakeRunner::default());
        let r = runsc(&fake);
        r.exec("sb1", &["true".to_string()], ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(
            fake.lines(),
            vec![format!("/usr/bin/runsc {G} exec sb1 true")]
        );
    }

    #[tokio::test]
    async fn failures_carry_stderr() {
        let fake = Arc::new(FakeRunner::default());
        fake.respond(|_| Ok(out(1, "", "boom: bad bundle\n")));
        let r = runsc(&fake);
        let err = r.create("sb1", Path::new("/b")).await.unwrap_err();
        assert!(matches!(err, FlameError::Internal(_)));
        assert!(err.to_string().contains("boom: bad bundle"), "{err}");
        assert!(r.start("sb1").await.is_err());
    }

    #[tokio::test]
    async fn timeout_of_a_control_command_is_an_error() {
        let fake = Arc::new(FakeRunner::default());
        fake.respond(|_| {
            Ok(CommandOutput {
                timed_out: true,
                exit_code: 137,
                ..CommandOutput::default()
            })
        });
        let err = runsc(&fake).start("sb1").await.unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn kill_and_delete_of_unknown_ids_succeed() {
        let fake = Arc::new(FakeRunner::default());
        fake.respond(|_| Ok(out(1, "", "container \"sb1\" does not exist\n")));
        let r = runsc(&fake);
        r.kill("sb1", "SIGKILL").await.unwrap();
        r.delete("sb1").await.unwrap();

        fake.respond(|_| Ok(out(1, "", "cannot signal: container is not running")));
        r.kill("sb1", "SIGKILL").await.unwrap();

        fake.respond(|_| Ok(out(1, "", "permission denied")));
        assert!(r.kill("sb1", "SIGKILL").await.is_err());
        assert!(r.delete("sb1").await.is_err());
    }

    #[tokio::test]
    async fn state_parses_the_status() {
        let fake = Arc::new(FakeRunner::default());
        fake.respond(|_| {
            Ok(out(
                0,
                r#"{"ociVersion":"1.0.2","id":"sb1","status":"running","pid":4}"#,
                "",
            ))
        });
        let r = runsc(&fake);
        assert_eq!(r.state("sb1").await.unwrap(), "running");
        assert_eq!(fake.lines(), vec![format!("/usr/bin/runsc {G} state sb1")]);

        fake.respond(|_| Ok(out(0, "{}", "")));
        assert!(r.state("sb1").await.is_err());
        fake.respond(|_| Ok(out(0, "not json", "")));
        assert!(r.state("sb1").await.is_err());
        fake.respond(|_| Ok(out(1, "", "container does not exist")));
        assert!(r.state("sb1").await.is_err());
    }

    #[test]
    fn list_parser_accepts_json_and_table() {
        assert!(parse_list("").is_empty());
        assert!(parse_list("null").is_empty());
        assert!(parse_list("[]").is_empty());
        assert_eq!(
            parse_list(r#"[{"id":"a","pid":1},{"id":"b"},"c"]"#),
            vec!["a", "b", "c"]
        );
        let table = "ID\tPID\tSTATUS\tBUNDLE\tCREATED\tOWNER\n\
                     flmbox-e1.s1\t12\trunning\t/b\t2026-01-01T00:00:00Z\troot\n\
                     other\t0\tcreated\t/c\t2026-01-01T00:00:00Z\troot\n";
        assert_eq!(parse_list(table), vec!["flmbox-e1.s1", "other"]);
        assert_eq!(
            parse_list("ID PID STATUS BUNDLE CREATED OWNER\n"),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn list_runs_the_list_subcommand() {
        let fake = Arc::new(FakeRunner::default());
        fake.respond(|_| Ok(out(0, "ID PID STATUS\nsb1 1 running\n", "")));
        let ids = runsc(&fake).list().await.unwrap();
        assert_eq!(ids, vec!["sb1"]);
        assert_eq!(fake.lines(), vec![format!("/usr/bin/runsc {G} list")]);
    }
}

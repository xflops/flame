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

//! Process execution behind a trait, so the `runsc` wrapper can be tested
//! without `runsc`.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use flame_rs::apis::FlameError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// How long the output readers get to finish after the process ended or was
/// killed; a daemonized grandchild may keep the pipes open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A command to run.
#[derive(Debug, Clone, Default)]
pub struct CommandSpec {
    /// Program to execute.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub envs: Vec<(String, String)>,
    /// Working directory of the process.
    pub cwd: Option<PathBuf>,
    /// Bytes written to the standard input, which is then closed. Without it
    /// the standard input is empty.
    pub stdin: Option<Vec<u8>>,
    /// The process is killed after this duration.
    pub timeout: Option<Duration>,
    /// Maximum number of bytes kept per output stream.
    pub max_output: usize,
}

/// The outcome of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code; `128 + signal` when the process died from a signal.
    pub exit_code: i32,
    /// Captured standard output (at most `max_output` bytes).
    pub stdout: Vec<u8>,
    /// Captured standard error (at most `max_output` bytes).
    pub stderr: Vec<u8>,
    /// The process was killed because the timeout elapsed.
    pub timed_out: bool,
    /// An output stream exceeded `max_output` and was cut.
    pub truncated: bool,
}

/// Runs commands.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Runs the command to completion. `Err` is only for failing to run it at
    /// all; a nonzero exit code is a normal result.
    async fn run(&self, spec: CommandSpec) -> Result<CommandOutput, FlameError>;
}

/// Runs commands as child processes of this process.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioRunner;

#[derive(Default)]
struct Capture {
    buf: Mutex<Vec<u8>>,
    truncated: AtomicBool,
}

impl Capture {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Reads to the end, keeping at most `max` bytes; the excess is discarded so
/// the child never blocks on a full pipe.
async fn pump<R: AsyncRead + Unpin>(mut reader: R, capture: Arc<Capture>, max: usize) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut buf = capture.buf.lock().unwrap_or_else(PoisonError::into_inner);
                let take = n.min(max.saturating_sub(buf.len()));
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    capture.truncated.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

async fn drain(mut handle: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(DRAIN_GRACE, &mut handle)
        .await
        .is_err()
    {
        handle.abort();
    }
}

#[async_trait]
impl CommandRunner for TokioRunner {
    async fn run(&self, spec: CommandSpec) -> Result<CommandOutput, FlameError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .envs(spec.envs.iter().map(|(k, v)| (k, v)))
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| FlameError::Internal(format!("failed to run '{}': {e}", spec.program)))?;

        let stdin_task = match (child.stdin.take(), spec.stdin) {
            (Some(mut stdin), Some(data)) => Some(tokio::spawn(async move {
                // The child may exit without reading its input; that is fine.
                let _ = stdin.write_all(&data).await;
                let _ = stdin.shutdown().await;
            })),
            _ => None,
        };

        let out = Arc::new(Capture::default());
        let err = Arc::new(Capture::default());
        let out_task = child
            .stdout
            .take()
            .map(|r| tokio::spawn(pump(r, out.clone(), spec.max_output)));
        let err_task = child
            .stderr
            .take()
            .map(|r| tokio::spawn(pump(r, err.clone(), spec.max_output)));

        let wait_err = |e: std::io::Error| {
            FlameError::Internal(format!("failed to wait for '{}': {e}", spec.program))
        };
        let (status, timed_out) = match spec.timeout {
            Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
                Ok(status) => (status.map_err(wait_err)?, false),
                Err(_) => {
                    let _ = child.kill().await;
                    (child.wait().await.map_err(wait_err)?, true)
                }
            },
            None => (child.wait().await.map_err(wait_err)?, false),
        };

        if let Some(task) = stdin_task {
            task.abort();
        }
        for task in [out_task, err_task].into_iter().flatten() {
            drain(task).await;
        }

        Ok(CommandOutput {
            exit_code: exit_code(status),
            stdout: out.take(),
            stderr: err.take(),
            timed_out,
            truncated: out.truncated.load(Ordering::Relaxed)
                || err.truncated.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> CommandSpec {
        CommandSpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            max_output: 1 << 20,
            ..CommandSpec::default()
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let out = TokioRunner.run(sh("echo out; echo err >&2")).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert!(!out.timed_out && !out.truncated);
    }

    #[tokio::test]
    async fn passes_args_envs_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = sh("echo \"$FOO $1\"; pwd");
        spec.args.push("arg0".to_string());
        spec.args.push("x".to_string());
        spec.envs.push(("FOO".to_string(), "bar".to_string()));
        spec.cwd = Some(dir.path().to_path_buf());
        let out = TokioRunner.run(spec).await.unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("bar x"));
        let pwd = std::fs::canonicalize(lines.next().unwrap()).unwrap();
        assert_eq!(pwd, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[tokio::test]
    async fn stdin_round_trip() {
        let mut spec = CommandSpec {
            program: "cat".to_string(),
            max_output: 1 << 20,
            ..CommandSpec::default()
        };
        spec.stdin = Some(b"hello stdin".to_vec());
        let out = TokioRunner.run(spec).await.unwrap();
        assert_eq!(out.stdout, b"hello stdin");
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn large_stdin_does_not_deadlock() {
        let data = vec![b'x'; 1 << 20];
        let spec = CommandSpec {
            program: "cat".to_string(),
            stdin: Some(data.clone()),
            max_output: 4 << 20,
            ..CommandSpec::default()
        };
        let out = TokioRunner.run(spec).await.unwrap();
        assert_eq!(out.stdout, data);
    }

    #[tokio::test]
    async fn no_stdin_means_empty_input() {
        let spec = CommandSpec {
            program: "cat".to_string(),
            max_output: 1024,
            ..CommandSpec::default()
        };
        let out = TokioRunner.run(spec).await.unwrap();
        assert!(out.stdout.is_empty());
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_result() {
        let out = TokioRunner.run(sh("echo bad >&2; exit 7")).await.unwrap();
        assert_eq!(out.exit_code, 7);
        assert_eq!(out.stderr, b"bad\n");
    }

    #[tokio::test]
    async fn signal_death_maps_to_128_plus_signal() {
        let out = TokioRunner.run(sh("kill -9 $$")).await.unwrap();
        assert_eq!(out.exit_code, 137);
    }

    #[tokio::test]
    async fn timeout_kills_and_keeps_partial_output() {
        let mut spec = sh("echo partial; exec sleep 30");
        spec.timeout = Some(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let out = TokioRunner.run(spec).await.unwrap();
        assert!(out.timed_out);
        assert_eq!(out.stdout, b"partial\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn output_is_capped() {
        let mut spec = sh("head -c 100000 /dev/zero | tr '\\0' a; echo e >&2");
        spec.max_output = 1000;
        let out = TokioRunner.run(spec).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.len(), 1000);
        assert_eq!(out.stderr, b"e\n");
        assert!(out.truncated);
    }

    #[tokio::test]
    async fn missing_program_is_an_error() {
        let spec = CommandSpec {
            program: "/nonexistent/flmbox-test-program".to_string(),
            max_output: 10,
            ..CommandSpec::default()
        };
        let err = TokioRunner.run(spec).await.unwrap_err();
        assert!(matches!(err, FlameError::Internal(_)));
        assert!(err.to_string().contains("flmbox-test-program"));
    }
}

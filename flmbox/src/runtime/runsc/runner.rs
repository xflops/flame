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

use crate::runtime::CommandOutput;

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
    /// Appends stdout and stderr to this file instead of reading them through
    /// pipes, and gives the process no input. Only what this command appended is
    /// returned, as `stderr`, for the diagnostics of a failure.
    ///
    /// A command that creates a long-lived process (`runsc create` hands its
    /// stdio to the container's init process) must not own pipes: they stay open
    /// as long as that process lives, so their end never comes. With a file there
    /// is no stream to wait for, and the caller keeps the file as the log of that
    /// process.
    pub log: Option<PathBuf>,
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

/// Waits for the readers to reach the end of their streams, at most `grace` in
/// total; a reader that is still going is aborted, which closes its pipe.
async fn drain(handles: Vec<tokio::task::JoinHandle<()>>, grace: Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    for mut handle in handles {
        if tokio::time::timeout_at(deadline, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
        }
    }
}

/// Up to `max` bytes of the file from `start`, without touching its offset (the
/// child shares it).
fn read_capture(file: &std::fs::File, start: u64, max: usize) -> (Vec<u8>, bool) {
    use std::os::unix::fs::FileExt;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let available = len.saturating_sub(start);
    let keep = usize::try_from(available).unwrap_or(usize::MAX).min(max);
    let mut buf = vec![0u8; keep];
    let read = file.read_at(&mut buf, start).unwrap_or(0);
    buf.truncate(read);
    (buf, (keep as u64) < available)
}

fn capture_error(e: std::io::Error) -> FlameError {
    FlameError::Internal(format!("failed to create a capture file: {e}"))
}

#[async_trait]
impl CommandRunner for TokioRunner {
    async fn run(&self, spec: CommandSpec) -> Result<CommandOutput, FlameError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .envs(spec.envs.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true);
        let capture: Option<(std::fs::File, u64)> = if let Some(path) = &spec.log {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(path)
                .map_err(capture_error)?;
            let start = file.metadata().map(|m| m.len()).unwrap_or(0);
            let out_end = file.try_clone().map_err(capture_error)?;
            let err_end = file.try_clone().map_err(capture_error)?;
            cmd.stdout(Stdio::from(out_end))
                .stderr(Stdio::from(err_end));
            Some((file, start))
        } else {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            None
        };
        cmd.stdin(if spec.stdin.is_some() && capture.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
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
        drain(
            [out_task, err_task].into_iter().flatten().collect(),
            DRAIN_GRACE,
        )
        .await;

        let mut truncated =
            out.truncated.load(Ordering::Relaxed) || err.truncated.load(Ordering::Relaxed);
        let stderr = match &capture {
            Some((file, start)) => {
                let (bytes, cut) = read_capture(file, *start, spec.max_output);
                truncated |= cut;
                bytes
            }
            None => err.take(),
        };

        Ok(CommandOutput {
            exit_code: exit_code(status),
            stdout: out.take(),
            stderr,
            timed_out,
            truncated,
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

    fn logged(script: &str, path: &std::path::Path) -> CommandSpec {
        CommandSpec {
            log: Some(path.to_path_buf()),
            max_output: 1000,
            ..sh(script)
        }
    }

    #[tokio::test]
    async fn log_does_not_wait_for_a_process_that_keeps_the_stdio() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sandbox.log");
        // The shell exits at once; its background child keeps stdout and stderr open.
        let started = std::time::Instant::now();
        let out = TokioRunner
            .run(logged("echo out; echo err >&2; sleep 30 &", &log))
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty());
        assert_eq!(out.stderr, b"out\nerr\n");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(std::fs::read(&log).unwrap(), b"out\nerr\n");
    }

    #[tokio::test]
    async fn log_appends_and_returns_only_the_new_output() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sandbox.log");
        TokioRunner.run(logged("echo first", &log)).await.unwrap();
        let out = TokioRunner
            .run(logged("echo bad >&2; exit 7", &log))
            .await
            .unwrap();
        assert_eq!(out.exit_code, 7);
        assert_eq!(out.stderr, b"bad\n");
        assert_eq!(std::fs::read(&log).unwrap(), b"first\nbad\n");
    }

    #[tokio::test]
    async fn log_output_is_capped_but_the_file_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sandbox.log");
        let mut spec = logged("head -c 5000 /dev/zero | tr '\\0' e >&2", &log);
        spec.max_output = 100;
        let out = TokioRunner.run(spec).await.unwrap();
        assert_eq!(out.stderr.len(), 100);
        assert!(out.truncated);
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 5000);
    }

    #[tokio::test]
    async fn log_gives_no_input() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sandbox.log");
        let out = TokioRunner
            .run(logged("cat; echo done", &log))
            .await
            .unwrap();
        assert_eq!(out.stderr, b"done\n");
    }

    #[tokio::test]
    async fn readers_share_one_drain_deadline() {
        let never = || tokio::spawn(std::future::pending::<()>());
        let started = std::time::Instant::now();
        drain(vec![never(), never()], Duration::from_millis(300)).await;
        // Two readers that never finish take one grace together, not two.
        assert!(started.elapsed() < Duration::from_millis(500));
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

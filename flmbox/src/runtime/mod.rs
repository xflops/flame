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

//! The sandbox runtimes behind [`Sandbox`](crate::sandbox::Sandbox).
//!
//! gVisor (`runsc`) is the first implementation; a microVM runtime such as
//! Firecracker implements the same trait. The runtime owns everything specific
//! to it: how the root filesystem and the sandbox definition are laid out, how
//! a sandbox is started, entered and removed.

pub mod runsc;

/// File name of a sandbox's log, in the sandbox directory: where the stdout and
/// stderr of its init process go.
pub const LOG_FILE: &str = "sandbox.log";

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use flame_rs::apis::FlameError;

use crate::config::{FlameRuntime, FlameSandbox};
use crate::images::ImageRoot;

/// What a runtime needs to start one sandbox.
pub struct SandboxSpec<'a> {
    /// Sandbox id, `[A-Za-z0-9_.-]`, validated by the caller.
    pub id: &'a str,
    /// A directory of this sandbox for the runtime's own files; the caller
    /// removes it when the sandbox is killed.
    pub dir: &'a Path,
    /// The unpacked image: a read-only root filesystem and the image config.
    pub image: &'a ImageRoot,
}

/// A sandbox the runtime knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInfo {
    /// Sandbox id.
    pub id: String,
    /// The runtime's state: `running`, `created`, `stopped`, ...
    pub status: String,
    /// Creation time as the runtime reports it, when it does.
    pub created: Option<String>,
}

/// The outcome of a command run in a sandbox.
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

/// Options of [`Runtime::exec`].
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Working directory inside the sandbox.
    pub cwd: Option<String>,
    /// Extra environment variables.
    pub envs: Vec<(String, String)>,
    /// Standard input of the command.
    pub stdin: Option<Vec<u8>>,
    /// Stop the command after this duration.
    pub timeout: Option<Duration>,
    /// Maximum bytes kept per output stream.
    pub max_output: usize,
}

/// Creates, enters and removes isolated sandboxes. Implementations keep their
/// state outside the process (the runtime's own state directory), so that
/// several processes reach the same sandboxes.
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Creates and starts a sandbox with a long-lived init process.
    async fn start(&self, spec: &SandboxSpec<'_>) -> Result<(), FlameError>;

    /// Runs a command in the sandbox. The exit code is the command's own.
    async fn exec(
        &self,
        id: &str,
        argv: &[String],
        opts: ExecOptions,
    ) -> Result<CommandOutput, FlameError>;

    /// Stops and deletes the sandbox. An unknown sandbox is success. Every
    /// step runs; the first failure is returned.
    async fn kill(&self, id: &str) -> Result<(), FlameError>;

    /// The sandboxes the runtime knows.
    async fn list(&self) -> Result<Vec<SandboxInfo>, FlameError>;
}

/// The runtime named by the configuration.
pub fn runtime_for(cfg: &FlameSandbox) -> Arc<dyn Runtime> {
    match &cfg.runtime {
        FlameRuntime::Runsc(runsc) => {
            Arc::new(runsc::Runsc::new(runsc, &cfg.state_dir, cfg.network))
        }
    }
}

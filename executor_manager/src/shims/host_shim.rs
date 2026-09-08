/*
Copyright 2025 The Flame Authors.
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

use std::collections::HashMap;
use std::env;
use std::fs::{self, create_dir_all, OpenOptions};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
#[cfg(unix)]
use nix::sys::signal::{killpg, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use stdng::{logs::TraceFn, trace_fn};
use tokio::sync::Mutex;

use crate::executor::Executor;
use crate::shims::grpc_shim::GrpcShim;
use crate::shims::{ExecutorWorkDir, Shim, ShimPtr};
use ::rpc::flame::v1 as rpc;
use common::apis::{ApplicationContext, SessionContext, TaskContext, TaskOutput, TaskResult};
use common::{
    FlameError, FLAME_CACHE_ENDPOINT, FLAME_CA_FILE, FLAME_ENDPOINT, FLAME_INSTANCE_ENDPOINT,
    FLAME_LOG,
};

struct HostInstance {
    child: tokio::process::Child,
}

impl HostInstance {
    fn new(child: tokio::process::Child) -> Self {
        Self { child }
    }

    /// Kill the child process
    #[cfg(unix)]
    fn kill_process(&mut self) {
        if let Some(id) = self.child.id() {
            let ig = Pid::from_raw(id as i32);
            let _ = killpg(ig, Signal::SIGTERM);
            tracing::debug!("Killed process group <{}>", id);
        } else {
            drop(self.child.kill());
            tracing::debug!("Killed child process");
        }
    }

    #[cfg(not(unix))]
    fn kill_process(&mut self) {
        drop(self.child.kill());
        tracing::debug!("Killed child process");
    }
}

pub struct HostShim {
    instance: HostInstance,
    instance_client: GrpcShim,
    work_dir: ExecutorWorkDir,
}

const RUST_LOG: &str = "RUST_LOG";
const DEFAULT_SVC_LOG_LEVEL: &str = "info";

impl HostShim {
    pub async fn new_ptr(
        executor: &Executor,
        app: &ApplicationContext,
        install_env_vars: &HashMap<String, String>,
    ) -> Result<ShimPtr, FlameError> {
        trace_fn!("HostShim::new_ptr");

        // Create work directory first - it provides socket path for GrpcShim
        let work_dir = ExecutorWorkDir::new(app, &executor.id)?;

        let mut instance_client = GrpcShim::new(&work_dir)?;

        let instance = Self::launch_instance(app, executor, &work_dir, install_env_vars)?;

        instance_client.connect().await?;

        Ok(Arc::new(Mutex::new(Self {
            instance,
            instance_client,
            work_dir,
        })))
    }

    fn create_dir(path: &Path, name: &str) -> Result<(), FlameError> {
        create_dir_all(path).map_err(|e| {
            FlameError::Internal(format!(
                "failed to create {} directory {}: {e}",
                name,
                path.display()
            ))
        })
    }

    /// Setup working directory and tmp directory for an application instance (per-instance).
    fn setup_working_directory(work_dir: &Path) -> Result<HashMap<String, String>, FlameError> {
        trace_fn!("HostShim::setup_working_directory");

        let tmp_dir = work_dir.join("tmp");

        tracing::debug!(
            "Working directory of application instance: {}",
            work_dir.display()
        );
        tracing::debug!(
            "Temporary directory of application instance: {}",
            tmp_dir.display()
        );

        Self::create_dir(work_dir, "working")?;
        Self::create_dir(&tmp_dir, "temporary")?;

        let mut envs = HashMap::new();
        envs.insert("TMPDIR".to_string(), tmp_dir.to_string_lossy().to_string());
        envs.insert("TEMP".to_string(), tmp_dir.to_string_lossy().to_string());
        envs.insert("TMP".to_string(), tmp_dir.to_string_lossy().to_string());

        Ok(envs)
    }

    /// Expand environment variables in a string
    /// Supports both ${VAR} and $VAR syntax
    fn expand_env_vars(s: &str, envs: Option<&HashMap<String, String>>) -> String {
        match envs {
            Some(envs) => shellexpand::env_with_context_no_errors(s, |key| {
                envs.get(key).cloned().or_else(|| env::var(key).ok())
            })
            .into_owned(),
            None => shellexpand::env(s)
                .unwrap_or(std::borrow::Cow::Borrowed(s))
                .into_owned(),
        }
    }

    fn launch_instance(
        app: &ApplicationContext,
        executor: &Executor,
        work_dir: &ExecutorWorkDir,
        install_env_vars: &HashMap<String, String>,
    ) -> Result<HostInstance, FlameError> {
        trace_fn!("HostShim::launch_instance");

        let command = app.command.clone().unwrap_or_default();
        let args = app.arguments.clone();

        let log_level = env::var(RUST_LOG).unwrap_or(String::from(DEFAULT_SVC_LOG_LEVEL));

        // Expand environment variables in the application's environment settings
        let mut envs: HashMap<String, String> = app
            .environments
            .iter()
            .map(|(k, v)| (k.clone(), Self::expand_env_vars(v, None)))
            .collect();
        envs.insert(RUST_LOG.to_string(), log_level.clone());
        envs.insert(FLAME_LOG.to_string(), log_level);
        envs.insert(
            FLAME_INSTANCE_ENDPOINT.to_string(),
            work_dir.socket().to_string_lossy().to_string(),
        );
        if let Some(context) = &executor.context {
            // Pass session manager endpoint for recursive runner calls
            envs.insert(FLAME_ENDPOINT.to_string(), context.cluster.endpoint.clone());
            // Pass CA file for TLS certificate verification
            if let Some(ref tls) = context.cluster.tls {
                if let Some(ref ca_file) = tls.ca_file {
                    envs.insert(FLAME_CA_FILE.to_string(), ca_file.clone());
                }
            }
            if let Some(cache) = &context.cache {
                envs.insert(FLAME_CACHE_ENDPOINT.to_string(), cache.endpoint.clone());
            }
        }

        // Propagate HOME environment variable to ensure Python finds user site-packages
        // This is needed when flamepy is installed with --user flag for the flame user
        if let Ok(home) = env::var("HOME") {
            envs.entry("HOME".to_string()).or_insert(home);
        }

        // Merge app-specific install environment variables (from appmgr)
        for (key, value) in install_env_vars {
            if Self::is_path_env(key) {
                envs.entry(key.clone())
                    .and_modify(|e| *e = format!("{}:{}", value, e))
                    .or_insert(value.clone());
            } else {
                envs.entry(key.clone()).or_insert(value.clone());
            }
        }

        let command = Self::expand_env_vars(&command, Some(&envs));
        let args: Vec<String> = args
            .iter()
            .map(|arg| Self::expand_env_vars(arg, Some(&envs)))
            .collect();

        tracing::debug!(
            "Try to start service by command <{command}> with args <{args:?}> and envs <{envs:?}>"
        );

        // Spawn child process
        let mut cmd = tokio::process::Command::new(&command);

        // Use app_dir for temp files (per-instance isolation)
        let app_work_dir = work_dir.app_dir();
        // Use process_dir for actual process working directory and stdout/stderr logs
        let process_work_dir = work_dir.process_dir();

        // Setup working directory and tmp (per-instance)
        let work_dir_envs = Self::setup_working_directory(app_work_dir)?;
        for (key, value) in work_dir_envs {
            envs.entry(key).or_insert(value);
        }

        let log_out = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(process_work_dir.join(format!("{}.out", executor.id)))
            .map_err(|e| FlameError::Internal(format!("failed to open stdout log file: {e}")))?;

        let log_err = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(process_work_dir.join(format!("{}.err", executor.id)))
            .map_err(|e| FlameError::Internal(format!("failed to open stderr log file: {e}")))?;

        #[cfg(unix)]
        let child = cmd
            .envs(envs)
            .args(args)
            .current_dir(process_work_dir)
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err))
            .process_group(0)
            .spawn()
            .map_err(|e| {
                FlameError::InvalidConfig(format!(
                    "failed to start service by command <{command}>: {e}"
                ))
            })?;

        #[cfg(not(unix))]
        let child = cmd
            .envs(envs)
            .args(args)
            .current_dir(process_work_dir)
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err))
            .spawn()
            .map_err(|e| {
                FlameError::InvalidConfig(format!(
                    "failed to start service by command <{command}>: {e}"
                ))
            })?;

        Ok(HostInstance::new(child))
    }

    fn is_path_env(key: &str) -> bool {
        matches!(key, "PATH" | "PYTHONPATH" | "LD_LIBRARY_PATH")
    }
}

impl Drop for HostShim {
    fn drop(&mut self) {
        // 1. Close gRPC connection first
        self.instance_client.close();
        // 2. Kill child process
        self.instance.kill_process();
        // 3. Cleanup is handled by ExecutorWorkDir::drop()
    }
}

#[async_trait]
impl Shim for HostShim {
    async fn on_session_enter(
        &mut self,
        ctx: &SessionContext,
    ) -> Result<super::SessionEnterResponse, FlameError> {
        trace_fn!("HostShim::on_session_enter");

        self.instance_client.on_session_enter(ctx).await
    }

    async fn on_task_invoke(
        &mut self,
        ctx: &TaskContext,
    ) -> Result<super::TaskInvokeResponse, FlameError> {
        trace_fn!("HostShim::on_task_invoke");

        self.instance_client.on_task_invoke(ctx).await
    }

    async fn on_session_leave(&mut self) -> Result<(), FlameError> {
        trace_fn!("HostShim::on_session_leave");

        self.instance_client.on_session_leave().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_command_args_from_launch_env() {
        let envs = HashMap::from([("FLAME_PYTHON_VERSION".to_string(), "3.12".to_string())]);

        assert_eq!(
            HostShim::expand_env_vars("python${FLAME_PYTHON_VERSION}", Some(&envs)),
            "python3.12"
        );
    }
}

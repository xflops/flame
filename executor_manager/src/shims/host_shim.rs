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

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, create_dir_all, OpenOptions};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
#[cfg(unix)]
use nix::sys::signal::{killpg, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use stdng::{logs::TraceFn, trace_fn};
use tokio::sync::Mutex;

use crate::appmgr::ApplicationManager;
use crate::executor::Executor;
use crate::shims::grpc_shim::GrpcShim;
use crate::shims::{ExecutorWorkDir, Shim, ShimPtr};
use ::rpc::flame::v1 as rpc;
use common::apis::{ApplicationContext, SessionContext, TaskContext, TaskOutput, TaskResult};
use common::{
    get_python_runtime, FlameError, FLAME_CACHE_ENDPOINT, FLAME_CA_FILE, FLAME_ENDPOINT,
    FLAME_INSTANCE_ENDPOINT, FLAME_LOG, FLAME_PYTHON_VERSION_ENV,
};

struct HostInstance {
    child: tokio::process::Child,
    #[cfg(unix)]
    process_group: Option<Pid>,
}

impl HostInstance {
    fn new(child: tokio::process::Child) -> Self {
        Self {
            #[cfg(unix)]
            process_group: child.id().map(|id| Pid::from_raw(id as i32)),
            child,
        }
    }

    /// Await termination before the executor's device assignment can be returned.
    async fn stop(&mut self) -> Result<(), FlameError> {
        #[cfg(unix)]
        let process_group = self.process_group;
        #[cfg(unix)]
        if let Some(group) = process_group {
            signal_group(group, Signal::SIGTERM)?;
        }
        #[cfg(not(unix))]
        self.child.start_kill()?;

        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(result) => {
                result?;
                // The leader may exit while worker processes still hold GPU memory.
                #[cfg(unix)]
                if let Some(group) = process_group {
                    signal_group(group, Signal::SIGKILL)?;
                }
            }
            Err(_) => {
                #[cfg(unix)]
                if let Some(group) = process_group {
                    signal_group(group, Signal::SIGKILL)?;
                }
                #[cfg(not(unix))]
                self.child.start_kill()?;
                tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                    .await
                    .map_err(|_| {
                        FlameError::Internal("Host instance did not exit after SIGKILL".to_string())
                    })??;
            }
        }
        #[cfg(unix)]
        if let Some(group) = process_group {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match killpg(group, None) {
                        Err(nix::errno::Errno::ESRCH) => return Ok(()),
                        Ok(()) => tokio::time::sleep(Duration::from_millis(20)).await,
                        Err(error) => {
                            return Err(FlameError::Internal(format!(
                                "failed to check Host process group <{group}>: {error}"
                            )))
                        }
                    }
                }
            })
            .await
            .map_err(|_| {
                FlameError::Internal(format!(
                "Host process group <{group}> still exists after SIGKILL; keeping GPU assignment"
            ))
            })??;
            self.process_group = None;
        }
        Ok(())
    }

    /// Synchronous drop fallback; ordinary teardown calls `stop` and reaps the child.
    fn kill_process(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.process_group {
            let _ = signal_group(group, Signal::SIGKILL);
        }
        let _ = self.child.start_kill();
    }
}

#[cfg(unix)]
fn signal_group(group: Pid, signal: Signal) -> Result<(), FlameError> {
    match killpg(group, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(FlameError::Internal(format!(
            "failed to signal Host process group <{group}>: {error}"
        ))),
    }
}

pub struct HostShim {
    executor: Executor,
    app: ApplicationContext,
    instance: Option<HostInstance>,
    instance_client: Option<GrpcShim>,
    work_dir: Option<ExecutorWorkDir>,
}

const RUST_LOG: &str = "RUST_LOG";
const DEFAULT_SVC_LOG_LEVEL: &str = "info";

impl HostShim {
    pub fn new_ptr(executor: &Executor, app: &ApplicationContext) -> ShimPtr {
        trace_fn!("HostShim::new_ptr");
        Arc::new(Mutex::new(Self {
            executor: executor.clone(),
            app: app.clone(),
            instance: None,
            instance_client: None,
            work_dir: None,
        }))
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
            // Pass session manager endpoint for recursive app calls
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

        // Assignments belong to the executor and override application,
        // installer, and inherited settings for the lifetime of its service.
        Self::apply_assignments(&mut envs, &executor.assignments)?;

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

    fn apply_assignments(
        environment: &mut HashMap<String, String>,
        assignments: &crate::devices::DeviceAssignments,
    ) -> Result<(), FlameError> {
        let mut assigned_envs = HashMap::new();
        for assignment in assignments.values() {
            for (key, value) in &assignment.envs {
                if let Some(previous) = assigned_envs.insert(key.clone(), value.clone()) {
                    if previous != *value {
                        return Err(FlameError::InvalidConfig(format!(
                            "device assignments conflict on environment variable '{key}'"
                        )));
                    }
                }
            }
        }
        environment.extend(assigned_envs);
        Ok(())
    }

    fn is_path_env(key: &str) -> bool {
        matches!(key, "PATH" | "PYTHONPATH" | "LD_LIBRARY_PATH")
    }

    fn base_runtime_environment(
        app: &ApplicationContext,
        flame_home: &Path,
    ) -> HashMap<String, String> {
        if app.url.is_some()
            || !app
                .installer
                .as_deref()
                .is_some_and(|installer| installer.eq_ignore_ascii_case("python"))
        {
            return HashMap::new();
        }

        let runtime = get_python_runtime(
            flame_home,
            app.environments
                .get(FLAME_PYTHON_VERSION_ENV)
                .map(String::as_str),
        );
        let Some(site_packages) = runtime.site_packages else {
            return HashMap::new();
        };

        let mut environment = HashMap::from([
            (FLAME_PYTHON_VERSION_ENV.to_string(), runtime.version),
            (
                "PYTHONPATH".to_string(),
                site_packages.to_string_lossy().to_string(),
            ),
        ]);
        let native_paths = Self::find_native_lib_paths(&site_packages);
        if !native_paths.is_empty() {
            environment.insert("LD_LIBRARY_PATH".to_string(), native_paths.join(":"));
        }
        environment
    }

    fn find_native_lib_paths(root: &Path) -> Vec<String> {
        fn scan(path: &Path, paths: &mut HashSet<String>, depth: usize) {
            if depth > 4 {
                return;
            }
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan(&path, paths, depth + 1);
                    } else if path.extension().is_some_and(|extension| extension == "so") {
                        if let Some(parent) = path.parent() {
                            paths.insert(parent.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        let mut paths = HashSet::new();
        scan(root, &mut paths, 0);
        paths.into_iter().collect()
    }
}

impl Drop for HostShim {
    fn drop(&mut self) {
        if let Some(client) = self.instance_client.as_mut() {
            client.close();
        }
        if let Some(instance) = self.instance.as_mut() {
            instance.kill_process();
        }
    }
}

#[async_trait]
impl Shim for HostShim {
    async fn create_instance(
        &mut self,
        app_manager: Arc<ApplicationManager>,
    ) -> Result<(), FlameError> {
        if self.instance.is_some() {
            return Ok(());
        }
        let mut installation = app_manager.install(&self.app).await?;
        let flame_home = env::var("FLAME_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/opt/flame"));
        installation
            .env_vars
            .extend(Self::base_runtime_environment(&self.app, &flame_home));
        let work_dir = ExecutorWorkDir::new(&self.app, &self.executor.id)?;
        let mut instance_client = GrpcShim::new(&work_dir)?;
        let mut instance =
            Self::launch_instance(&self.app, &self.executor, &work_dir, &installation.env_vars)?;
        if let Err(error) = instance_client.connect().await {
            // Retain ownership if cleanup fails so the GPU assignment stays leased.
            self.work_dir = Some(work_dir);
            self.instance = Some(instance);
            self.destroy_instance().await.map_err(|cleanup| {
                FlameError::Internal(format!("{error}; Host instance cleanup failed: {cleanup}"))
            })?;
            return Err(error);
        }
        self.work_dir = Some(work_dir);
        self.instance_client = Some(instance_client);
        self.instance = Some(instance);
        Ok(())
    }

    async fn on_session_enter(
        &mut self,
        ctx: &SessionContext,
    ) -> Result<super::SessionEnterResponse, FlameError> {
        trace_fn!("HostShim::on_session_enter");

        self.instance_client
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("Host instance is not created".to_string()))?
            .on_session_enter(ctx)
            .await
    }

    async fn on_task_invoke(
        &mut self,
        ctx: &TaskContext,
    ) -> Result<super::TaskInvokeResponse, FlameError> {
        trace_fn!("HostShim::on_task_invoke");

        self.instance_client
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("Host instance is not created".to_string()))?
            .on_task_invoke(ctx)
            .await
    }

    async fn on_session_leave(&mut self) -> Result<(), FlameError> {
        trace_fn!("HostShim::on_session_leave");

        self.instance_client
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("Host instance is not created".to_string()))?
            .on_session_leave()
            .await
    }

    async fn destroy_instance(&mut self) -> Result<(), FlameError> {
        if let Some(mut client) = self.instance_client.take() {
            client.close();
        }
        if let Some(instance) = self.instance.as_mut() {
            instance.stop().await?;
        }
        self.instance = None;
        self.work_dir = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::DeviceManager;
    use common::apis::{ExecutorState, ResourceRequirement, Shim as ShimType};

    fn test_executor() -> Executor {
        Executor {
            id: "executor-1".to_string(),
            application: "test-app".to_string(),
            resreq: ResourceRequirement::default(),
            node: "node-1".to_string(),
            shim: ShimType::Host,
            session: None,
            task: None,
            context: None,
            app_mgr: None,
            shim_instance: None,
            assignments: Default::default(),
            state: ExecutorState::Idle,
        }
    }

    #[test]
    fn gpu_assignment_overrides_application_and_installer_visibility() {
        let gpu_workdir = tempfile::tempdir().unwrap();
        let allocator = crate::devices::gpu::GpuManager::new()
            .with_workdir(gpu_workdir.path())
            .with_inventory(vec!["GPU-first".to_string(), "GPU-second".to_string()])
            .unwrap();
        let assignment = allocator
            .allocate(&crate::devices::test_executor(
                (common::apis::ResourceRequirement {
                    gpu: 2,
                    ..Default::default()
                })
                .clone(),
            ))
            .unwrap();
        // The launcher applies this after merging both application and installer envs.
        let mut environment = HashMap::from([
            ("CUDA_VISIBLE_DEVICES".to_string(), "0,1,2,3".to_string()),
            ("OTHER_VARIABLE".to_string(), "preserved".to_string()),
        ]);
        let assignments =
            crate::devices::DeviceAssignments::from([("gpu".to_string(), assignment)]);
        HostShim::apply_assignments(&mut environment, &assignments).unwrap();
        assert_eq!(environment["CUDA_VISIBLE_DEVICES"], "GPU-first,GPU-second");
        assert_eq!(environment["OTHER_VARIABLE"], "preserved");
        assert_eq!(
            HostShim::expand_env_vars("$CUDA_VISIBLE_DEVICES", Some(&environment)),
            "GPU-first,GPU-second"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launched_process_receives_assignment_instead_of_app_or_install_override() {
        let temp = tempfile::tempdir().unwrap();
        let gpu_workdir = tempfile::tempdir().unwrap();
        let allocator = crate::devices::gpu::GpuManager::new()
            .with_workdir(gpu_workdir.path())
            .with_inventory(vec!["GPU-assigned".to_string()])
            .unwrap();
        let mut executor = test_executor();
        executor.resreq.gpu = 1;
        executor.assignments.insert(
            "gpu".to_string(),
            allocator
                .allocate(&crate::devices::test_executor(
                    (common::apis::ResourceRequirement {
                        gpu: 1,
                        ..Default::default()
                    })
                    .clone(),
                ))
                .unwrap(),
        );
        let app = ApplicationContext {
            name: "test-app".to_string(),
            shim: ShimType::Host,
            image: None,
            command: Some("sh".to_string()),
            arguments: vec![
                "-c".to_string(),
                "printf '%s' \"$CUDA_VISIBLE_DEVICES\"".to_string(),
            ],
            working_directory: None,
            environments: HashMap::from([(
                "CUDA_VISIBLE_DEVICES".to_string(),
                "app-override".to_string(),
            )]),
            url: None,
            installer: None,
        };
        let work_dir = ExecutorWorkDir {
            top_dir: temp.path().to_path_buf(),
            app_dir: temp.path().join("app"),
            socket: temp.path().join("instance.sock"),
            auto_dir: false,
        };
        let install_environment = HashMap::from([(
            "CUDA_VISIBLE_DEVICES".to_string(),
            "install-override".to_string(),
        )]);
        let mut instance =
            HostShim::launch_instance(&app, &executor, &work_dir, &install_environment).unwrap();
        instance.child.wait().await.unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("executor-1.out")).unwrap(),
            "GPU-assigned"
        );
        instance.stop().await.unwrap();
    }

    #[test]
    fn cpu_executor_hides_gpus_even_when_application_sets_visibility() {
        let mut environment =
            HashMap::from([("CUDA_VISIBLE_DEVICES".to_string(), "0".to_string())]);
        let gpu_workdir = tempfile::tempdir().unwrap();
        let gpu = crate::devices::gpu::GpuManager::new()
            .with_workdir(gpu_workdir.path())
            .with_inventory(Vec::new())
            .unwrap();
        let assignments = crate::devices::DeviceAssignments::from([(
            "gpu".to_string(),
            gpu.allocate(&crate::devices::test_executor(
                (common::apis::ResourceRequirement {
                    gpu: 0,
                    ..Default::default()
                })
                .clone(),
            ))
            .unwrap(),
        )]);
        HostShim::apply_assignments(&mut environment, &assignments).unwrap();
        assert_eq!(environment["CUDA_VISIBLE_DEVICES"], "");
    }

    #[test]
    fn applies_environment_from_all_device_assignments() {
        let gpu_workdir = tempfile::tempdir().unwrap();
        let gpu = crate::devices::gpu::GpuManager::new()
            .with_workdir(gpu_workdir.path())
            .with_inventory(Vec::new())
            .unwrap();
        let mut first = gpu
            .allocate(&crate::devices::test_executor(
                (common::apis::ResourceRequirement {
                    gpu: 0,
                    ..Default::default()
                })
                .clone(),
            ))
            .unwrap();
        first.envs = HashMap::from([("FIRST_DEVICE".to_string(), "a".to_string())]);
        let mut second = gpu
            .allocate(&crate::devices::test_executor(
                (common::apis::ResourceRequirement {
                    gpu: 0,
                    ..Default::default()
                })
                .clone(),
            ))
            .unwrap();
        second.envs = HashMap::from([("SECOND_DEVICE".to_string(), "b".to_string())]);
        let assignments = crate::devices::DeviceAssignments::from([
            ("first".to_string(), first),
            ("second".to_string(), second),
        ]);
        let mut environment =
            HashMap::from([("FIRST_DEVICE".to_string(), "app-override".to_string())]);
        HostShim::apply_assignments(&mut environment, &assignments).unwrap();
        assert_eq!(environment["FIRST_DEVICE"], "a");
        assert_eq!(environment["SECOND_DEVICE"], "b");
    }

    #[test]
    fn conflicting_assignment_environments_fail_without_partial_updates() {
        let gpu_workdir = tempfile::tempdir().unwrap();
        let gpu = crate::devices::gpu::GpuManager::new()
            .with_workdir(gpu_workdir.path())
            .with_inventory(Vec::new())
            .unwrap();
        let mut first = gpu
            .allocate(&crate::devices::test_executor(
                (common::apis::ResourceRequirement {
                    gpu: 0,
                    ..Default::default()
                })
                .clone(),
            ))
            .unwrap();
        first.envs = HashMap::from([("DEVICE".to_string(), "a".to_string())]);
        let mut second = gpu
            .allocate(&crate::devices::test_executor(
                (common::apis::ResourceRequirement {
                    gpu: 0,
                    ..Default::default()
                })
                .clone(),
            ))
            .unwrap();
        second.envs = HashMap::from([("DEVICE".to_string(), "b".to_string())]);
        let assignments = crate::devices::DeviceAssignments::from([
            ("first".to_string(), first),
            ("second".to_string(), second),
        ]);
        let mut environment = HashMap::from([("DEVICE".to_string(), "original".to_string())]);
        assert!(HostShim::apply_assignments(&mut environment, &assignments).is_err());
        assert_eq!(environment["DEVICE"], "original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_reaps_running_host_process_before_returning() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .current_dir(std::env::temp_dir())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut instance = HostInstance::new(child);
        instance.stop().await.unwrap();
        assert!(instance.child.try_wait().unwrap().is_some());
        assert!(instance.child.id().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_kills_and_reaps_process_that_ignores_sigterm() {
        use tokio::io::AsyncReadExt;
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "trap '' TERM; printf ready; exec sleep 60"])
            .stdout(Stdio::piped())
            .current_dir(std::env::temp_dir())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut ready = [0_u8; 5];
        tokio::time::timeout(
            Duration::from_secs(5),
            child.stdout.as_mut().unwrap().read_exact(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&ready, b"ready");
        let mut instance = HostInstance::new(child);
        tokio::time::timeout(Duration::from_secs(12), instance.stop())
            .await
            .unwrap()
            .unwrap();
        assert!(instance.child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_waits_for_worker_group_after_leader_exits() {
        use tokio::io::AsyncReadExt;
        let leader = tokio::process::Command::new("sleep")
            .arg("60")
            .current_dir(std::env::temp_dir())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(leader.id().unwrap() as i32);
        let mut worker = tokio::process::Command::new("sh")
            .args(["-c", "trap '' TERM; printf ready; exec sleep 60"])
            .stdout(Stdio::piped())
            .current_dir(std::env::temp_dir())
            .process_group(group.as_raw())
            .spawn()
            .unwrap();
        let mut ready = [0_u8; 5];
        tokio::time::timeout(
            Duration::from_secs(5),
            worker.stdout.as_mut().unwrap().read_exact(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        // Reap the worker as a real init/subreaper would. A zombie still belongs
        // to the group, so teardown conservatively retains capacity until reaped.
        let worker_wait = tokio::spawn(async move { worker.wait().await.unwrap() });
        let mut instance = HostInstance::new(leader);
        instance.child.start_kill().unwrap();
        instance.child.wait().await.unwrap();
        assert!(instance.child.id().is_none());
        assert!(killpg(group, None).is_ok());
        instance.stop().await.unwrap();
        worker_wait.await.unwrap();
        assert_eq!(killpg(group, None), Err(nix::errno::Errno::ESRCH));
        assert!(instance.process_group.is_none());
        instance.stop().await.unwrap();
    }

    #[test]
    fn expand_command_args_from_launch_env() {
        let envs = HashMap::from([("FLAME_PYTHON_VERSION".to_string(), "3.12".to_string())]);

        assert_eq!(
            HostShim::expand_env_vars("python${FLAME_PYTHON_VERSION}", Some(&envs)),
            "python3.12"
        );
    }

    #[test]
    fn url_less_python_host_uses_installed_base_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let site_packages = temp.path().join("lib/python3.12/site-packages");
        fs::create_dir_all(&site_packages).unwrap();
        let app = ApplicationContext {
            name: "flmrun".to_string(),
            shim: ShimType::Host,
            image: None,
            command: None,
            arguments: vec![],
            working_directory: None,
            environments: HashMap::new(),
            url: None,
            installer: Some("python".to_string()),
        };

        let environment = HostShim::base_runtime_environment(&app, temp.path());

        assert_eq!(
            environment
                .get(FLAME_PYTHON_VERSION_ENV)
                .map(String::as_str),
            Some("3.12")
        );
        assert_eq!(
            environment.get("PYTHONPATH").map(String::as_str),
            Some(site_packages.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn create_instance_installs_before_runtime_creation() {
        let app = ApplicationContext {
            name: "test-app".to_string(),
            shim: ShimType::Host,
            image: None,
            command: None,
            arguments: vec![],
            working_directory: None,
            environments: HashMap::new(),
            url: Some("file:///unused-package.tar.gz".to_string()),
            installer: Some("unsupported".to_string()),
        };
        let mut shim = HostShim {
            executor: test_executor(),
            app,
            instance: None,
            instance_client: None,
            work_dir: None,
        };

        let error = shim
            .create_instance(Arc::new(ApplicationManager::new().unwrap()))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Unknown installer type"));
        assert!(shim.instance.is_none());
        assert!(shim.instance_client.is_none());
        assert!(shim.work_dir.is_none());
    }
}

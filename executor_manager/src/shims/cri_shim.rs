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

use std::collections::{BTreeMap, HashMap};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cri_rs::{
    ContainerRuntimeState, ContainerSecurityContext, ContainerSpec, Mount, ResourceLimits,
    WorkloadFilter, WorkloadHandle, WorkloadManager, WorkloadMetadata, WorkloadSpec,
};
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

use crate::appmgr::{ApplicationInstallation, ApplicationManager, InstallationMountKind};
use crate::executor::Executor;
use crate::shims::grpc_shim::GrpcShim;
use crate::shims::{Shim, ShimPtr};
use common::apis::{ApplicationContext, SessionContext, TaskContext};
use common::ctx::{FlameCache, FlameClusterContext};
use common::{FlameError, FLAME_CACHE_ENDPOINT, FLAME_CA_FILE, FLAME_INSTANCE_ENDPOINT, FLAME_LOG};

const WORK_ROOT: &str = "/var/lib/flame/executors";
const LOG_ROOT: &str = "/var/log/flame/executors";
const INSTANCE_SOCKET: &str = "instance.sock";
const RUST_LOG: &str = "RUST_LOG";
const DEFAULT_LOG_LEVEL: &str = "info";

pub struct CriShim {
    executor: Executor,
    app: Option<ApplicationContext>,
    manager: Option<WorkloadManager>,
    handle: Option<WorkloadHandle>,
    instance_client: Option<GrpcShim>,
    work_dir: Option<PathBuf>,
}

impl CriShim {
    pub fn new_ptr(executor: &Executor, app: Option<&ApplicationContext>) -> ShimPtr {
        Arc::new(Mutex::new(Self {
            executor: executor.clone(),
            app: app.cloned(),
            manager: None,
            handle: None,
            instance_client: None,
            work_dir: None,
        }))
    }

    async fn create(&mut self, app_manager: &ApplicationManager) -> Result<(), FlameError> {
        if self.handle.is_some() {
            return Ok(());
        }
        let executor = &self.executor;
        let app = self.app.as_ref().ok_or_else(|| {
            FlameError::InvalidState("cleanup-only CRI shim cannot create an instance".to_string())
        })?;
        validate_inputs(executor, app)?;
        let context = executor.context.as_ref().ok_or_else(|| {
            FlameError::InvalidState("CRI executor is missing cluster context".to_string())
        })?;
        let installation = app_manager.install(app).await?;

        let (work_dir, uid, gid) = prepare_executor_directory(&executor.id)?;
        let mut directory_guard = DirectoryGuard::new(work_dir.clone());
        let log_dir = Path::new(LOG_ROOT).join(&executor.id);
        fs::create_dir_all(&log_dir).map_err(|error| {
            FlameError::Storage(format!(
                "failed to create CRI log directory <{}>: {error}",
                log_dir.display()
            ))
        })?;
        let socket = work_dir.join(INSTANCE_SOCKET);
        remove_stale_socket(&socket)?;

        let log_level = std::env::var(RUST_LOG).unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());
        let mut env = build_environment(
            &app.environments,
            context.cache.as_ref().map(|cache| cache.endpoint.as_str()),
            &socket,
            &log_level,
        );
        let installation_mounts = container_mounts(&work_dir, &installation)?;
        let installed_environment = rebase_install_environment(&installation, &installation_mounts);
        merge_install_environment(&mut env, &installed_environment);
        if let Some(ca_source) = cache_ca_file(context.cache.as_ref()) {
            let ca_target = work_dir.join("ca.crt");
            fs::copy(ca_source, &ca_target).map_err(|error| {
                FlameError::Storage(format!(
                    "failed to stage CRI object-cache CA file <{ca_source}>: {error}"
                ))
            })?;
            #[cfg(unix)]
            fs::set_permissions(&ca_target, fs::Permissions::from_mode(0o400))?;
            env.insert(
                FLAME_CA_FILE.to_string(),
                ca_target.to_string_lossy().to_string(),
            );
        }

        let image = app.image.clone().ok_or_else(|| {
            FlameError::InvalidConfig(format!("CRI application <{}> requires an image", app.name))
        })?;
        let command = app
            .command
            .as_deref()
            .map(|command| expand_container_environment(command, &env))
            .transpose()?;
        let args = app
            .arguments
            .iter()
            .map(|argument| expand_container_environment(argument, &env))
            .collect::<Result<Vec<_>, _>>()?;
        let workload_uid = Uuid::new_v4().to_string();
        let spec = WorkloadSpec {
            metadata: WorkloadMetadata {
                name: workload_name(&app.name, &executor.id),
                namespace: "flame".to_string(),
                uid: workload_uid,
                executor_id: executor.id.clone(),
                application: app.name.clone(),
            },
            containers: vec![ContainerSpec {
                name: "application".to_string(),
                image,
                command,
                args,
                env,
                working_directory: app.working_directory.clone().unwrap_or_default(),
                mounts: installation_mounts,
                resources: ResourceLimits {
                    cpu: executor.resreq.cpu,
                    memory: executor.resreq.memory,
                },
                security_context: ContainerSecurityContext {
                    run_as_user: Some(uid),
                    run_as_group: Some(gid),
                    supplemental_groups: vec![gid],
                },
            }],
            log_directory: log_dir.to_string_lossy().to_string(),
        };

        let mut manager = WorkloadManager::connect().await?;
        let handle = match manager.create(&spec).await {
            Ok(handle) => handle,
            Err(error) => return Err(error),
        };
        let mut instance_client = match GrpcShim::new_at(&socket) {
            Ok(client) => client,
            Err(primary) => {
                let cleanup = manager.delete(&handle).await.err();
                return Err(with_cleanup(primary, cleanup));
            }
        };

        let readiness = tokio::time::timeout(Duration::from_secs(30), async {
            let monitor = async {
                loop {
                    let status = manager.status(&handle).await?;
                    if let Some(container) = status.containers.first() {
                        if matches!(
                            container.state,
                            ContainerRuntimeState::Exited | ContainerRuntimeState::Unknown
                        ) {
                            return Err(FlameError::InvalidState(format!(
                                "CRI application container exited before readiness: exit_code={}, reason={}, message={}",
                                container.exit_code, container.reason, container.message
                            )));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            tokio::select! {
                result = instance_client.connect() => result,
                result = monitor => result,
            }
        })
        .await
        .map_err(|_| FlameError::Network("CRI instance startup timed out after 30s".to_string()))
        .and_then(|result| result);

        if let Err(primary) = readiness {
            let cleanup = manager.delete(&handle).await.err();
            return Err(with_cleanup(primary, cleanup));
        }

        directory_guard.disarm();
        self.manager = Some(manager);
        self.handle = Some(handle);
        self.instance_client = Some(instance_client);
        self.work_dir = Some(work_dir);
        Ok(())
    }
}

#[async_trait]
impl Shim for CriShim {
    async fn create_service(
        &mut self,
        app_manager: Arc<ApplicationManager>,
    ) -> Result<(), FlameError> {
        self.create(&app_manager).await
    }

    async fn on_session_enter(
        &mut self,
        context: &SessionContext,
    ) -> Result<super::SessionEnterResponse, FlameError> {
        self.instance_client
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("CRI instance is not created".to_string()))?
            .on_session_enter(context)
            .await
    }

    async fn on_task_invoke(
        &mut self,
        context: &TaskContext,
    ) -> Result<super::TaskInvokeResponse, FlameError> {
        let handle = self.handle.as_ref().ok_or_else(|| {
            FlameError::InvalidState("CRI workload has already been shut down".to_string())
        })?;
        let status = self
            .manager
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("CRI instance is not created".to_string()))?
            .status(handle)
            .await?;
        if !status.healthy() {
            return Err(FlameError::InvalidState(format!(
                "CRI workload <{}> is not healthy",
                handle.sandbox_id()
            )));
        }
        self.instance_client
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("CRI instance is not created".to_string()))?
            .on_task_invoke(context)
            .await
    }

    async fn on_session_leave(&mut self) -> Result<(), FlameError> {
        self.instance_client
            .as_mut()
            .ok_or_else(|| FlameError::InvalidState("CRI instance is not created".to_string()))?
            .on_session_leave()
            .await
    }

    async fn destroy_instance(&mut self) -> Result<(), FlameError> {
        if let Some(client) = self.instance_client.as_mut() {
            client.close();
        }
        let result =
            if let (Some(manager), Some(handle)) = (self.manager.as_mut(), self.handle.as_ref()) {
                manager.delete(handle).await
            } else {
                self.destroy_persisted_instance().await
            };
        if result.is_ok() {
            self.handle = None;
            self.manager = None;
            self.instance_client = None;
            let work_dir = self
                .work_dir
                .take()
                .unwrap_or_else(|| Path::new(WORK_ROOT).join(&self.executor.id));
            cleanup_directory(&work_dir);
        }
        result
    }
}

impl CriShim {
    async fn destroy_persisted_instance(&mut self) -> Result<(), FlameError> {
        validate_path_component(&self.executor.id)?;
        let filter = WorkloadFilter::new(self.executor.id.clone())?;
        let mut manager = WorkloadManager::connect().await?;
        let handles = manager.list_workload(&filter).await?;
        let mut errors = Vec::new();
        for handle in handles {
            if let Err(error) = manager.delete(&handle).await {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(FlameError::Internal(format!(
                "failed to destroy persisted CRI executor <{}>: {}",
                self.executor.id,
                errors.join("; ")
            )))
        }
    }
}

fn with_cleanup(primary: FlameError, cleanup: Option<FlameError>) -> FlameError {
    match cleanup {
        Some(cleanup) => FlameError::Internal(format!("{primary}; CRI rollback failed: {cleanup}")),
        None => primary,
    }
}

struct DirectoryGuard {
    path: PathBuf,
    armed: bool,
}

impl DirectoryGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        if self.armed {
            cleanup_directory(&self.path);
        }
    }
}

fn validate_inputs(executor: &Executor, app: &ApplicationContext) -> Result<(), FlameError> {
    if app.image.as_deref().is_none_or(str::is_empty) {
        return Err(FlameError::InvalidConfig(format!(
            "CRI application <{}> requires an image",
            app.name
        )));
    }
    if executor.resreq.gpu != 0 {
        return Err(FlameError::InvalidConfig(
            "CRI applications do not support GPU resources in v1".to_string(),
        ));
    }
    if let Some(cache) = executor
        .context
        .as_ref()
        .and_then(|context| context.cache.as_ref())
    {
        reject_loopback_endpoint(&cache.endpoint)?;
    }
    validate_path_component(&executor.id)
}

fn merge_install_environment(
    environment: &mut BTreeMap<String, String>,
    installed: &HashMap<String, String>,
) {
    for (key, value) in installed {
        if matches!(key.as_str(), "PATH" | "PYTHONPATH" | "LD_LIBRARY_PATH") {
            environment
                .entry(key.clone())
                .and_modify(|current| *current = format!("{value}:{current}"))
                .or_insert_with(|| value.clone());
        } else {
            environment
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
}

fn installation_target(work_dir: &Path, kind: InstallationMountKind) -> PathBuf {
    match kind {
        InstallationMountKind::Release => work_dir.join("installation"),
        InstallationMountKind::PythonRuntime => work_dir.join("runtime/site-packages"),
        InstallationMountKind::UvCache => work_dir.join("cache/uv"),
        InstallationMountKind::PipCache => work_dir.join("cache/pip"),
    }
}

fn container_mounts(
    work_dir: &Path,
    installation: &ApplicationInstallation,
) -> Result<Vec<Mount>, FlameError> {
    let mut mounts = vec![Mount {
        host_path: work_dir.to_string_lossy().to_string(),
        container_path: work_dir.to_string_lossy().to_string(),
        readonly: false,
    }];
    for installation_mount in &installation.mounts {
        let target = installation_target(work_dir, installation_mount.kind);
        fs::create_dir_all(&target).map_err(|error| {
            FlameError::Storage(format!(
                "failed to create CRI installation mount target <{}>: {error}",
                target.display()
            ))
        })?;
        mounts.push(Mount {
            host_path: installation_mount.host_path.to_string_lossy().to_string(),
            container_path: target.to_string_lossy().to_string(),
            readonly: installation_mount.readonly,
        });
    }
    Ok(mounts)
}

fn rebase_install_environment(
    installation: &ApplicationInstallation,
    mounts: &[Mount],
) -> HashMap<String, String> {
    fn rebase_path(value: &str, mounts: &[Mount]) -> Option<String> {
        let value = Path::new(value);
        mounts.iter().skip(1).find_map(|mount| {
            value
                .strip_prefix(Path::new(&mount.host_path))
                .ok()
                .map(|suffix| {
                    let target = Path::new(&mount.container_path);
                    if suffix.as_os_str().is_empty() {
                        target.to_string_lossy().to_string()
                    } else {
                        target.join(suffix).to_string_lossy().to_string()
                    }
                })
        })
    }

    installation
        .env_vars
        .iter()
        .filter_map(|(key, value)| {
            let value = match key.as_str() {
                "PATH" | "PYTHONPATH" | "LD_LIBRARY_PATH" => {
                    let paths = value
                        .split(':')
                        .filter_map(|path| rebase_path(path, mounts))
                        .collect::<Vec<_>>();
                    (!paths.is_empty()).then(|| paths.join(":"))
                }
                "FLAME_APP_DIR" | "UV_CACHE_DIR" | "PIP_CACHE_DIR" => rebase_path(value, mounts),
                _ => Some(value.clone()),
            }?;
            Some((key.clone(), value))
        })
        .collect()
}

fn expand_container_environment(
    value: &str,
    environment: &BTreeMap<String, String>,
) -> Result<String, FlameError> {
    shellexpand::env_with_context(value, |key| {
        environment
            .get(key)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("environment variable <{key}> is not set in the CRI container"))
    })
    .map(|expanded| expanded.into_owned())
    .map_err(|error| FlameError::InvalidConfig(error.cause))
}

fn reject_loopback_endpoint(endpoint: &str) -> Result<(), FlameError> {
    let url = Url::parse(endpoint).map_err(|error| {
        FlameError::InvalidConfig(format!("invalid cache endpoint <{endpoint}>: {error}"))
    })?;
    let host = url.host_str().ok_or_else(|| {
        FlameError::InvalidConfig(format!("cache endpoint <{endpoint}> has no host"))
    })?;
    let normalized_host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = normalized_host.eq_ignore_ascii_case("localhost")
        || normalized_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if loopback {
        Err(FlameError::InvalidConfig(format!(
            "cache endpoint <{endpoint}> is loopback and unreachable from a CRI sandbox"
        )))
    } else {
        Ok(())
    }
}

fn build_environment(
    application: &HashMap<String, String>,
    cache_endpoint: Option<&str>,
    socket: &Path,
    log_level: &str,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from_iter(application.clone());
    env.insert(RUST_LOG.to_string(), log_level.to_string());
    env.insert(FLAME_LOG.to_string(), log_level.to_string());
    env.insert(
        FLAME_INSTANCE_ENDPOINT.to_string(),
        socket.to_string_lossy().to_string(),
    );
    if let Some(endpoint) = cache_endpoint {
        env.insert(FLAME_CACHE_ENDPOINT.to_string(), endpoint.to_string());
    }
    env
}

fn cache_ca_file(cache: Option<&FlameCache>) -> Option<&str> {
    cache
        .and_then(|cache| cache.tls.as_ref())
        .and_then(|tls| tls.ca_file.as_deref())
}

fn workload_name(application: &str, executor_id: &str) -> String {
    format!("{application}-{executor_id}")
}

fn validate_path_component(value: &str) -> Result<(), FlameError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        Err(FlameError::InvalidConfig(format!(
            "invalid executor ID for CRI work directory: <{value}>"
        )))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn prepare_executor_directory(executor_id: &str) -> Result<(PathBuf, i64, i64), FlameError> {
    let root = Path::new(WORK_ROOT);
    fs::create_dir_all(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    let work_dir = root.join(executor_id);
    fs::create_dir(&work_dir).map_err(|error| {
        FlameError::Storage(format!(
            "failed to create private CRI work directory <{}>: {error}",
            work_dir.display()
        ))
    })?;
    fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(&work_dir)?;
    Ok((
        work_dir,
        i64::from(metadata.uid()),
        i64::from(metadata.gid()),
    ))
}

#[cfg(not(unix))]
fn prepare_executor_directory(_executor_id: &str) -> Result<(PathBuf, i64, i64), FlameError> {
    Err(FlameError::InvalidConfig(
        "CRI shim requires a Unix platform".to_string(),
    ))
}

fn remove_stale_socket(socket: &Path) -> Result<(), FlameError> {
    match fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.is_dir() => Err(FlameError::InvalidState(format!(
            "stale Instance path <{}> is a directory",
            socket.display()
        ))),
        Ok(_) => fs::remove_file(socket).map_err(FlameError::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FlameError::from(error)),
    }
}

fn cleanup_directory(path: &Path) {
    if let Err(error) = fs::remove_dir_all(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove CRI executor directory <{}>: {}",
                path.display(),
                error
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::{ExecutorState, ResourceRequirement, Shim as ShimType};
    use common::ctx::FlameTls;
    use common::FLAME_ENDPOINT;
    use tempfile::tempdir;

    fn test_executor() -> Executor {
        Executor {
            id: "executor-1".to_string(),
            application: "test-app".to_string(),
            resreq: ResourceRequirement::default(),
            node: "node-1".to_string(),
            shim: ShimType::Cri,
            session: None,
            task: None,
            context: None,
            shim_instance: None,
            state: ExecutorState::Idle,
        }
    }

    fn test_app() -> ApplicationContext {
        ApplicationContext {
            name: "test-app".to_string(),
            shim: ShimType::Cri,
            image: Some("example/image:latest".to_string()),
            command: None,
            arguments: vec![],
            working_directory: None,
            environments: HashMap::new(),
            url: None,
            installer: None,
        }
    }

    #[tokio::test]
    async fn cleanup_only_shim_cannot_create_service() {
        let shim = CriShim::new_ptr(&test_executor(), None);
        let error = shim
            .lock()
            .await
            .create_service(Arc::new(ApplicationManager::new().unwrap()))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("cleanup-only CRI shim"));
    }

    #[test]
    fn cri_mounts_installation_at_executor_local_paths() {
        let temp = tempdir().unwrap();
        let work_dir = temp.path().join("executor");
        let release = temp.path().join("releases/hash");
        let installation = ApplicationInstallation {
            env_vars: HashMap::new(),
            mounts: vec![
                crate::appmgr::InstallationMount {
                    host_path: release.clone(),
                    kind: InstallationMountKind::Release,
                    readonly: true,
                },
                crate::appmgr::InstallationMount {
                    host_path: temp.path().join("lib/python3.12/site-packages"),
                    kind: InstallationMountKind::PythonRuntime,
                    readonly: true,
                },
                crate::appmgr::InstallationMount {
                    host_path: temp.path().join("cache/uv"),
                    kind: InstallationMountKind::UvCache,
                    readonly: false,
                },
                crate::appmgr::InstallationMount {
                    host_path: temp.path().join("cache/pip"),
                    kind: InstallationMountKind::PipCache,
                    readonly: false,
                },
            ],
        };

        let mounts = container_mounts(&work_dir, &installation).unwrap();

        assert_eq!(mounts.len(), 5);
        assert!(!mounts[0].readonly);
        assert!(mounts[1].readonly);
        assert_eq!(mounts[1].host_path, release.to_string_lossy());
        assert_eq!(
            mounts[1].container_path,
            work_dir.join("installation").to_string_lossy()
        );
        assert!(mounts[2].readonly);
        assert!(!mounts[3].readonly);
        assert!(!mounts[4].readonly);
        assert_eq!(
            mounts[3].container_path,
            work_dir.join("cache/uv").to_string_lossy()
        );
        assert_eq!(
            mounts[4].container_path,
            work_dir.join("cache/pip").to_string_lossy()
        );
    }

    #[test]
    fn cri_rebases_installed_environment_and_uses_pod_caches() {
        let temp = tempdir().unwrap();
        let work_dir = temp.path().join("executor");
        let release = temp.path().join("releases/hash");
        let runtime = temp.path().join("lib/python3.12/site-packages");
        let uv_cache = temp.path().join("cache/uv");
        let pip_cache = temp.path().join("cache/pip");
        let mut environment = BTreeMap::from([
            ("PYTHONPATH".to_string(), "/application/path".to_string()),
            ("PATH".to_string(), "/usr/local/bin".to_string()),
        ]);
        let installation = ApplicationInstallation {
            env_vars: HashMap::from([
                (
                    "PYTHONPATH".to_string(),
                    format!(
                        "{}/deps:{}/src:{}:/application/path",
                        release.display(),
                        release.display(),
                        runtime.display()
                    ),
                ),
                (
                    "PATH".to_string(),
                    format!("{}/deps/bin:/usr/bin", release.display()),
                ),
                ("FLAME_PYTHON_VERSION".to_string(), "3.12".to_string()),
                (
                    "UV_CACHE_DIR".to_string(),
                    uv_cache.to_string_lossy().to_string(),
                ),
                (
                    "PIP_CACHE_DIR".to_string(),
                    pip_cache.to_string_lossy().to_string(),
                ),
            ]),
            mounts: vec![
                crate::appmgr::InstallationMount {
                    host_path: release,
                    kind: InstallationMountKind::Release,
                    readonly: true,
                },
                crate::appmgr::InstallationMount {
                    host_path: runtime,
                    kind: InstallationMountKind::PythonRuntime,
                    readonly: true,
                },
                crate::appmgr::InstallationMount {
                    host_path: uv_cache,
                    kind: InstallationMountKind::UvCache,
                    readonly: false,
                },
                crate::appmgr::InstallationMount {
                    host_path: pip_cache,
                    kind: InstallationMountKind::PipCache,
                    readonly: false,
                },
            ],
        };
        let mounts = container_mounts(&work_dir, &installation).unwrap();
        let installed = rebase_install_environment(&installation, &mounts);

        merge_install_environment(&mut environment, &installed);

        assert_eq!(
            environment["PYTHONPATH"],
            format!(
                "{0}/installation/deps:{0}/installation/src:{0}/runtime/site-packages:/application/path",
                work_dir.display()
            )
        );
        assert_eq!(
            environment["PATH"],
            format!(
                "{}/installation/deps/bin:/usr/local/bin",
                work_dir.display()
            )
        );
        assert_eq!(environment["FLAME_PYTHON_VERSION"], "3.12");
        assert_eq!(
            environment["UV_CACHE_DIR"],
            work_dir.join("cache/uv").to_string_lossy()
        );
        assert_eq!(
            environment["PIP_CACHE_DIR"],
            work_dir.join("cache/pip").to_string_lossy()
        );
        assert!(!environment.values().any(|value| value.contains("/usr/bin")));
    }

    #[test]
    fn cri_expands_commands_only_from_container_environment() {
        let environment = BTreeMap::from([
            ("FLAME_HOME".to_string(), "/opt/flame".to_string()),
            ("FLAME_PYTHON_VERSION".to_string(), "3.12".to_string()),
        ]);

        assert_eq!(
            expand_container_environment("${FLAME_HOME}/bin/uv", &environment).unwrap(),
            "/opt/flame/bin/uv"
        );
        assert_eq!(
            expand_container_environment("python${FLAME_PYTHON_VERSION}", &environment).unwrap(),
            "python3.12"
        );
        assert!(expand_container_environment("$HOST_ONLY_VALUE", &environment).is_err());
    }

    #[test]
    fn cri_rebase_requires_a_path_component_boundary() {
        let temp = tempdir().unwrap();
        let work_dir = temp.path().join("executor");
        let uv_cache = temp.path().join("cache/uv");
        let installation = ApplicationInstallation {
            env_vars: HashMap::from([(
                "UV_CACHE_DIR".to_string(),
                format!("{}-other", uv_cache.display()),
            )]),
            mounts: vec![crate::appmgr::InstallationMount {
                host_path: uv_cache,
                kind: InstallationMountKind::UvCache,
                readonly: false,
            }],
        };
        let mounts = container_mounts(&work_dir, &installation).unwrap();

        assert!(rebase_install_environment(&installation, &mounts).is_empty());
    }

    #[test]
    fn cri_uses_only_object_cache_ca() {
        let cache = FlameCache {
            tls: Some(FlameTls {
                cert_file: String::new(),
                key_file: String::new(),
                ca_file: Some("/cache/ca.crt".to_string()),
            }),
            ..Default::default()
        };

        assert_eq!(cache_ca_file(Some(&cache)), Some("/cache/ca.crt"));
        assert_eq!(cache_ca_file(None), None);
    }

    #[test]
    fn workload_name_includes_application_and_executor() {
        assert_eq!(
            workload_name("image-classifier", "executor-123"),
            "image-classifier-executor-123"
        );
    }

    #[test]
    fn loopback_cache_endpoints_are_rejected() {
        for endpoint in [
            "grpc://127.0.0.1:9090",
            "grpc://[::1]:9090",
            "grpc://localhost:9090",
        ] {
            assert!(reject_loopback_endpoint(endpoint).is_err());
        }
        assert!(reject_loopback_endpoint("grpc://10.0.0.10:9090").is_ok());
        assert!(reject_loopback_endpoint("grpcs-proxy://cache.example:443").is_ok());
    }

    #[test]
    fn executor_id_must_be_one_path_component() {
        assert!(validate_path_component("executor-1").is_ok());
        assert!(validate_path_component("../executor").is_err());
        assert!(validate_path_component("executor/child").is_err());
    }

    #[test]
    fn cri_environment_does_not_expose_session_manager() {
        let env = build_environment(
            &HashMap::new(),
            Some("grpc://cache.example:9090"),
            Path::new("/var/lib/flame/executors/e/instance.sock"),
            "info",
        );
        assert!(!env.contains_key(FLAME_ENDPOINT));
        assert_eq!(
            env.get(FLAME_CACHE_ENDPOINT).map(String::as_str),
            Some("grpc://cache.example:9090")
        );
    }

    #[test]
    fn directory_guard_removes_partial_create_directory() {
        let root = tempdir().unwrap();
        let work_dir = root.path().join("executor");
        fs::create_dir(&work_dir).unwrap();
        {
            let _guard = DirectoryGuard::new(work_dir.clone());
        }
        assert!(!work_dir.exists());
    }
}

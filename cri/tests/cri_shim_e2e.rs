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

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::FlameError;
use cri_rs::{
    ContainerRuntimeState, ContainerSecurityContext, ContainerSpec, Mount, ResourceLimits,
    WorkloadFilter, WorkloadHandle, WorkloadManager, WorkloadMetadata, WorkloadSpec,
    WorkloadStatus,
};
use uuid::Uuid;

const TEST_IMAGE: &str = "docker.io/library/nginx:latest";
const GVISOR_TEST_IMAGE: &str = "docker.io/library/busybox:1.36.1";
const TEST_UID: i64 = 65534;
const TEST_GID: i64 = 65534;
const TEST_SUPPLEMENTAL_GID: i64 = 65533;
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestDirectories {
    root: PathBuf,
    input: PathBuf,
    logs: PathBuf,
    work: PathBuf,
    release: PathBuf,
    runtime: PathBuf,
    uv_cache: PathBuf,
    pip_cache: PathBuf,
}

impl TestDirectories {
    fn create() -> Result<Self, FlameError> {
        let root = std::env::temp_dir().join(format!("flame-cri-test-{}", Uuid::new_v4()));
        let input = root.join("input");
        let logs = root.join("logs");
        let work = root.join("work");
        let release = root.join("release");
        let runtime = root.join("runtime");
        let uv_cache = root.join("cache/uv");
        let pip_cache = root.join("cache/pip");

        for directory in [
            &input, &logs, &work, &release, &runtime, &uv_cache, &pip_cache,
        ] {
            fs::create_dir_all(directory)?;
        }
        fs::write(input.join("fixture"), "mounted-input")?;
        for directory in [&input, &work, &uv_cache, &pip_cache] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o777))?;
        }

        Ok(Self {
            root,
            input,
            logs,
            work,
            release,
            runtime,
            uv_cache,
            pip_cache,
        })
    }

    fn result(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }

    fn log(&self) -> PathBuf {
        self.logs.join("application.log")
    }
}

impl Drop for TestDirectories {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct TestFixture {
    executor_id: String,
    filter: WorkloadFilter,
    directories: TestDirectories,
    manager: WorkloadManager,
    handles: Vec<WorkloadHandle>,
}

impl TestFixture {
    async fn create() -> Result<Self, FlameError> {
        let executor_id = format!("test-{}", Uuid::new_v4());
        let filter = WorkloadFilter::new(executor_id.clone())?;
        let directories = TestDirectories::create()?;
        let manager = WorkloadManager::connect().await?;
        if manager.version().is_empty() {
            return Err(FlameError::InvalidState(
                "CRI runtime returned an empty version".to_string(),
            ));
        }
        Ok(Self {
            executor_id,
            filter,
            directories,
            manager,
            handles: Vec::new(),
        })
    }

    async fn create_workload(&mut self, spec: WorkloadSpec) -> Result<WorkloadHandle, FlameError> {
        let handle = self.manager.create(&spec).await?;
        self.handles.push(handle.clone());
        Ok(handle)
    }

    async fn finish(mut self, verification: Result<(), FlameError>) -> Result<(), FlameError> {
        let mut cleanup_errors = Vec::new();
        for handle in self.handles.iter().rev() {
            if let Err(error) = self.manager.delete(handle).await {
                cleanup_errors.push(error.to_string());
            }
        }

        match (verification, cleanup_errors.is_empty()) {
            (Err(primary), false) => Err(FlameError::Internal(format!(
                "{primary}; cleanup also failed: {}",
                cleanup_errors.join("; ")
            ))),
            (Err(primary), true) => Err(primary),
            (Ok(()), false) => Err(FlameError::Internal(format!(
                "test cleanup failed: {}",
                cleanup_errors.join("; ")
            ))),
            (Ok(()), true) => Ok(()),
        }
    }
}

fn base_spec(executor_id: &str, directories: &TestDirectories) -> WorkloadSpec {
    let id = Uuid::new_v4().to_string();
    WorkloadSpec {
        metadata: WorkloadMetadata {
            name: format!("flame-cri-test-{id}"),
            namespace: "flame-test".to_string(),
            uid: id,
            executor_id: executor_id.to_string(),
            application: "nginx".to_string(),
        },
        containers: vec![ContainerSpec {
            name: "application".to_string(),
            image: TEST_IMAGE.to_string(),
            command: Some("/bin/sh".to_string()),
            args: Vec::new(),
            env: BTreeMap::new(),
            working_directory: "/flame-work".to_string(),
            mounts: vec![Mount {
                host_path: directories.work.to_string_lossy().to_string(),
                container_path: "/flame-work".to_string(),
                readonly: false,
            }],
            resources: ResourceLimits {
                cpu: 1,
                memory: 64 * 1024 * 1024,
            },
            security_context: ContainerSecurityContext {
                run_as_user: Some(TEST_UID),
                run_as_group: Some(TEST_GID),
                supplemental_groups: vec![TEST_SUPPLEMENTAL_GID],
            },
        }],
        log_directory: directories.logs.to_string_lossy().to_string(),
    }
}

fn shim_shaped_spec(executor_id: &str, directories: &TestDirectories) -> WorkloadSpec {
    let mut spec = base_spec(executor_id, directories);
    let container = &mut spec.containers[0];
    container.args = vec![
        "-c".to_string(),
        format!(
            "set -eu; \
             test \"$1\" = expected-argument; \
             test \"$FLAME_TEST_ENV\" = executor-shaped; \
             test \"$(pwd)\" = /flame-work; \
             test \"$(cat /flame-input/fixture)\" = mounted-input; \
             if touch /flame-input/forbidden 2>/dev/null; then exit 1; fi; \
             test \"$(id -u)\" = {TEST_UID}; \
             test \"$(id -g)\" = {TEST_GID}; \
             id -G | tr ' ' '\\n' | grep -qx {TEST_SUPPLEMENTAL_GID}; \
             printf 'log-marker\\n'; \
             printf ready > result; \
             exec sleep 300"
        ),
        "flame-cri-test".to_string(),
        "expected-argument".to_string(),
    ];
    container.env = BTreeMap::from([("FLAME_TEST_ENV".to_string(), "executor-shaped".to_string())]);
    container.mounts.insert(
        0,
        Mount {
            host_path: directories.input.to_string_lossy().to_string(),
            container_path: "/flame-input".to_string(),
            readonly: true,
        },
    );
    spec
}

fn gvisor_runtime_spec(executor_id: &str, directories: &TestDirectories) -> WorkloadSpec {
    let mut spec = base_spec(executor_id, directories);
    let container = &mut spec.containers[0];
    container.image = GVISOR_TEST_IMAGE.to_string();
    container.args = vec![
        "-c".to_string(),
        "set -eu; \
         dmesg | grep -q 'Starting gVisor'; \
         printf gvisor > /flame-work/runtime-result; \
         exec sleep 300"
            .to_string(),
    ];
    container.security_context = ContainerSecurityContext::default();
    spec
}

fn environment_spec(
    executor_id: &str,
    directories: &TestDirectories,
) -> Result<WorkloadSpec, FlameError> {
    let release_app = directories.release.join("app");
    let release_bin = directories.release.join("bin");
    let runtime_native = directories.runtime.join("native");
    fs::create_dir_all(&release_app)?;
    fs::create_dir_all(&release_bin)?;
    fs::create_dir_all(&runtime_native)?;
    fs::write(release_app.join("fixture"), "application-release")?;
    fs::write(directories.runtime.join("fixture"), "python-runtime")?;
    fs::write(directories.work.join("ca.crt"), "cache-ca")?;
    fs::set_permissions(&release_app, fs::Permissions::from_mode(0o777))?;
    fs::set_permissions(&directories.runtime, fs::Permissions::from_mode(0o777))?;
    let tool = release_bin.join("flame-tool");
    fs::write(&tool, "#!/bin/sh\nprintf installer-tool\n")?;
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755))?;

    let mut spec = base_spec(executor_id, directories);
    let container = &mut spec.containers[0];
    container.args = vec![
        "-c".to_string(),
        "set -eu; \
         test \"$FLAME_INSTANCE_ENDPOINT\" = /flame-work/instance.sock; \
         test \"$FLAME_CACHE_ENDPOINT\" = grpc://10.250.0.1:9090; \
         test \"$FLAME_LOG\" = debug; \
         test \"$RUST_LOG\" = debug; \
         test \"$APPLICATION_SETTING\" = application-value; \
         test \"$FLAME_PYTHON_VERSION\" = 3.12; \
         test \"$FLAME_CA_FILE\" = /flame-work/ca.crt; \
         test -z \"${FLAME_ENDPOINT+x}\"; \
         test \"$(cat \"$FLAME_CA_FILE\")\" = cache-ca; \
         test \"$(cat \"$FLAME_APP_DIR/fixture\")\" = application-release; \
         test \"$(cat \"$PYTHONPATH/fixture\")\" = python-runtime; \
         test \"$LD_LIBRARY_PATH\" = /flame-runtime/site-packages/native; \
         if touch \"$FLAME_APP_DIR/forbidden\" 2>/dev/null; then exit 1; fi; \
         if touch \"$PYTHONPATH/forbidden\" 2>/dev/null; then exit 1; fi; \
         test \"$(flame-tool)\" = installer-tool; \
         printf uv-ready > \"$UV_CACHE_DIR/result\"; \
         printf pip-ready > \"$PIP_CACHE_DIR/result\"; \
         printf env-ready > /flame-work/environment-result; \
         exec sleep 300"
            .to_string(),
    ];
    container.env = BTreeMap::from([
        (
            "FLAME_INSTANCE_ENDPOINT".to_string(),
            "/flame-work/instance.sock".to_string(),
        ),
        (
            "FLAME_CACHE_ENDPOINT".to_string(),
            "grpc://10.250.0.1:9090".to_string(),
        ),
        ("FLAME_LOG".to_string(), "debug".to_string()),
        ("RUST_LOG".to_string(), "debug".to_string()),
        (
            "APPLICATION_SETTING".to_string(),
            "application-value".to_string(),
        ),
        ("FLAME_PYTHON_VERSION".to_string(), "3.12".to_string()),
        (
            "FLAME_CA_FILE".to_string(),
            "/flame-work/ca.crt".to_string(),
        ),
        (
            "FLAME_APP_DIR".to_string(),
            "/flame-installation/app".to_string(),
        ),
        (
            "PATH".to_string(),
            "/flame-installation/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
        ),
        (
            "PYTHONPATH".to_string(),
            "/flame-runtime/site-packages".to_string(),
        ),
        (
            "LD_LIBRARY_PATH".to_string(),
            "/flame-runtime/site-packages/native".to_string(),
        ),
        ("UV_CACHE_DIR".to_string(), "/flame-cache/uv".to_string()),
        ("PIP_CACHE_DIR".to_string(), "/flame-cache/pip".to_string()),
    ]);
    container.mounts.extend([
        Mount {
            host_path: directories.release.to_string_lossy().to_string(),
            container_path: "/flame-installation".to_string(),
            readonly: true,
        },
        Mount {
            host_path: directories.runtime.to_string_lossy().to_string(),
            container_path: "/flame-runtime/site-packages".to_string(),
            readonly: true,
        },
        Mount {
            host_path: directories.uv_cache.to_string_lossy().to_string(),
            container_path: "/flame-cache/uv".to_string(),
            readonly: false,
        },
        Mount {
            host_path: directories.pip_cache.to_string_lossy().to_string(),
            container_path: "/flame-cache/pip".to_string(),
            readonly: false,
        },
    ]);
    Ok(spec)
}

async fn wait_for_file_and_running(
    manager: &mut WorkloadManager,
    handle: &WorkloadHandle,
    file: &Path,
) -> Result<WorkloadStatus, FlameError> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            let status = manager.status(handle).await?;
            if status.containers.iter().any(|container| {
                matches!(
                    container.state,
                    ContainerRuntimeState::Exited | ContainerRuntimeState::Unknown
                )
            }) {
                return Err(FlameError::InvalidState(format!(
                    "test container exited before writing <{}>",
                    file.display()
                )));
            }
            if file.is_file() {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        FlameError::InvalidState(format!(
            "test container did not write <{}> within {} seconds",
            file.display(),
            WAIT_TIMEOUT.as_secs()
        ))
    })?
}

async fn wait_for_exited(
    manager: &mut WorkloadManager,
    handle: &WorkloadHandle,
) -> Result<WorkloadStatus, FlameError> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            let status = manager.status(handle).await?;
            if status
                .containers
                .iter()
                .any(|container| container.state == ContainerRuntimeState::Exited)
            {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        FlameError::InvalidState(format!(
            "test container did not exit within {} seconds",
            WAIT_TIMEOUT.as_secs()
        ))
    })?
}

async fn wait_until_absent(
    manager: &mut WorkloadManager,
    filter: &WorkloadFilter,
    sandbox_id: &str,
) -> Result<(), FlameError> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            if !manager
                .list_workload(filter)
                .await?
                .iter()
                .any(|handle| handle.sandbox_id() == sandbox_id)
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        FlameError::InvalidState(format!(
            "sandbox <{sandbox_id}> remained listed for {} seconds after deletion",
            WAIT_TIMEOUT.as_secs()
        ))
    })?
}

#[tokio::test]
#[ignore = "requires a configured containerd CRI v1 service"]
async fn create_runs_shim_shaped_workload() -> Result<(), FlameError> {
    let _guard = TEST_LOCK.lock().await;
    let mut fixture = TestFixture::create().await?;
    let spec = shim_shaped_spec(&fixture.executor_id, &fixture.directories);
    let handle = fixture.create_workload(spec).await?;
    let verification = async {
        let result = fixture.directories.result("result");
        let status = wait_for_file_and_running(&mut fixture.manager, &handle, &result).await?;
        if !fixture.directories.log().is_file() {
            return Err(FlameError::InvalidState(format!(
                "containerd did not create log <{}>",
                fixture.directories.log().display()
            )));
        }
        if status.sandbox_id != handle.sandbox_id() || !status.healthy() {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> returned unhealthy status: {status:?}",
                handle.sandbox_id()
            )));
        }
        let [container] = status.containers.as_slice() else {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> returned {} containers, expected one",
                handle.sandbox_id(),
                status.containers.len()
            )));
        };
        if container.name != "application"
            || container.image != TEST_IMAGE
            || container.state != ContainerRuntimeState::Running
        {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> returned unexpected container status: {container:?}",
                handle.sandbox_id()
            )));
        }
        if fs::read_to_string(result)? != "ready" {
            return Err(FlameError::InvalidState(
                "test container wrote an unexpected result".to_string(),
            ));
        }
        Ok(())
    }
    .await;
    fixture.finish(verification).await
}

#[tokio::test]
#[ignore = "requires containerd with gVisor configured as the default CRI runtime"]
async fn empty_runtime_handler_uses_gvisor_default() -> Result<(), FlameError> {
    let _guard = TEST_LOCK.lock().await;
    let mut fixture = TestFixture::create().await?;
    let spec = gvisor_runtime_spec(&fixture.executor_id, &fixture.directories);
    let handle = fixture.create_workload(spec).await?;
    let verification = async {
        let result = fixture.directories.result("runtime-result");
        let status = wait_for_file_and_running(&mut fixture.manager, &handle, &result).await?;
        if !status.healthy() || fs::read_to_string(result)? != "gvisor" {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> did not use the configured gVisor default: {status:?}",
                handle.sandbox_id()
            )));
        }
        Ok(())
    }
    .await;
    fixture.finish(verification).await
}

#[tokio::test]
#[ignore = "requires a configured containerd CRI v1 service"]
async fn create_exposes_cri_shim_and_installer_environment() -> Result<(), FlameError> {
    let _guard = TEST_LOCK.lock().await;
    let mut fixture = TestFixture::create().await?;
    let spec = environment_spec(&fixture.executor_id, &fixture.directories)?;
    let handle = fixture.create_workload(spec).await?;
    let verification = async {
        let result = fixture.directories.result("environment-result");
        wait_for_file_and_running(&mut fixture.manager, &handle, &result).await?;
        for (path, expected) in [
            (result, "env-ready"),
            (fixture.directories.uv_cache.join("result"), "uv-ready"),
            (fixture.directories.pip_cache.join("result"), "pip-ready"),
        ] {
            if fs::read_to_string(&path)? != expected {
                return Err(FlameError::InvalidState(format!(
                    "test container wrote an unexpected result to <{}>",
                    path.display()
                )));
            }
        }
        Ok(())
    }
    .await;
    fixture.finish(verification).await
}

#[tokio::test]
#[ignore = "requires a configured containerd CRI v1 service"]
async fn status_reports_exited_container_without_command_override() -> Result<(), FlameError> {
    let _guard = TEST_LOCK.lock().await;
    let mut fixture = TestFixture::create().await?;
    let mut spec = base_spec(&fixture.executor_id, &fixture.directories);
    let container = &mut spec.containers[0];
    container.command = None;
    container.args = vec!["sh".to_string(), "-c".to_string(), "exit 23".to_string()];
    let handle = fixture.create_workload(spec).await?;
    let verification = async {
        let status = wait_for_exited(&mut fixture.manager, &handle).await?;
        let [container] = status.containers.as_slice() else {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> returned {} containers, expected one",
                handle.sandbox_id(),
                status.containers.len()
            )));
        };
        if container.exit_code != 23 || status.healthy() {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> returned unexpected exited status: {status:?}",
                handle.sandbox_id()
            )));
        }
        Ok(())
    }
    .await;
    fixture.finish(verification).await
}

#[tokio::test]
#[ignore = "requires a configured containerd CRI v1 service"]
async fn list_workload_is_executor_scoped_after_reconnect() -> Result<(), FlameError> {
    let _guard = TEST_LOCK.lock().await;
    let mut fixture = TestFixture::create().await?;
    let spec = shim_shaped_spec(&fixture.executor_id, &fixture.directories);
    let handle = fixture.create_workload(spec).await?;
    let verification = async {
        let result = fixture.directories.result("result");
        wait_for_file_and_running(&mut fixture.manager, &handle, &result).await?;

        let mut recovered = WorkloadManager::connect().await?;
        let listed = recovered.list_workload(&fixture.filter).await?;
        let recovered_handle = listed
            .into_iter()
            .find(|item| item.sandbox_id() == handle.sandbox_id())
            .ok_or_else(|| {
                FlameError::InvalidState(format!(
                    "created sandbox <{}> was not listed after reconnect",
                    handle.sandbox_id()
                ))
            })?;
        if recovered_handle.container_ids().is_empty() {
            return Err(FlameError::InvalidState(format!(
                "recovered sandbox <{}> has no containers",
                handle.sandbox_id()
            )));
        }
        let status = recovered.status(&recovered_handle).await?;
        if !status.healthy() {
            return Err(FlameError::InvalidState(format!(
                "recovered sandbox <{}> is not healthy",
                handle.sandbox_id()
            )));
        }

        let foreign_filter = WorkloadFilter::new(format!("foreign-{}", fixture.executor_id))?;
        if recovered
            .list_workload(&foreign_filter)
            .await?
            .iter()
            .any(|item| item.sandbox_id() == handle.sandbox_id())
        {
            return Err(FlameError::InvalidState(format!(
                "sandbox <{}> was visible through a foreign executor filter",
                handle.sandbox_id()
            )));
        }
        recovered.delete(&recovered_handle).await?;
        wait_until_absent(&mut recovered, &fixture.filter, handle.sandbox_id()).await?;
        Ok(())
    }
    .await;
    fixture.finish(verification).await
}

#[tokio::test]
#[ignore = "requires a configured containerd CRI v1 service"]
async fn delete_removes_workload_and_is_idempotent() -> Result<(), FlameError> {
    let _guard = TEST_LOCK.lock().await;
    let mut fixture = TestFixture::create().await?;
    let spec = shim_shaped_spec(&fixture.executor_id, &fixture.directories);
    let handle = fixture.create_workload(spec).await?;
    let verification = async {
        let result = fixture.directories.result("result");
        wait_for_file_and_running(&mut fixture.manager, &handle, &result).await?;
        fixture.manager.delete(&handle).await?;
        wait_until_absent(&mut fixture.manager, &fixture.filter, handle.sandbox_id()).await?;
        fixture.manager.delete(&handle).await?;
        Ok(())
    }
    .await;
    fixture.finish(verification).await
}

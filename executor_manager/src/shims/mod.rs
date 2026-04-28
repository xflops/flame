/*
Copyright 2023 The Flame Authors.
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

mod grpc_shim;
mod host_shim;
#[cfg(feature = "wasm")]
mod wasm_shim;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use self::host_shim::HostShim;
#[cfg(feature = "wasm")]
use self::wasm_shim::WasmShim;

use crate::executor::Executor;
use common::apis::{
    ApplicationContext, SessionContext, Shim as ShimType, TaskContext, TaskOutput, TaskResult,
};
use common::{FlameError, FLAME_WORKING_DIRECTORY};

pub type ShimPtr = Arc<Mutex<dyn Shim>>;

/// Represents the executor's working directory with cleanup management.
/// Directory structure:
///   top_dir/                     - Process working directory, stdout/stderr logs
///   top_dir/work/<app_name>/     - App-specific directory for tmp, cache
///   /var/flame/executors/<executor_id>.sock - Socket for gRPC communication
/// Cleanup:
///   - top_dir: cleaned up only if auto-generated
///   - app_dir: always cleaned up
///   - socket: always cleaned up
pub struct ExecutorWorkDir {
    /// Top-level working directory (process runs here).
    top_dir: PathBuf,
    /// Application working directory: top_dir/work/<app-name> (for logs, tmp, cache).
    app_dir: PathBuf,
    /// Socket path: /var/flame/executors/<executor_id>.sock
    socket: PathBuf,
    /// If true, top_dir was auto-generated and should be cleaned up on release.
    auto_dir: bool,
}

const FLAME_SOCKET_DIR: &str = "/var/flame/executors";

fn get_socket_dir() -> PathBuf {
    std::env::var("FLAME_SOCKET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(FLAME_SOCKET_DIR))
}

impl ExecutorWorkDir {
    /// Create an ExecutorWorkDir from application context and executor ID.
    pub fn new(app: &ApplicationContext, executor_id: &str) -> Result<Self, FlameError> {
        let (top_dir, auto_dir) = match &app.working_directory {
            Some(wd) if !wd.is_empty() => (Path::new(wd).to_path_buf(), false),
            _ => (
                env::current_dir()
                    .unwrap_or(Path::new(FLAME_WORKING_DIRECTORY).to_path_buf())
                    .join(executor_id),
                true,
            ),
        };

        let work_dir = top_dir.join("work");
        let app_dir = work_dir.join(&app.name);
        let socket_dir = get_socket_dir();
        let socket = socket_dir.join(format!("{}.sock", executor_id));

        // Create top_dir if auto-generated
        if auto_dir {
            fs::create_dir_all(&top_dir).map_err(|e| {
                FlameError::Internal(format!(
                    "failed to create top working directory {}: {e}",
                    top_dir.display()
                ))
            })?;
        }

        // Always create work dir
        fs::create_dir_all(&work_dir).map_err(|e| {
            FlameError::Internal(format!(
                "failed to create work directory {}: {e}",
                work_dir.display()
            ))
        })?;

        // Always create app_dir (for logs, tmp, cache)
        fs::create_dir_all(&app_dir).map_err(|e| {
            FlameError::Internal(format!(
                "failed to create app working directory {}: {e}",
                app_dir.display()
            ))
        })?;

        // Create socket directory
        fs::create_dir_all(&socket_dir).map_err(|e| {
            FlameError::Internal(format!(
                "failed to create socket directory {}: {e}",
                socket_dir.display()
            ))
        })?;

        Ok(Self {
            top_dir,
            app_dir,
            socket,
            auto_dir,
        })
    }

    /// Returns the application working directory path (for tmp, cache).
    pub fn app_dir(&self) -> &Path {
        &self.app_dir
    }

    /// Returns the directory where the process should run (always top_dir).
    pub fn process_dir(&self) -> &Path {
        &self.top_dir
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for ExecutorWorkDir {
    fn drop(&mut self) {
        // Always cleanup socket file
        if self.socket.exists() {
            if let Err(e) = fs::remove_file(&self.socket) {
                tracing::warn!(
                    "Failed to remove socket file {}: {}",
                    self.socket.display(),
                    e
                );
            } else {
                tracing::debug!("Removed socket file: {}", self.socket.display());
            }
        }

        // Always cleanup app_dir
        if self.app_dir.exists() {
            if let Err(e) = fs::remove_dir_all(&self.app_dir) {
                tracing::warn!(
                    "Failed to remove app working directory {}: {}",
                    self.app_dir.display(),
                    e
                );
            } else {
                tracing::debug!("Removed app working directory: {}", self.app_dir.display());
            }
        }

        // Cleanup top_dir only if auto-generated
        if self.auto_dir && self.top_dir.exists() {
            if let Err(e) = fs::remove_dir_all(&self.top_dir) {
                tracing::warn!(
                    "Failed to remove executor working directory {}: {}",
                    self.top_dir.display(),
                    e
                );
            } else {
                tracing::debug!(
                    "Removed executor working directory: {}",
                    self.top_dir.display()
                );
            }
        }
    }
}

/// Create a new shim instance based on executor's cluster context configuration.
/// The shim type is determined by the executor-manager's flame-cluster.yaml config,
/// not from the application context (which is deprecated).
pub async fn new(
    executor: &Executor,
    app: &ApplicationContext,
    install_env_vars: &HashMap<String, String>,
) -> Result<ShimPtr, FlameError> {
    // Get shim type from executor's cluster context configuration
    let shim_type = executor
        .context
        .as_ref()
        .map(|ctx| ctx.cluster.executors.shim)
        .unwrap_or(ShimType::Host);

    tracing::info!(
        "Creating shim for executor <{}> with type: {:?} (from executor-manager config)",
        executor.id,
        shim_type
    );

    match shim_type {
        #[cfg(feature = "wasm")]
        ShimType::Wasm => Ok(WasmShim::new_ptr(executor, app, install_env_vars).await?),
        #[cfg(not(feature = "wasm"))]
        ShimType::Wasm => Err(FlameError::InvalidConfig(
            "WASM shim is not enabled. Rebuild with --features wasm".to_string(),
        )),
        ShimType::Host => Ok(HostShim::new_ptr(executor, app, install_env_vars).await?),
    }
}

#[async_trait]
pub trait Shim: Send + 'static {
    async fn on_session_enter(&mut self, ctx: &SessionContext) -> Result<(), FlameError>;
    async fn on_task_invoke(&mut self, ctx: &TaskContext) -> Result<TaskResult, FlameError>;
    async fn on_session_leave(&mut self) -> Result<(), FlameError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs::File;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn create_test_app(name: &str, working_directory: Option<String>) -> ApplicationContext {
        ApplicationContext {
            name: name.to_string(),
            shim: common::apis::Shim::Host,
            image: None,
            command: None,
            arguments: vec![],
            working_directory,
            environments: HashMap::new(),
            url: None,
            installer: None,
        }
    }

    fn setup_test_env(temp: &tempfile::TempDir) -> PathBuf {
        let socket_dir = temp.path().join("sockets");
        std::fs::create_dir_all(&socket_dir).unwrap();
        std::env::set_var("FLAME_SOCKET_DIR", &socket_dir);
        std::env::set_current_dir(temp.path()).unwrap();
        socket_dir
    }

    #[test]
    fn test_executor_work_dir_with_auto_dir() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        let socket_dir = setup_test_env(&temp);

        let app = create_test_app("test-app", None);
        let executor_id = "exec-123";

        let work_dir = ExecutorWorkDir::new(&app, executor_id).unwrap();

        assert!(work_dir.process_dir().ends_with(executor_id));
        assert!(work_dir.app_dir().ends_with("test-app"));
        assert_eq!(
            work_dir.socket(),
            socket_dir.join("exec-123.sock").as_path()
        );
        assert!(work_dir.process_dir().exists());
        assert!(work_dir.app_dir().exists());
    }

    #[test]
    fn test_executor_work_dir_with_custom_working_directory() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        let socket_dir = setup_test_env(&temp);
        let custom_dir = temp.path().join("custom-workdir");
        std::fs::create_dir_all(&custom_dir).unwrap();

        let app = create_test_app("test-app", Some(custom_dir.to_string_lossy().to_string()));
        let executor_id = "exec-456";

        let work_dir = ExecutorWorkDir::new(&app, executor_id).unwrap();

        assert_eq!(work_dir.process_dir(), custom_dir.as_path());
        assert_eq!(work_dir.app_dir(), custom_dir.join("work").join("test-app"));
        assert_eq!(
            work_dir.socket(),
            socket_dir.join("exec-456.sock").as_path()
        );
    }

    #[test]
    fn test_executor_work_dir_socket_path_length() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);

        let app = create_test_app("test-app", None);
        let long_executor_id = "550e8400-e29b-41d4-a716-446655440000";

        let work_dir = ExecutorWorkDir::new(&app, long_executor_id).unwrap();

        let socket_path = work_dir.socket();
        let default_path = format!("{}/{}.sock", FLAME_SOCKET_DIR, long_executor_id);
        assert!(
            default_path.len() < 104,
            "Default socket path should be under SUN_LEN limit: {} chars",
            default_path.len()
        );
    }

    #[test]
    fn test_executor_work_dir_cleanup_on_drop() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);

        let app = create_test_app("test-app", None);
        let executor_id = "exec-drop-test";

        let top_dir: PathBuf;
        let app_dir: PathBuf;
        let socket_path: PathBuf;

        {
            let work_dir = ExecutorWorkDir::new(&app, executor_id).unwrap();
            top_dir = work_dir.process_dir().to_path_buf();
            app_dir = work_dir.app_dir().to_path_buf();
            socket_path = work_dir.socket().to_path_buf();

            assert!(top_dir.exists());
            assert!(app_dir.exists());

            File::create(&socket_path).unwrap();
            assert!(socket_path.exists());
        }

        assert!(!socket_path.exists(), "Socket should be cleaned up on drop");
        assert!(!app_dir.exists(), "App dir should be cleaned up on drop");
        assert!(
            !top_dir.exists(),
            "Top dir should be cleaned up on drop (auto_dir=true)"
        );
    }

    #[test]
    fn test_executor_work_dir_no_cleanup_custom_dir_on_drop() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        setup_test_env(&temp);
        let custom_dir = temp.path().join("persistent-workdir");
        std::fs::create_dir_all(&custom_dir).unwrap();

        let app = create_test_app("test-app", Some(custom_dir.to_string_lossy().to_string()));
        let executor_id = "exec-persist-test";

        let socket_path: PathBuf;

        {
            let work_dir = ExecutorWorkDir::new(&app, executor_id).unwrap();
            socket_path = work_dir.socket().to_path_buf();

            File::create(&socket_path).unwrap();
        }

        assert!(!socket_path.exists(), "Socket should be cleaned up on drop");
        assert!(
            custom_dir.exists(),
            "Custom top_dir should NOT be cleaned up (auto_dir=false)"
        );
    }

    #[test]
    fn test_socket_path_is_fixed_location() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        let socket_dir = setup_test_env(&temp);

        let app1 = create_test_app("app1", None);
        let custom_work_dir = temp.path().join("custom");
        std::fs::create_dir_all(&custom_work_dir).unwrap();
        let app2 = create_test_app("app2", Some(custom_work_dir.to_string_lossy().to_string()));

        let work_dir1 = ExecutorWorkDir::new(&app1, "exec-1").unwrap();
        let work_dir2 = ExecutorWorkDir::new(&app2, "exec-2").unwrap();

        assert_eq!(work_dir1.socket().parent().unwrap(), socket_dir.as_path());
        assert_eq!(work_dir2.socket().parent().unwrap(), socket_dir.as_path());
    }
}

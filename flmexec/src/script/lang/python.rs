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

use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use rand::Rng;

use flame_rs::{apis::FlameError, FLAME_PYTHON_VERSION_ENV};
use stdng::trace_fn;

use crate::api::Script;
use crate::script::{ScriptEngine, ScriptRuntime};

const DEFAULT_PYTHON_VERSION: &str = "3.12";
const DEFAULT_ENTRYPOINT: &str = "main.py";
const DEFAULT_FLAME_HOME: &str = "/usr/local/flame";
const PYTHONPATH_ENV: &str = "PYTHONPATH";
const FLAME_HOME_ENV: &str = "FLAME_HOME";
const FLAME_ENDPOINT_ENV: &str = "FLAME_ENDPOINT";
const FLAME_CACHE_ENDPOINT_ENV: &str = "FLAME_CACHE_ENDPOINT";
const FLAME_CA_FILE_ENV: &str = "FLAME_CA_FILE";
const FLAME_CERT_FILE_ENV: &str = "FLAME_CERT_FILE";
const FLAME_KEY_FILE_ENV: &str = "FLAME_KEY_FILE";
const PROPAGATED_ENV_VARS: &[&str] = &[
    // Python/Flame
    PYTHONPATH_ENV,
    FLAME_HOME_ENV,
    FLAME_PYTHON_VERSION_ENV,
    FLAME_ENDPOINT_ENV,
    FLAME_CACHE_ENDPOINT_ENV,
    FLAME_CA_FILE_ENV,
    FLAME_CERT_FILE_ENV,
    FLAME_KEY_FILE_ENV,
    // uv cache and config
    "UV_CACHE_DIR",
    "UV_PYTHON_INSTALL_DIR",
    "XDG_CACHE_HOME",
    // System essentials
    "PATH",
    "HOME",
    "USER",
    "TMPDIR",
    "TMP",
    "TEMP",
];

/// Get the uv command path from FLAME_HOME or fallback to system uv
fn get_uv_cmd() -> String {
    let flame_home = env::var(FLAME_HOME_ENV).unwrap_or_else(|_| DEFAULT_FLAME_HOME.to_string());
    let uv_path = format!("{}/bin/uv", flame_home);

    // Check if uv exists in FLAME_HOME, otherwise fallback to system uv
    if std::path::Path::new(&uv_path).exists() {
        uv_path
    } else {
        "/usr/bin/uv".to_string()
    }
}

fn configure_python_env(envs: &mut HashMap<String, String>, runtime: Option<&str>) -> String {
    let flame_home = flame_home(envs);
    envs.entry(FLAME_HOME_ENV.to_string())
        .or_insert_with(|| flame_home.to_string_lossy().to_string());

    let python_version = python_version(envs, &flame_home, runtime);
    envs.insert(FLAME_PYTHON_VERSION_ENV.to_string(), python_version.clone());

    let site_packages = site_packages_path(&flame_home, &python_version);
    if site_packages.is_dir() {
        prepend_path_env(envs, PYTHONPATH_ENV, &site_packages);
    } else {
        tracing::debug!(
            "Flame Python site-packages not found: {}",
            site_packages.display()
        );
    }

    python_version
}

fn flame_home(envs: &HashMap<String, String>) -> PathBuf {
    envs.get(FLAME_HOME_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FLAME_HOME))
}

fn python_version(
    envs: &HashMap<String, String>,
    flame_home: &Path,
    runtime: Option<&str>,
) -> String {
    runtime
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(version_number)
        .map(ToOwned::to_owned)
        .or_else(|| {
            envs.get(FLAME_PYTHON_VERSION_ENV)
                .map(|version| version_number(version))
                .filter(|version| !version.is_empty())
                .map(ToOwned::to_owned)
        })
        .or_else(|| latest_installed_python_version(flame_home))
        .unwrap_or_else(|| DEFAULT_PYTHON_VERSION.to_string())
}

fn version_number(version: &str) -> &str {
    version.strip_prefix("python").unwrap_or(version)
}

fn latest_installed_python_version(flame_home: &Path) -> Option<String> {
    let lib_path = flame_home.join("lib");
    let mut versions = fs::read_dir(lib_path)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() || !path.join("site-packages").is_dir() {
                return None;
            }

            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("python")
                .filter(|version| !version.is_empty())
                .map(|version| version.to_string())
        })
        .collect::<Vec<_>>();

    versions.sort_by_key(|version| minor_version(version));
    versions.pop()
}

fn minor_version(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0))
        .collect()
}

fn site_packages_path(flame_home: &Path, version: &str) -> PathBuf {
    flame_home
        .join("lib")
        .join(format!("python{}", version_number(version)))
        .join("site-packages")
}

fn prepend_path_env(envs: &mut HashMap<String, String>, key: &str, path: &Path) {
    let mut paths = vec![path.to_path_buf()];
    if let Some(existing) = envs.get(key) {
        paths.extend(
            env::split_paths(existing)
                .filter(|existing_path| !existing_path.as_os_str().is_empty())
                .filter(|existing_path| existing_path != path),
        );
    }

    match env::join_paths(paths) {
        Ok(joined) => {
            envs.insert(key.to_string(), joined.to_string_lossy().to_string());
        }
        Err(e) => {
            tracing::warn!("Failed to build {key} with Flame site-packages: {e}");
        }
    }
}

pub struct PythonScript {
    runtime: ScriptRuntime,
}

impl PythonScript {
    pub fn new(script: &Script) -> Result<Self, FlameError> {
        trace_fn!("PythonScript::new");

        let mut rng = rand::rng();
        let work_dir_path = format!("/tmp/flame/script/python-{}", rng.random::<u32>());
        let work_dir = Path::new(&work_dir_path);

        fs::create_dir_all(work_dir).map_err(|e| FlameError::Internal(e.to_string()))?;
        tracing::debug!("Created work directory: {work_dir_path}");

        let entrypoint = DEFAULT_ENTRYPOINT;

        let mut file = File::create(work_dir.join(entrypoint))
            .map_err(|e| FlameError::Internal(e.to_string()))?;
        file.write_all(script.code.as_bytes())
            .map_err(|e| FlameError::Internal(e.to_string()))?;

        let full_path = work_dir.join(entrypoint);

        // Propagate essential environment variables from parent process
        let mut env = HashMap::new();
        for key in PROPAGATED_ENV_VARS {
            if let Ok(value) = std::env::var(key) {
                env.insert((*key).to_string(), value);
            }
        }
        configure_python_env(&mut env, script.runtime.as_deref());

        let runtime = ScriptRuntime {
            entrypoint: full_path.to_string_lossy().to_string(),
            work_dir: work_dir.to_string_lossy().to_string(),
            input: script.input.clone(),
            env,
        };

        Ok(Self { runtime })
    }
}

impl ScriptEngine for PythonScript {
    fn run(&self) -> Result<Option<Vec<u8>>, FlameError> {
        trace_fn!("PythonScript::run");

        tracing::debug!("Running script: {}", self.runtime.entrypoint);
        tracing::debug!("Work directory: {}", self.runtime.work_dir);

        let uv_cmd = get_uv_cmd();
        tracing::debug!("Using uv from: {}", uv_cmd);

        let python_version = self
            .runtime
            .env
            .get(FLAME_PYTHON_VERSION_ENV)
            .map(String::as_str)
            .filter(|version| !version.is_empty())
            .unwrap_or(DEFAULT_PYTHON_VERSION);
        tracing::debug!("Using Python version: {}", python_version);

        let mut child = Command::new(uv_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .current_dir(&self.runtime.work_dir)
            .args(["run", "--python", python_version, &self.runtime.entrypoint])
            .envs(self.runtime.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .spawn()
            .map_err(|e| FlameError::Internal(format!("failed to start subprocess: {e}")))?;

        tracing::debug!("Spawned child process: {}", child.id());
        let mut stdin = child.stdin.take().unwrap();
        if let Some(input) = &self.runtime.input {
            let input = input.clone();
            let _handler = thread::spawn(move || {
                match stdin.write_all(&input) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Failed to send input into shim instance: {e}.");
                    }
                };
            });
            tracing::debug!("Sent input into child process.");
        }

        let mut stdout = child.stdout.take().unwrap();
        let mut data = vec![];
        let n = stdout
            .read_to_end(&mut data)
            .map_err(|_| FlameError::Internal("failed to read task output".to_string()))?;

        tracing::debug!("Read <{n}> data from child process.");

        match child.wait() {
            Ok(es) => {
                if !es.success() {
                    tracing::info!("Child process exist with error: {es}");
                }
            }
            Err(e) => {
                tracing::error!("Failed to wait child process: {e}")
            }
        };

        tracing::debug!("Child process exited.");

        Ok(Some(data))
    }
}

impl Drop for PythonScript {
    fn drop(&mut self) {
        trace_fn!("PythonScript::drop");

        fs::remove_dir_all(Path::new(&self.runtime.work_dir)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propagates_nested_flame_client_env_vars() {
        assert!(PROPAGATED_ENV_VARS.contains(&FLAME_ENDPOINT_ENV));
        assert!(PROPAGATED_ENV_VARS.contains(&FLAME_CACHE_ENDPOINT_ENV));
        assert!(PROPAGATED_ENV_VARS.contains(&FLAME_CA_FILE_ENV));
        assert!(PROPAGATED_ENV_VARS.contains(&FLAME_CERT_FILE_ENV));
        assert!(PROPAGATED_ENV_VARS.contains(&FLAME_KEY_FILE_ENV));
    }

    #[test]
    fn configures_default_python_version() {
        let temp = tempfile::tempdir().unwrap();
        let site_packages = temp
            .path()
            .join(format!("lib/python{DEFAULT_PYTHON_VERSION}/site-packages"));
        fs::create_dir_all(&site_packages).unwrap();

        let existing_path = temp.path().join("existing-pythonpath");
        let mut envs = HashMap::from([
            (
                FLAME_HOME_ENV.to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                PYTHONPATH_ENV.to_string(),
                existing_path.to_string_lossy().to_string(),
            ),
        ]);

        let python_version = configure_python_env(&mut envs, None);

        assert_eq!(python_version, DEFAULT_PYTHON_VERSION);
        assert_eq!(
            envs.get(FLAME_PYTHON_VERSION_ENV).unwrap(),
            DEFAULT_PYTHON_VERSION
        );

        let python_paths = env::split_paths(envs.get(PYTHONPATH_ENV).unwrap()).collect::<Vec<_>>();
        assert_eq!(python_paths, vec![site_packages, existing_path]);
    }

    #[test]
    fn normalizes_script_runtime_python_version() {
        let temp = tempfile::tempdir().unwrap();
        let site_packages = temp.path().join("lib/python3.11/site-packages");
        fs::create_dir_all(&site_packages).unwrap();

        let mut envs = HashMap::from([(
            FLAME_HOME_ENV.to_string(),
            temp.path().to_string_lossy().to_string(),
        )]);

        let python_version = configure_python_env(&mut envs, Some("python3.11"));

        assert_eq!(python_version, "3.11");
        assert_eq!(envs.get(FLAME_PYTHON_VERSION_ENV).unwrap(), "3.11");
        assert_eq!(
            env::split_paths(envs.get(PYTHONPATH_ENV).unwrap()).collect::<Vec<_>>(),
            vec![site_packages]
        );
    }

    #[test]
    fn configures_env_python_version() {
        let temp = tempfile::tempdir().unwrap();
        let site_packages = temp.path().join("lib/python3.11/site-packages");
        fs::create_dir_all(&site_packages).unwrap();

        let mut envs = HashMap::from([
            (
                FLAME_HOME_ENV.to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                FLAME_PYTHON_VERSION_ENV.to_string(),
                "python3.11".to_string(),
            ),
        ]);

        let python_version = configure_python_env(&mut envs, None);

        assert_eq!(python_version, "3.11");
        assert_eq!(envs.get(FLAME_PYTHON_VERSION_ENV).unwrap(), "3.11");
        assert_eq!(
            env::split_paths(envs.get(PYTHONPATH_ENV).unwrap()).collect::<Vec<_>>(),
            vec![site_packages]
        );
    }

    #[test]
    fn configures_latest_installed_python_version() {
        let temp = tempfile::tempdir().unwrap();
        let old_site_packages = temp.path().join("lib/python3.10/site-packages");
        let new_site_packages = temp.path().join("lib/python3.11/site-packages");
        fs::create_dir_all(&old_site_packages).unwrap();
        fs::create_dir_all(&new_site_packages).unwrap();

        let mut envs = HashMap::from([(
            FLAME_HOME_ENV.to_string(),
            temp.path().to_string_lossy().to_string(),
        )]);

        let python_version = configure_python_env(&mut envs, None);

        assert_eq!(python_version, "3.11");
        assert_eq!(envs.get(FLAME_PYTHON_VERSION_ENV).unwrap(), "3.11");
        assert_eq!(
            env::split_paths(envs.get(PYTHONPATH_ENV).unwrap()).collect::<Vec<_>>(),
            vec![new_site_packages]
        );
    }

    #[test]
    fn configures_script_runtime_python_version() {
        let temp = tempfile::tempdir().unwrap();
        let site_packages = temp.path().join("lib/python3.10/site-packages");
        fs::create_dir_all(&site_packages).unwrap();

        let mut envs = HashMap::from([
            (
                FLAME_HOME_ENV.to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                FLAME_PYTHON_VERSION_ENV.to_string(),
                "python3.12".to_string(),
            ),
        ]);

        let python_version = configure_python_env(&mut envs, Some("python3.10"));

        assert_eq!(python_version, "3.10");
        assert_eq!(envs.get(FLAME_PYTHON_VERSION_ENV).unwrap(), "3.10");
        assert_eq!(
            env::split_paths(envs.get(PYTHONPATH_ENV).unwrap()).collect::<Vec<_>>(),
            vec![site_packages]
        );
    }
}

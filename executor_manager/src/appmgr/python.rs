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
use std::fs;
use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use common::FlameError;

use super::installer::Installer;

pub struct PythonInstaller;

impl PythonInstaller {
    pub fn new() -> Self {
        Self
    }

    fn find_base_site_packages(flame_home: &Path) -> Option<String> {
        let lib_path = flame_home.join("lib");
        if !lib_path.exists() {
            return None;
        }

        for entry in fs::read_dir(&lib_path).ok()?.flatten() {
            let python_dir = entry.path();
            if python_dir.is_dir() && entry.file_name().to_string_lossy().starts_with("python") {
                let site_packages = python_dir.join("site-packages");
                if site_packages.exists() {
                    return Some(site_packages.to_string_lossy().to_string());
                }
            }
        }
        None
    }

    fn find_native_lib_paths(deps_path: &Path) -> Vec<String> {
        let mut paths = HashSet::new();

        fn scan_dir(dir: &Path, paths: &mut HashSet<String>, depth: usize) {
            if depth > 4 {
                return;
            }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_dir(&path, paths, depth + 1);
                    } else if path.extension().map(|e| e == "so").unwrap_or(false) {
                        if let Some(parent) = path.parent() {
                            paths.insert(parent.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        scan_dir(deps_path, &mut paths, 0);
        paths.into_iter().collect()
    }
}

impl Default for PythonInstaller {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Installer for PythonInstaller {
    async fn install(
        &self,
        app_name: &str,
        src_path: &Path,
        flame_home: &Path,
        app_environments: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, FlameError> {
        let app_data_path = flame_home.join("data/apps").join(app_name);
        let deps_path = app_data_path.join("deps");
        let uv_cache_path = flame_home.join("data/cache/uv");
        let pip_cache_path = flame_home.join("data/cache/pip");
        let log_path = flame_home
            .join("logs/install")
            .join(format!("{}.log", app_name));

        fs::create_dir_all(&deps_path)
            .map_err(|e| FlameError::Internal(format!("failed to create deps directory: {}", e)))?;
        fs::create_dir_all(&uv_cache_path).map_err(|e| {
            FlameError::Internal(format!("failed to create uv cache directory: {}", e))
        })?;
        fs::create_dir_all(&pip_cache_path).map_err(|e| {
            FlameError::Internal(format!("failed to create pip cache directory: {}", e))
        })?;
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                FlameError::Internal(format!("failed to create log directory: {}", e))
            })?;
        }

        let is_python_package = src_path.join("pyproject.toml").exists()
            || src_path.join("setup.py").exists()
            || src_path.join("setup.cfg").exists();

        if !is_python_package {
            return Err(FlameError::InvalidConfig(format!(
                "No Python package found in {} (missing pyproject.toml, setup.py, or setup.cfg)",
                src_path.display()
            )));
        }

        let uv_path = flame_home.join("bin/uv");
        let uv_cmd = if uv_path.exists() {
            uv_path.to_string_lossy().to_string()
        } else {
            "uv".to_string()
        };

        let log_file = fs::File::create(&log_path)
            .map_err(|e| FlameError::Internal(format!("failed to create log file: {}", e)))?;
        let log_file_err = log_file
            .try_clone()
            .map_err(|e| FlameError::Internal(format!("failed to clone log file handle: {}", e)))?;

        let status = tokio::process::Command::new(&uv_cmd)
            .arg("pip")
            .arg("install")
            .arg("--link-mode=copy")
            .arg("--target")
            .arg(&deps_path)
            .arg(".")
            .current_dir(src_path)
            .env("UV_CACHE_DIR", &uv_cache_path)
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err))
            .status()
            .await
            .map_err(|e| FlameError::Internal(format!("failed to execute uv: {}", e)))?;

        if !status.success() {
            return Err(FlameError::Internal(format!(
                "Python installation failed for app <{}>. See log: {}",
                app_name,
                log_path.display()
            )));
        }

        let mut env_vars = HashMap::new();

        let base_site_packages = Self::find_base_site_packages(flame_home);
        let mut python_paths = vec![
            deps_path.to_string_lossy().to_string(),
            src_path.to_string_lossy().to_string(),
        ];
        if let Some(base_sp) = base_site_packages {
            python_paths.push(base_sp);
        }
        if let Some(app_pythonpath) = app_environments.get("PYTHONPATH") {
            python_paths.push(app_pythonpath.clone());
        }
        env_vars.insert("PYTHONPATH".to_string(), python_paths.join(":"));

        let mut ld_paths = Self::find_native_lib_paths(&deps_path);
        if let Some(app_ld_path) = app_environments.get("LD_LIBRARY_PATH") {
            ld_paths.push(app_ld_path.clone());
        }
        if !ld_paths.is_empty() {
            env_vars.insert("LD_LIBRARY_PATH".to_string(), ld_paths.join(":"));
        }

        env_vars.insert(
            "UV_CACHE_DIR".to_string(),
            uv_cache_path.to_string_lossy().to_string(),
        );
        env_vars.insert(
            "PIP_CACHE_DIR".to_string(),
            pip_cache_path.to_string_lossy().to_string(),
        );

        tracing::info!(
            "Python installation completed for app <{}>: PYTHONPATH={}, LD_LIBRARY_PATH={}, UV_CACHE_DIR={}, PIP_CACHE_DIR={}",
            app_name,
            env_vars.get("PYTHONPATH").unwrap_or(&String::new()),
            env_vars.get("LD_LIBRARY_PATH").unwrap_or(&String::new()),
            env_vars.get("UV_CACHE_DIR").unwrap_or(&String::new()),
            env_vars.get("PIP_CACHE_DIR").unwrap_or(&String::new())
        );

        Ok(env_vars)
    }

    fn name(&self) -> &'static str {
        "python"
    }
}

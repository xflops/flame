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
use common::{get_python_runtime, FlameError, FLAME_PYTHON_VERSION_ENV};

use super::installer::Installer;

/// Whether the package at `src_path` has anything to install: dependencies, or
/// something that is built or installed for it, like scripts and extension
/// modules, or packages laid out so that they are importable only once
/// installed. A package without any is used from its source directory, which is
/// on `PYTHONPATH`, and installing it takes time on every node for each new
/// application and has no effect. Anything that is not clearly such a package is
/// treated as having dependencies.
fn has_dependencies(src_path: &Path) -> bool {
    let yes = |why: &str| {
        tracing::debug!(
            "Python package {} has something to install: {why}",
            src_path.display()
        );
        true
    };
    let Ok(text) = fs::read_to_string(src_path.join("pyproject.toml")) else {
        return yes("no readable pyproject.toml");
    };
    let Ok(pyproject) = toml::from_str::<toml::Table>(&text) else {
        return yes("pyproject.toml is not valid TOML");
    };
    // Packages under src/ are importable only once installed, and these files
    // may add dependencies or build steps of their own.
    for name in ["src", "setup.py", "setup.cfg"] {
        if src_path.join(name).exists() {
            return yes(&format!("it has {name}"));
        }
    }
    let Some(project) = pyproject.get("project").and_then(toml::Value::as_table) else {
        return yes("pyproject.toml has no [project] table");
    };
    let empty = |value: Option<&toml::Value>| match value {
        None => true,
        Some(toml::Value::Array(items)) => items.is_empty(),
        Some(_) => false,
    };
    if !empty(project.get("dependencies")) {
        return yes("it declares dependencies");
    }
    if !empty(project.get("dynamic")) {
        return yes("it has dynamic metadata, which may add dependencies");
    }
    for key in ["scripts", "gui-scripts", "entry-points"] {
        if project.contains_key(key) {
            return yes(&format!("it declares {key}"));
        }
    }
    // Only setuptools, which builds nothing without the options checked below.
    let backend = pyproject
        .get("build-system")
        .and_then(toml::Value::as_table)
        .and_then(|build| build.get("build-backend"))
        .and_then(toml::Value::as_str);
    if let Some(backend) = backend.filter(|backend| !backend.starts_with("setuptools.")) {
        return yes(&format!("it builds with {backend}"));
    }
    let tool = pyproject.get("tool").and_then(toml::Value::as_table);
    if let Some(name) = tool.and_then(|tool| {
        tool.keys()
            .find(|name| *name != "setuptools" && is_packaging_tool(name))
    }) {
        return yes(&format!("it is configured for {name}"));
    }
    let setuptools = tool
        .and_then(|tool| tool.get("setuptools"))
        .and_then(toml::Value::as_table);
    if let Some(key) = setuptools.and_then(|setuptools| {
        ["ext-modules", "package-dir", "packages", "script-files"]
            .into_iter()
            .find(|key| setuptools.contains_key(*key))
    }) {
        return yes(&format!("it sets setuptools {key}"));
    }
    tracing::debug!(
        "Python package {} has no dependencies and nothing to install",
        src_path.display()
    );
    false
}

/// Tools that configure how a package is built; other `[tool.*]` tables, such
/// as linters, do not affect installing it.
fn is_packaging_tool(name: &str) -> bool {
    matches!(
        name,
        "poetry" | "hatch" | "flit" | "pdm" | "maturin" | "scikit-build" | "setuptools_scm"
    )
}

pub struct PythonInstaller;

impl PythonInstaller {
    pub fn new() -> Self {
        Self
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
        let app_data_path = src_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| flame_home.join("data/apps").join(app_name));
        let deps_path = app_data_path.join("deps");
        let deps_bin_path = deps_path.join("bin");
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

        let requested_python_version = app_environments
            .get(FLAME_PYTHON_VERSION_ENV)
            .map(|s| s.as_str());
        let runtime = get_python_runtime(flame_home, requested_python_version);

        if has_dependencies(src_path) {
            let status = tokio::process::Command::new(&uv_cmd)
                .arg("pip")
                .arg("install")
                .arg("--link-mode=copy")
                .arg("--python")
                .arg(format!("python{}", runtime.version))
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
        }

        let mut env_vars = HashMap::new();

        let base_site_packages = runtime
            .site_packages
            .as_ref()
            .map(|site_packages| site_packages.to_string_lossy().to_string());
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
        let inherited_ld_path = std::env::var("LD_LIBRARY_PATH").ok();
        // Keep dependency/application precedence while retaining host libraries,
        // such as NVIDIA drivers mounted into the executor manager.
        for path in [
            app_environments.get("LD_LIBRARY_PATH").map(String::as_str),
            inherited_ld_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_empty() {
                ld_paths.push(path.to_string());
            }
        }
        if !ld_paths.is_empty() {
            env_vars.insert("LD_LIBRARY_PATH".to_string(), ld_paths.join(":"));
        }

        if deps_bin_path.exists() {
            let mut paths = vec![deps_bin_path.to_string_lossy().to_string()];
            if let Some(app_path) = app_environments.get("PATH") {
                paths.push(app_path.clone());
            }
            if let Ok(current_path) = std::env::var("PATH") {
                paths.push(current_path);
            }
            env_vars.insert("PATH".to_string(), paths.join(":"));
        }

        env_vars.insert(
            "UV_CACHE_DIR".to_string(),
            uv_cache_path.to_string_lossy().to_string(),
        );
        env_vars.insert(
            "PIP_CACHE_DIR".to_string(),
            pip_cache_path.to_string_lossy().to_string(),
        );
        env_vars.insert(FLAME_PYTHON_VERSION_ENV.to_string(), runtime.version);

        tracing::info!(
            "Python installation completed for app <{}>: PYTHONPATH={}, LD_LIBRARY_PATH={}, UV_CACHE_DIR={}, PIP_CACHE_DIR={}, {}={}",
            app_name,
            env_vars.get("PYTHONPATH").unwrap_or(&String::new()),
            env_vars.get("LD_LIBRARY_PATH").unwrap_or(&String::new()),
            env_vars.get("UV_CACHE_DIR").unwrap_or(&String::new()),
            env_vars.get("PIP_CACHE_DIR").unwrap_or(&String::new()),
            FLAME_PYTHON_VERSION_ENV,
            env_vars.get(FLAME_PYTHON_VERSION_ENV).unwrap_or(&String::new())
        );

        Ok(env_vars)
    }

    fn name(&self) -> &'static str {
        "python"
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{has_dependencies, Installer, PythonInstaller};

    fn package(pyproject: &str, extra: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), pyproject).unwrap();
        for path in extra {
            let path = dir.path().join(path);
            if path.extension().is_none() {
                fs::create_dir_all(path).unwrap();
            } else {
                fs::write(path, "").unwrap();
            }
        }
        dir
    }

    const BENCHMARK_PACKAGE: &str = "[build-system]\nrequires = ['setuptools>=61', 'wheel']\nbuild-backend = 'setuptools.build_meta'\n\n[project]\nname = 'bench'\nversion = '0.1.0'\nrequires-python = '>=3.9'\ndependencies = []\n\n[tool.setuptools]\npy-modules = []\n\n[tool.ruff]\nline-length = 320\n";

    #[test]
    fn a_package_without_dependencies_has_nothing_to_install() {
        assert!(!has_dependencies(package(BENCHMARK_PACKAGE, &[]).path()));
        // Without a dependencies list, with the default build backend, and with a
        // list of modules of the source directory.
        assert!(!has_dependencies(
            package("[project]\nname = 'a'\nversion = '1'\n", &[]).path()
        ));
        assert!(!has_dependencies(
            package(
                "[project]\nname = 'a'\nversion = '1'\n[tool.setuptools]\npy-modules = ['a']\n",
                &[]
            )
            .path()
        ));
        // A sibling that is not a source layout does not matter.
        assert!(!has_dependencies(
            package(BENCHMARK_PACKAGE, &["pkg", "main.py"]).path()
        ));
    }

    #[test]
    fn a_package_with_dependencies_or_a_build_is_installed() {
        let cases: &[(&str, &str)] = &[
            ("dependencies", "[project]\nname='a'\nversion='1'\ndependencies=['requests']\n"),
            ("dynamic dependencies", "[project]\nname='a'\nversion='1'\ndynamic=['dependencies']\n"),
            ("scripts", "[project]\nname='a'\nversion='1'\n[project.scripts]\na='a:main'\n"),
            ("entry points", "[project]\nname='a'\nversion='1'\n[project.entry-points.x]\na='a:main'\n"),
            ("no project table", "[tool.poetry]\nname='a'\n"),
            ("other backend", "[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n[project]\nname='a'\nversion='1'\n"),
            ("packaging tool", "[project]\nname='a'\nversion='1'\n[tool.hatch.build]\nx=1\n"),
            ("package dir", "[project]\nname='a'\nversion='1'\n[tool.setuptools]\npackage-dir={''='lib'}\n"),
            ("packages", "[project]\nname='a'\nversion='1'\n[tool.setuptools]\npackages=['a']\n"),
            ("extension modules", "[project]\nname='a'\nversion='1'\n[[tool.setuptools.ext-modules]]\nname='a'\nsources=['a.c']\n"),
            ("invalid toml", "[project\n"),
        ];
        for (name, pyproject) in cases {
            assert!(has_dependencies(package(pyproject, &[]).path()), "{name}");
        }
        for extra in ["src", "setup.py", "setup.cfg"] {
            assert!(
                has_dependencies(package(BENCHMARK_PACKAGE, &[extra]).path()),
                "{extra}"
            );
        }
        // No pyproject.toml at all.
        assert!(has_dependencies(tempfile::tempdir().unwrap().path()));
    }

    /// An installer home whose `uv` records that it ran.
    fn recording_uv(home: &std::path::Path) -> std::path::PathBuf {
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let marker = home.join("uv-ran");
        let uv = bin.join("uv");
        fs::write(
            &uv,
            format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
        marker
    }

    #[tokio::test]
    async fn install_skips_uv_for_a_package_without_dependencies() {
        let home = tempfile::tempdir().unwrap();
        let marker = recording_uv(home.path());
        let src = home.path().join("release/src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("pyproject.toml"), BENCHMARK_PACKAGE).unwrap();

        let environment = PythonInstaller::new()
            .install("bench", &src, home.path(), &HashMap::new())
            .await
            .unwrap();

        assert!(!marker.exists());
        // The source directory is still importable and the environment complete.
        let pythonpath = environment.get("PYTHONPATH").unwrap();
        assert!(pythonpath.contains(src.to_str().unwrap()));
        assert!(environment.contains_key("UV_CACHE_DIR"));
    }

    #[tokio::test]
    async fn install_runs_uv_for_a_package_with_dependencies() {
        let home = tempfile::tempdir().unwrap();
        let marker = recording_uv(home.path());
        let src = home.path().join("release/src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("pyproject.toml"),
            "[project]\nname = 'a'\nversion = '1'\ndependencies = ['requests']\n",
        )
        .unwrap();

        PythonInstaller::new()
            .install("a", &src, home.path(), &HashMap::new())
            .await
            .unwrap();

        assert!(marker.exists());
    }

    #[tokio::test]
    async fn install_preserves_inherited_library_path_after_dependency_and_application_paths() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join("release/src");
        let native_dir = home.path().join("release/deps/torch/lib");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&native_dir).unwrap();
        fs::write(
            src.join("pyproject.toml"),
            "[project]\nname = 'test-app'\nversion = '0.1.0'\n",
        )
        .unwrap();
        fs::write(native_dir.join("libtorch.so"), b"").unwrap();

        // Avoid package downloads; the fixture already provides installed libraries.
        let bin = home.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let uv = bin.join("uv");
        fs::write(&uv, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

        let inherited_path = std::env::var("LD_LIBRARY_PATH").ok();
        let installer = PythonInstaller::new();
        for application_path in [Some("/app/lib:/app/vendor/lib"), None, Some("")] {
            let mut app_environment = HashMap::new();
            let mut expected_paths = vec![native_dir.to_string_lossy().into_owned()];
            if let Some(path) = application_path {
                app_environment.insert("LD_LIBRARY_PATH".into(), path.into());
                if !path.is_empty() {
                    expected_paths.push(path.into());
                }
            }
            if let Some(path) = inherited_path.as_ref().filter(|path| !path.is_empty()) {
                expected_paths.push(path.clone());
            }
            let environment = installer
                .install("test-app", &src, home.path(), &app_environment)
                .await
                .unwrap();
            assert_eq!(
                environment.get("LD_LIBRARY_PATH"),
                Some(&expected_paths.join(":"))
            );
        }
    }
}

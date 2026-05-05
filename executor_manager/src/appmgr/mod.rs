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

mod downloader;
mod installer;
mod python;

pub use downloader::{DownloaderRegistry, PackageDownloader};
pub use installer::{Installer, InstallerType};
pub use python::PythonInstaller;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use stdng::{lock_ptr, MutexPtr};
use tar::Archive;
use tokio::sync::RwLock;

use common::apis::ApplicationContext;
use common::FlameError;

#[derive(Clone, Debug, PartialEq)]
pub enum InstallState {
    NotInstalled,
    Installing,
    Installed,
    Failed(String),
}

pub struct AppInstaller {
    pub name: String,
    pub installer_type: InstallerType,
    pub state: InstallState,
    pub install_path: PathBuf,
    pub env_vars: HashMap<String, String>,
    pub installed_at: Option<DateTime<Utc>>,
}

impl AppInstaller {
    pub fn new(name: &str, installer_type: InstallerType) -> Self {
        Self {
            name: name.to_string(),
            installer_type,
            state: InstallState::NotInstalled,
            install_path: PathBuf::new(),
            env_vars: HashMap::new(),
            installed_at: None,
        }
    }
}

pub struct ApplicationManager {
    apps: MutexPtr<HashMap<String, Arc<RwLock<AppInstaller>>>>,
    flame_home: PathBuf,
    downloader: DownloaderRegistry,
}

impl ApplicationManager {
    pub fn new() -> Result<Self, FlameError> {
        let flame_home = env::var("FLAME_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/opt/flame"));

        Ok(Self {
            apps: Arc::new(std::sync::Mutex::new(HashMap::new())),
            flame_home,
            downloader: DownloaderRegistry::new(),
        })
    }

    pub async fn install(
        &self,
        app: &ApplicationContext,
    ) -> Result<HashMap<String, String>, FlameError> {
        let installer_type = match &app.installer {
            None => {
                tracing::debug!("No installer configured for app <{}>, skipping", app.name);
                return Ok(HashMap::new());
            }
            Some(installer_str) => installer_str.parse::<InstallerType>()?,
        };

        {
            let app_entry = {
                let apps = lock_ptr!(self.apps)?;
                apps.get(&app.name).cloned()
            };
            if let Some(installed) = app_entry {
                let installed = installed.read().await;
                if installed.state == InstallState::Installed {
                    return Ok(installed.env_vars.clone());
                }
            }
        }

        let app_entry = {
            let mut apps = lock_ptr!(self.apps)?;
            apps.entry(app.name.clone())
                .or_insert_with(|| {
                    Arc::new(RwLock::new(AppInstaller::new(
                        &app.name,
                        installer_type.clone(),
                    )))
                })
                .clone()
        };

        let mut installed = app_entry.write().await;

        if installed.state == InstallState::Installed {
            return Ok(installed.env_vars.clone());
        }

        if let InstallState::Failed(msg) = &installed.state {
            return Err(FlameError::Internal(msg.clone()));
        }

        installed.state = InstallState::Installing;

        // If no URL is provided, return base env vars (for built-in apps like flmrun)
        let url = match app.url.as_ref() {
            None => {
                tracing::debug!(
                    "No URL configured for app <{}> with installer, using base env",
                    app.name
                );
                let env_vars = self.get_base_env_vars(&app.environments);
                installed.state = InstallState::Installed;
                installed.env_vars = env_vars.clone();
                installed.installed_at = Some(Utc::now());
                return Ok(env_vars);
            }
            Some(url) => url,
        };

        let package_path = self.download_package(url, &app.name).await?;

        let src_path = self
            .flame_home
            .join("data/apps")
            .join(&app.name)
            .join("src");
        self.extract_package(&package_path, &src_path)?;

        let installer = installer_type.create_installer();
        tracing::info!(
            "Running {} installer for app <{}>",
            installer.name(),
            app.name
        );

        let env_vars = match installer
            .install(&app.name, &src_path, &self.flame_home, &app.environments)
            .await
        {
            Ok(vars) => vars,
            Err(e) => {
                installed.state = InstallState::Failed(e.to_string());
                return Err(e);
            }
        };

        installed.state = InstallState::Installed;
        installed.install_path = src_path;
        installed.env_vars = env_vars.clone();
        installed.installed_at = Some(Utc::now());

        Ok(env_vars)
    }

    pub fn is_installed(&self, app_name: &str) -> bool {
        if let Ok(apps) = lock_ptr!(self.apps) {
            if let Some(installed) = apps.get(app_name) {
                if let Ok(installed) = installed.try_read() {
                    return installed.state == InstallState::Installed;
                }
            }
        }
        false
    }

    fn get_base_env_vars(
        &self,
        app_environments: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        let mut env_vars = HashMap::new();

        let lib_path = self.flame_home.join("lib");
        if let Some(site_packages) = Self::find_site_packages(&lib_path) {
            let site_packages_str = site_packages.to_string_lossy().to_string();

            let mut python_paths = vec![site_packages_str.clone()];
            if let Some(app_pythonpath) = app_environments.get("PYTHONPATH") {
                python_paths.push(app_pythonpath.clone());
            }
            env_vars.insert("PYTHONPATH".to_string(), python_paths.join(":"));

            let mut ld_paths = Self::find_native_lib_paths(&site_packages);
            if let Some(app_ld_path) = app_environments.get("LD_LIBRARY_PATH") {
                ld_paths.push(app_ld_path.clone());
            }
            if !ld_paths.is_empty() {
                env_vars.insert("LD_LIBRARY_PATH".to_string(), ld_paths.join(":"));
            }
        }

        env_vars
    }

    fn find_site_packages(lib_path: &Path) -> Option<PathBuf> {
        if !lib_path.exists() {
            return None;
        }

        for entry in fs::read_dir(lib_path).ok()?.flatten() {
            let python_dir = entry.path();
            if python_dir.is_dir() && entry.file_name().to_string_lossy().starts_with("python") {
                let site_packages = python_dir.join("site-packages");
                if site_packages.exists() {
                    return Some(site_packages);
                }
            }
        }
        None
    }

    fn find_native_lib_paths(site_packages: &Path) -> Vec<String> {
        let mut paths = std::collections::HashSet::new();

        fn scan_dir(dir: &Path, paths: &mut std::collections::HashSet<String>, depth: usize) {
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

        scan_dir(site_packages, &mut paths, 0);
        paths.into_iter().collect()
    }

    async fn download_package(&self, url: &str, app_name: &str) -> Result<PathBuf, FlameError> {
        let download_dir = self
            .flame_home
            .join("data/apps")
            .join(app_name)
            .join("download");
        fs::create_dir_all(&download_dir).map_err(|e| {
            FlameError::Internal(format!("failed to create download directory: {}", e))
        })?;

        let parsed_url = url::Url::parse(url)
            .map_err(|e| FlameError::InvalidConfig(format!("invalid url: {}", e)))?;

        let filename = parsed_url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("package.tar.gz");

        let package_path = download_dir.join(filename);

        if package_path.exists() {
            tracing::debug!("Package already downloaded: {}", package_path.display());
            return Ok(package_path);
        }

        self.downloader.download(url, &package_path).await?;

        tracing::info!("Downloaded package to: {}", package_path.display());
        Ok(package_path)
    }

    fn extract_package(
        &self,
        package_path: &PathBuf,
        dest_path: &PathBuf,
    ) -> Result<(), FlameError> {
        if dest_path.exists() {
            tracing::warn!(
                "Cleaning up stale extraction directory: {}",
                dest_path.display()
            );
            fs::remove_dir_all(dest_path).map_err(|e| {
                FlameError::Internal(format!(
                    "failed to clean up stale extraction directory: {}",
                    e
                ))
            })?;
        }

        fs::create_dir_all(dest_path).map_err(|e| {
            FlameError::Internal(format!("failed to create extraction directory: {}", e))
        })?;

        let file = fs::File::open(package_path)
            .map_err(|e| FlameError::Internal(format!("failed to open package file: {}", e)))?;

        let package_name = package_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if package_name.ends_with(".tar.gz") || package_name.ends_with(".tgz") {
            let decoder = GzDecoder::new(file);
            let mut archive = Archive::new(decoder);
            archive.unpack(dest_path).map_err(|e| {
                FlameError::Internal(format!("failed to extract tar.gz archive: {}", e))
            })?;
        } else if package_name.ends_with(".zip") {
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| FlameError::Internal(format!("failed to read zip archive: {}", e)))?;
            archive.extract(dest_path).map_err(|e| {
                FlameError::Internal(format!("failed to extract zip archive: {}", e))
            })?;
        } else {
            return Err(FlameError::InvalidConfig(format!(
                "unsupported archive format: {}",
                package_name
            )));
        }

        tracing::info!("Extracted package to: {}", dest_path.display());
        Ok(())
    }
}

impl Default for ApplicationManager {
    fn default() -> Self {
        Self::new().expect("failed to create ApplicationManager")
    }
}

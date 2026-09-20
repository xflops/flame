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

mod binary;
mod downloader;
mod installer;
mod python;

pub use binary::BinaryInstaller;
pub use downloader::{DownloaderRegistry, PackageDownloader};
pub use installer::{Installer, InstallerType};
pub use python::PythonInstaller;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use stdng::{lock_ptr, MutexPtr};
use tar::Archive;
use tokio::sync::RwLock;
use tonic::transport::ClientTlsConfig;

use common::apis::ApplicationContext;
use common::{get_python_runtime, FlameError, PythonRuntime, FLAME_PYTHON_VERSION_ENV};

#[derive(Clone, Debug, Eq)]
struct InstallKey {
    app_name: String,
    installer: String,
    url: Option<String>,
    python_version: Option<String>,
}

impl InstallKey {
    fn new(
        app_name: &str,
        installer: &InstallerType,
        url: Option<&String>,
        python_version: Option<&String>,
    ) -> Self {
        Self {
            app_name: app_name.to_string(),
            installer: installer.to_string(),
            url: url.cloned(),
            python_version: python_version.cloned(),
        }
    }

    fn release_id(&self) -> String {
        let mut hasher = Sha256::new();
        update_release_hash(&mut hasher, "app", Some(&self.app_name));
        update_release_hash(&mut hasher, "installer", Some(&self.installer));
        update_release_hash(&mut hasher, "url", self.url.as_deref());
        if let Some(version) = &self.python_version {
            update_release_hash(&mut hasher, "python_version", Some(version));
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn update_release_hash(hasher: &mut Sha256, label: &str, value: Option<&str>) {
    hasher.update(label.as_bytes());
    hasher.update([0]);
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.len().to_be_bytes());
            hasher.update(value.as_bytes());
        }
        None => {
            hasher.update([0]);
        }
    }
}

impl PartialEq for InstallKey {
    fn eq(&self, other: &Self) -> bool {
        self.app_name == other.app_name
            && self.installer == other.installer
            && self.url == other.url
            && self.python_version == other.python_version
    }
}

impl Hash for InstallKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.app_name.hash(state);
        self.installer.hash(state);
        self.url.hash(state);
        self.python_version.hash(state);
    }
}

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
    pub env_vars: HashMap<String, String>,
    pub mounts: Vec<InstallationMount>,
    pub installed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallationMountKind {
    Release,
    PythonRuntime,
    UvCache,
    PipCache,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallationMount {
    pub host_path: PathBuf,
    pub kind: InstallationMountKind,
    pub readonly: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationInstallation {
    pub env_vars: HashMap<String, String>,
    pub mounts: Vec<InstallationMount>,
}

impl AppInstaller {
    pub fn new(name: &str, installer_type: InstallerType) -> Self {
        Self {
            name: name.to_string(),
            installer_type,
            state: InstallState::NotInstalled,
            env_vars: HashMap::new(),
            mounts: Vec::new(),
            installed_at: None,
        }
    }

    fn installation(&self) -> ApplicationInstallation {
        ApplicationInstallation {
            env_vars: self.env_vars.clone(),
            mounts: self.mounts.clone(),
        }
    }
}

fn installation_mounts(
    flame_home: &Path,
    release_path: &Path,
    installer_type: &InstallerType,
    python_runtime: Option<&PythonRuntime>,
) -> Vec<InstallationMount> {
    let mut mounts = vec![InstallationMount {
        host_path: release_path.to_path_buf(),
        kind: InstallationMountKind::Release,
        readonly: true,
    }];
    if installer_type == &InstallerType::Python {
        if let Some(site_packages) =
            python_runtime.and_then(|runtime| runtime.site_packages.clone())
        {
            mounts.push(InstallationMount {
                host_path: site_packages,
                kind: InstallationMountKind::PythonRuntime,
                readonly: true,
            });
        }
        mounts.extend([
            InstallationMount {
                host_path: flame_home.join("data/cache/uv"),
                kind: InstallationMountKind::UvCache,
                readonly: false,
            },
            InstallationMount {
                host_path: flame_home.join("data/cache/pip"),
                kind: InstallationMountKind::PipCache,
                readonly: false,
            },
        ]);
    }
    mounts
}

pub struct ApplicationManager {
    apps: MutexPtr<HashMap<InstallKey, Arc<RwLock<AppInstaller>>>>,
    flame_home: PathBuf,
    downloader: DownloaderRegistry,
}

impl ApplicationManager {
    pub fn new() -> Result<Self, FlameError> {
        Self::new_with_tls(None)
    }

    pub fn new_with_tls(tls_config: Option<ClientTlsConfig>) -> Result<Self, FlameError> {
        let flame_home = env::var("FLAME_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/opt/flame"));

        Ok(Self {
            apps: Arc::new(std::sync::Mutex::new(HashMap::new())),
            flame_home,
            downloader: DownloaderRegistry::new_with_tls(tls_config),
        })
    }

    pub async fn install(
        &self,
        app: &ApplicationContext,
    ) -> Result<ApplicationInstallation, FlameError> {
        if app.url.is_none() {
            tracing::debug!(
                "No package URL configured for app <{}>, skipping installation",
                app.name
            );
            return Ok(ApplicationInstallation::default());
        }
        let installer_type = match &app.installer {
            None => {
                tracing::debug!("No installer configured for app <{}>, skipping", app.name);
                return Ok(ApplicationInstallation::default());
            }
            Some(installer_str) => installer_str.parse::<InstallerType>()?,
        };
        let python_runtime = (installer_type == InstallerType::Python).then(|| {
            get_python_runtime(
                &self.flame_home,
                app.environments
                    .get(FLAME_PYTHON_VERSION_ENV)
                    .map(|s| s.as_str()),
            )
        });
        let python_version = python_runtime
            .as_ref()
            .map(|runtime| runtime.version.clone());
        let install_key = InstallKey::new(
            &app.name,
            &installer_type,
            app.url.as_ref(),
            python_version.as_ref(),
        );

        {
            let app_entry = {
                let apps = lock_ptr!(self.apps)?;
                apps.get(&install_key).cloned()
            };
            if let Some(installed) = app_entry {
                let installed = installed.read().await;
                if installed.state == InstallState::Installed {
                    return Ok(installed.installation());
                }
            }
        }

        let app_entry = {
            let mut apps = lock_ptr!(self.apps)?;
            apps.entry(install_key.clone())
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
            return Ok(installed.installation());
        }

        if let InstallState::Failed(msg) = &installed.state {
            return Err(FlameError::Internal(msg.clone()));
        }

        installed.state = InstallState::Installing;

        let url = app.url.as_ref().expect("URL presence checked above");

        let release_path = self
            .flame_home
            .join("data/apps")
            .join(&app.name)
            .join("releases")
            .join(install_key.release_id());
        let package_path = self.download_package(url, &release_path).await?;

        let src_path = release_path.join("src");
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
        installed.env_vars = env_vars.clone();
        installed.mounts = installation_mounts(
            &self.flame_home,
            &release_path,
            &installer_type,
            python_runtime.as_ref(),
        );
        installed.installed_at = Some(Utc::now());

        Ok(installed.installation())
    }

    pub fn is_installed(&self, app_name: &str) -> bool {
        if let Ok(apps) = lock_ptr!(self.apps) {
            for (key, installed) in apps.iter() {
                if key.app_name == app_name {
                    if let Ok(installed) = installed.try_read() {
                        if installed.state == InstallState::Installed {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    async fn download_package(
        &self,
        url: &str,
        release_path: &Path,
    ) -> Result<PathBuf, FlameError> {
        let download_dir = release_path.join("download");
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

#[cfg(test)]
mod tests {
    use super::*;
    use common::apis::Shim;

    #[test]
    fn release_id_is_stable_sha256() {
        let key = InstallKey::new(
            "demo",
            &InstallerType::Binary,
            Some(&"grpc://cache/demo/pkg/demo.tar.gz".to_string()),
            None,
        );

        assert_eq!(
            key.release_id(),
            "e39adc06cdb2124e255005866affce6fb279d816c4549a6a6a8c9dc03ac4674a"
        );
    }

    #[test]
    fn release_id_distinguishes_missing_url() {
        let with_url = InstallKey::new(
            "demo",
            &InstallerType::Binary,
            Some(&"grpc://cache/demo/pkg/demo.tar.gz".to_string()),
            None,
        );
        let without_url = InstallKey::new("demo", &InstallerType::Binary, None, None);

        assert_ne!(with_url.release_id(), without_url.release_id());
        assert_eq!(without_url.release_id().len(), 64);
    }

    #[test]
    fn release_id_distinguishes_python_version() {
        let url = "grpc://cache/demo/pkg/demo.tar.gz".to_string();
        let py311 = "3.11".to_string();
        let py312 = "3.12".to_string();
        let with_py311 = InstallKey::new("demo", &InstallerType::Python, Some(&url), Some(&py311));
        let with_py312 = InstallKey::new("demo", &InstallerType::Python, Some(&url), Some(&py312));

        assert_ne!(with_py311.release_id(), with_py312.release_id());
    }

    #[test]
    fn installation_preserves_runtime_mounts() {
        let mut installed = AppInstaller::new("demo", InstallerType::Python);
        installed.env_vars = HashMap::from([(
            "PYTHONPATH".to_string(),
            "/opt/flame/data/apps/demo/releases/hash/deps".to_string(),
        )]);
        installed.mounts = vec![InstallationMount {
            host_path: PathBuf::from("/opt/flame/data/apps/demo/releases/hash"),
            kind: InstallationMountKind::Release,
            readonly: true,
        }];

        let installation = installed.installation();

        assert_eq!(installation.mounts, installed.mounts);
        assert_eq!(installation.env_vars, installed.env_vars);
    }

    #[test]
    fn python_installation_mounts_release_runtime_and_writable_caches() {
        let runtime = PythonRuntime {
            version: "3.12".to_string(),
            site_packages: Some(PathBuf::from("/opt/flame/lib/python3.12/site-packages")),
        };

        let mounts = installation_mounts(
            Path::new("/opt/flame"),
            Path::new("/opt/flame/data/apps/demo/releases/hash"),
            &InstallerType::Python,
            Some(&runtime),
        );

        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0].kind, InstallationMountKind::Release);
        assert_eq!(mounts[1].kind, InstallationMountKind::PythonRuntime);
        assert!(mounts[0].readonly);
        assert!(mounts[1].readonly);
        assert_eq!(mounts[2].kind, InstallationMountKind::UvCache);
        assert_eq!(mounts[3].kind, InstallationMountKind::PipCache);
        assert!(!mounts[2].readonly);
        assert!(!mounts[3].readonly);
    }

    #[tokio::test]
    async fn image_only_application_skips_installation() {
        let manager = ApplicationManager::new().unwrap();
        let app = ApplicationContext {
            name: "image-only".to_string(),
            shim: Shim::Cri,
            image: Some("example/image:latest".to_string()),
            command: None,
            arguments: vec![],
            working_directory: None,
            environments: HashMap::new(),
            url: None,
            installer: Some("not-a-real-installer".to_string()),
        };

        let installation = manager.install(&app).await.unwrap();
        assert!(installation.env_vars.is_empty());
        assert!(installation.mounts.is_empty());
    }

    #[tokio::test]
    async fn url_less_host_installer_is_a_no_op() {
        let manager = ApplicationManager::new().unwrap();
        let app = ApplicationContext {
            name: "flmrun".to_string(),
            shim: Shim::Host,
            image: None,
            command: None,
            arguments: vec![],
            working_directory: None,
            environments: HashMap::new(),
            url: None,
            installer: Some("python".to_string()),
        };

        let installation = manager.install(&app).await.unwrap();
        assert!(installation.env_vars.is_empty());
        assert!(installation.mounts.is_empty());
    }
}

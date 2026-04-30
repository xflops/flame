use std::path::{Path, PathBuf};

/// Installation profiles for different deployment scenarios
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallProfile {
    ControlPlane,
    Worker,
    Client,
    Cache,
}

impl InstallProfile {
    /// Get the components that should be installed for this profile
    pub fn components(&self) -> &[&str] {
        match self {
            InstallProfile::ControlPlane => &["flame-session-manager", "flmctl", "flmadm"],
            InstallProfile::Worker => &[
                "flame-executor-manager",
                "flmping-service",
                "flmexec-service",
                "flamepy",
            ],
            InstallProfile::Client => &["flmctl", "flmping", "flmexec", "flamepy"],
            InstallProfile::Cache => &["flame-object-cache"],
        }
    }

    /// Check if a component should be installed for this profile
    pub fn includes_component(&self, component: &str) -> bool {
        self.components().contains(&component)
    }
}

pub const DEFAULT_PYTHON_VERSION: &str = "3.12";

/// Configuration for the install command
#[derive(Debug, Clone)]
pub struct InstallConfig {
    pub src_dir: Option<PathBuf>,
    pub prefix: PathBuf,
    pub systemd: bool,
    pub enable: bool,
    pub skip_build: bool,
    pub clean: bool,
    pub verbose: bool,
    pub profiles: Vec<InstallProfile>,
    pub force_overwrite: bool,
    pub python_version: String,
}

impl Default for InstallConfig {
    fn default() -> Self {
        Self {
            src_dir: None,
            prefix: PathBuf::from("/usr/local/flame"),
            systemd: true,
            enable: false,
            skip_build: false,
            clean: false,
            verbose: false,
            profiles: vec![
                InstallProfile::ControlPlane,
                InstallProfile::Worker,
                InstallProfile::Client,
                InstallProfile::Cache,
            ],
            force_overwrite: false,
            python_version: DEFAULT_PYTHON_VERSION.to_string(),
        }
    }
}

/// Configuration for the uninstall command
#[derive(Debug, Clone)]
pub struct UninstallConfig {
    pub prefix: PathBuf,
    pub preserve_data: bool,
    pub preserve_config: bool,
    pub preserve_logs: bool,
    pub backup_dir: Option<PathBuf>,
    pub no_backup: bool,
    pub force: bool,
}

impl Default for UninstallConfig {
    fn default() -> Self {
        Self {
            prefix: PathBuf::from("/usr/local/flame"),
            preserve_data: false,
            preserve_config: false,
            preserve_logs: false,
            backup_dir: None,
            no_backup: false,
            force: false,
        }
    }
}

/// Standard paths for a Flame installation
#[derive(Debug, Clone)]
pub struct InstallationPaths {
    pub prefix: PathBuf,
    pub bin: PathBuf,
    pub sbin: PathBuf,
    pub lib: PathBuf,
    pub wheels: PathBuf,
    pub work: PathBuf,
    pub logs: PathBuf,
    pub conf: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub migrations: PathBuf,
}

impl InstallationPaths {
    pub fn new(prefix: PathBuf) -> Self {
        Self {
            bin: prefix.join("bin"),
            sbin: prefix.join("sbin"),
            lib: prefix.join("lib"),
            wheels: prefix.join("wheels"),
            work: prefix.join("work"),
            logs: prefix.join("logs"),
            conf: prefix.join("conf"),
            data: prefix.join("data"),
            cache: prefix.join("data/cache"),
            migrations: prefix.join("migrations"),
            prefix,
        }
    }

    /// Check if this looks like a valid Flame installation
    pub fn is_valid_installation(&self) -> bool {
        self.prefix.exists() && (self.bin.exists() || self.conf.exists() || self.data.exists())
    }
}

/// Paths to built Flame binaries
#[derive(Debug, Clone)]
pub struct BuildArtifacts {
    pub session_manager: PathBuf,
    pub executor_manager: PathBuf,
    pub object_cache: PathBuf,
    pub flmctl: PathBuf,
    pub flmadm: PathBuf,
    pub flmping: PathBuf,
    pub flmping_service: PathBuf,
    pub flmexec: PathBuf,
    pub flmexec_service: PathBuf,
}

impl BuildArtifacts {
    /// Find build artifacts in a source directory
    pub fn from_source_dir(src_dir: &Path, profile: &str) -> anyhow::Result<Self> {
        let target_dir = src_dir.join("target").join(profile);

        let artifacts = Self {
            session_manager: target_dir.join("flame-session-manager"),
            executor_manager: target_dir.join("flame-executor-manager"),
            object_cache: target_dir.join("flame-object-cache"),
            flmctl: target_dir.join("flmctl"),
            flmadm: target_dir.join("flmadm"),
            flmping: target_dir.join("flmping"),
            flmping_service: target_dir.join("flmping-service"),
            flmexec: target_dir.join("flmexec"),
            flmexec_service: target_dir.join("flmexec-service"),
        };

        // Verify all artifacts exist
        for (name, path) in [
            ("flame-session-manager", &artifacts.session_manager),
            ("flame-executor-manager", &artifacts.executor_manager),
            ("flame-object-cache", &artifacts.object_cache),
            ("flmctl", &artifacts.flmctl),
            ("flmadm", &artifacts.flmadm),
            ("flmping", &artifacts.flmping),
            ("flmping-service", &artifacts.flmping_service),
            ("flmexec", &artifacts.flmexec),
            ("flmexec-service", &artifacts.flmexec_service),
        ] {
            if !path.exists() {
                anyhow::bail!("Build artifact not found: {} at {:?}", name, path);
            }
        }

        Ok(artifacts)
    }
}

/// Exit codes for flmadm commands
pub mod exit_codes {
    pub const SUCCESS: i32 = 0;
    pub const INSTALL_FAILURE: i32 = 3;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    mod install_profile {
        use super::*;

        #[test]
        fn control_plane_components() {
            let profile = InstallProfile::ControlPlane;
            let components = profile.components();
            assert!(components.contains(&"flame-session-manager"));
            assert!(components.contains(&"flmctl"));
            assert!(components.contains(&"flmadm"));
            assert!(!components.contains(&"flamepy"));
        }

        #[test]
        fn worker_components() {
            let profile = InstallProfile::Worker;
            let components = profile.components();
            assert!(components.contains(&"flame-executor-manager"));
            assert!(components.contains(&"flmping-service"));
            assert!(components.contains(&"flmexec-service"));
            assert!(components.contains(&"flamepy"));
        }

        #[test]
        fn client_components() {
            let profile = InstallProfile::Client;
            let components = profile.components();
            assert!(components.contains(&"flmctl"));
            assert!(components.contains(&"flmping"));
            assert!(components.contains(&"flmexec"));
            assert!(components.contains(&"flamepy"));
        }

        #[test]
        fn includes_component_true() {
            assert!(InstallProfile::ControlPlane.includes_component("flmctl"));
            assert!(InstallProfile::Worker.includes_component("flamepy"));
            assert!(InstallProfile::Client.includes_component("flmexec"));
        }

        #[test]
        fn includes_component_false() {
            assert!(!InstallProfile::ControlPlane.includes_component("flamepy"));
            assert!(!InstallProfile::Worker.includes_component("flmadm"));
            assert!(!InstallProfile::Client.includes_component("flame-session-manager"));
        }
    }

    mod install_config {
        use super::*;

        #[test]
        fn default_python_version() {
            let config = InstallConfig::default();
            assert_eq!(config.python_version, "3.12");
        }

        #[test]
        fn default_prefix() {
            let config = InstallConfig::default();
            assert_eq!(config.prefix, PathBuf::from("/usr/local/flame"));
        }

        #[test]
        fn default_profiles_include_all() {
            let config = InstallConfig::default();
            assert_eq!(config.profiles.len(), 4);
            assert!(config.profiles.contains(&InstallProfile::ControlPlane));
            assert!(config.profiles.contains(&InstallProfile::Worker));
            assert!(config.profiles.contains(&InstallProfile::Client));
            assert!(config.profiles.contains(&InstallProfile::Cache));
        }

        #[test]
        fn default_flags() {
            let config = InstallConfig::default();
            assert!(config.systemd);
            assert!(!config.enable);
            assert!(!config.skip_build);
            assert!(!config.clean);
            assert!(!config.verbose);
            assert!(!config.force_overwrite);
            assert!(config.src_dir.is_none());
        }

        #[test]
        fn custom_python_version() {
            let config = InstallConfig {
                python_version: "3.11".to_string(),
                ..Default::default()
            };
            assert_eq!(config.python_version, "3.11");
        }
    }

    mod uninstall_config {
        use super::*;

        #[test]
        fn default_prefix() {
            let config = UninstallConfig::default();
            assert_eq!(config.prefix, PathBuf::from("/usr/local/flame"));
        }

        #[test]
        fn default_preserve_flags() {
            let config = UninstallConfig::default();
            assert!(!config.preserve_data);
            assert!(!config.preserve_config);
            assert!(!config.preserve_logs);
            assert!(!config.no_backup);
            assert!(!config.force);
            assert!(config.backup_dir.is_none());
        }
    }

    mod installation_paths {
        use super::*;

        #[test]
        fn new_creates_correct_paths() {
            let prefix = PathBuf::from("/opt/flame");
            let paths = InstallationPaths::new(prefix.clone());

            assert_eq!(paths.prefix, prefix);
            assert_eq!(paths.bin, prefix.join("bin"));
            assert_eq!(paths.sbin, prefix.join("sbin"));
            assert_eq!(paths.lib, prefix.join("lib"));
            assert_eq!(paths.wheels, prefix.join("wheels"));
            assert_eq!(paths.work, prefix.join("work"));
            assert_eq!(paths.logs, prefix.join("logs"));
            assert_eq!(paths.conf, prefix.join("conf"));
            assert_eq!(paths.data, prefix.join("data"));
            assert_eq!(paths.cache, prefix.join("data/cache"));
            assert_eq!(paths.migrations, prefix.join("migrations"));
        }

        #[test]
        fn is_valid_installation_nonexistent() {
            let paths = InstallationPaths::new(PathBuf::from("/nonexistent/path"));
            assert!(!paths.is_valid_installation());
        }

        #[test]
        fn is_valid_installation_with_bin() {
            let temp = tempdir().unwrap();
            let prefix = temp.path().to_path_buf();
            std::fs::create_dir_all(prefix.join("bin")).unwrap();

            let paths = InstallationPaths::new(prefix);
            assert!(paths.is_valid_installation());
        }

        #[test]
        fn is_valid_installation_with_conf() {
            let temp = tempdir().unwrap();
            let prefix = temp.path().to_path_buf();
            std::fs::create_dir_all(prefix.join("conf")).unwrap();

            let paths = InstallationPaths::new(prefix);
            assert!(paths.is_valid_installation());
        }

        #[test]
        fn is_valid_installation_with_data() {
            let temp = tempdir().unwrap();
            let prefix = temp.path().to_path_buf();
            std::fs::create_dir_all(prefix.join("data")).unwrap();

            let paths = InstallationPaths::new(prefix);
            assert!(paths.is_valid_installation());
        }

        #[test]
        fn is_valid_installation_empty_prefix() {
            let temp = tempdir().unwrap();
            let prefix = temp.path().to_path_buf();

            let paths = InstallationPaths::new(prefix);
            assert!(!paths.is_valid_installation());
        }
    }

    mod constants {
        use super::*;

        #[test]
        fn default_python_version_value() {
            assert_eq!(DEFAULT_PYTHON_VERSION, "3.12");
        }

        #[test]
        fn exit_codes() {
            assert_eq!(exit_codes::SUCCESS, 0);
            assert_eq!(exit_codes::INSTALL_FAILURE, 3);
        }
    }
}

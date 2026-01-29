use std::path::PathBuf;

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
    pub remove_user: bool,
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
            remove_user: false,
        }
    }
}

/// Standard paths for a Flame installation
#[derive(Debug, Clone)]
pub struct InstallationPaths {
    pub prefix: PathBuf,
    pub bin: PathBuf,
    pub sdk_python: PathBuf,
    pub work: PathBuf,
    pub logs: PathBuf,
    pub conf: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl InstallationPaths {
    pub fn new(prefix: PathBuf) -> Self {
        Self {
            bin: prefix.join("bin"),
            sdk_python: prefix.join("sdk/python"),
            work: prefix.join("work"),
            logs: prefix.join("logs"),
            conf: prefix.join("conf"),
            data: prefix.join("data"),
            cache: prefix.join("data/cache"),
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
    pub flmctl: PathBuf,
    pub flmadm: PathBuf,
}

impl BuildArtifacts {
    /// Find build artifacts in a source directory
    pub fn from_source_dir(src_dir: &PathBuf, profile: &str) -> anyhow::Result<Self> {
        let target_dir = src_dir.join("target").join(profile);

        let artifacts = Self {
            session_manager: target_dir.join("flame-session-manager"),
            executor_manager: target_dir.join("flame-executor-manager"),
            flmctl: target_dir.join("flmctl"),
            flmadm: target_dir.join("flmadm"),
        };

        // Verify all artifacts exist
        for (name, path) in [
            ("flame-session-manager", &artifacts.session_manager),
            ("flame-executor-manager", &artifacts.executor_manager),
            ("flmctl", &artifacts.flmctl),
            ("flmadm", &artifacts.flmadm),
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

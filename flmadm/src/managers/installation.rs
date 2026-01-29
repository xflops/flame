use crate::types::{BuildArtifacts, InstallationPaths};
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub struct InstallationManager {
    user_manager: super::user::UserManager,
}

impl InstallationManager {
    pub fn new() -> Self {
        Self {
            user_manager: super::user::UserManager::new(),
        }
    }

    /// Create all required directories
    pub fn create_directories(&self, paths: &InstallationPaths) -> Result<()> {
        println!("📁 Creating directory structure...");

        for (name, path) in [
            ("bin", &paths.bin),
            ("sdk/python", &paths.sdk_python),
            ("work", &paths.work),
            ("work/sessions", &paths.work.join("sessions")),
            ("work/executors", &paths.work.join("executors")),
            ("logs", &paths.logs),
            ("conf", &paths.conf),
            ("data", &paths.data),
            ("data/cache", &paths.cache),
            ("data/packages", &paths.data.join("packages")),
        ] {
            if !path.exists() {
                fs::create_dir_all(path)
                    .context(format!("Failed to create directory: {}", name))?;
            }
        }

        // Set permissions
        self.set_directory_permissions(paths)?;

        println!(
            "✓ Created directory structure at: {}",
            paths.prefix.display()
        );
        Ok(())
    }

    fn set_directory_permissions(&self, paths: &InstallationPaths) -> Result<()> {
        // Set restrictive permissions on data directory
        let data_perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(&paths.data, data_perms)
            .context("Failed to set data directory permissions")?;

        // Set ownership if running as root
        if self.user_manager.is_root() {
            for path in [&paths.work, &paths.logs, &paths.data] {
                self.user_manager.set_ownership(path)?;
            }
        }

        Ok(())
    }

    /// Install binaries to the target directory
    pub fn install_binaries(
        &self,
        artifacts: &BuildArtifacts,
        paths: &InstallationPaths,
    ) -> Result<()> {
        println!("📦 Installing binaries...");

        for (name, src, dst) in [
            (
                "flame-session-manager",
                &artifacts.session_manager,
                paths.bin.join("flame-session-manager"),
            ),
            (
                "flame-executor-manager",
                &artifacts.executor_manager,
                paths.bin.join("flame-executor-manager"),
            ),
            ("flmctl", &artifacts.flmctl, paths.bin.join("flmctl")),
            ("flmadm", &artifacts.flmadm, paths.bin.join("flmadm")),
        ] {
            fs::copy(src, &dst).context(format!("Failed to copy {} binary", name))?;

            // Set executable permissions
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(&dst, perms)
                .context(format!("Failed to set permissions on {}", name))?;

            println!("  ✓ Installed {}", name);
        }

        // Set ownership if running as root
        if self.user_manager.is_root() {
            self.user_manager.set_ownership(&paths.bin)?;
        }

        Ok(())
    }

    /// Install Python SDK
    pub fn install_python_sdk(&self, src_dir: &Path, paths: &InstallationPaths) -> Result<()> {
        println!("🐍 Installing Python SDK...");

        // Check if pip is available
        let pip_cmd = which::which("pip3")
            .or_else(|_| which::which("pip"))
            .context("pip not found. Please install pip3")?;

        let sdk_src = src_dir.join("sdk/python");
        if !sdk_src.exists() {
            anyhow::bail!("Python SDK source not found at: {:?}", sdk_src);
        }

        // Install using pip
        let output = Command::new(pip_cmd)
            .args([
                "install",
                "-e",
                sdk_src.to_str().unwrap(),
                "--target",
                paths.sdk_python.to_str().unwrap(),
            ])
            .output()
            .context("Failed to install Python SDK")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to install Python SDK: {}", stderr);
        }

        println!("✓ Installed Python SDK to: {}", paths.sdk_python.display());
        Ok(())
    }

    /// Remove the installation directory
    pub fn remove_installation(
        &self,
        paths: &InstallationPaths,
        preserve_data: bool,
        preserve_config: bool,
        preserve_logs: bool,
    ) -> Result<()> {
        println!("🗑️  Removing installation files...");

        // Remove binaries
        if paths.bin.exists() {
            fs::remove_dir_all(&paths.bin).context("Failed to remove bin directory")?;
            println!("  ✓ Removed binaries");
        }

        // Remove SDK
        if paths.sdk_python.parent().unwrap().exists() {
            fs::remove_dir_all(paths.sdk_python.parent().unwrap())
                .context("Failed to remove sdk directory")?;
            println!("  ✓ Removed Python SDK");
        }

        // Remove work directory
        if paths.work.exists() {
            fs::remove_dir_all(&paths.work).context("Failed to remove work directory")?;
            println!("  ✓ Removed working directory");
        }

        // Remove data directory (unless preserved)
        if !preserve_data && paths.data.exists() {
            fs::remove_dir_all(&paths.data).context("Failed to remove data directory")?;
            println!("  ✓ Removed data directory");
        } else if preserve_data {
            println!("  ⚠️  Preserved data directory");
        }

        // Remove config directory (unless preserved)
        if !preserve_config && paths.conf.exists() {
            fs::remove_dir_all(&paths.conf).context("Failed to remove conf directory")?;
            println!("  ✓ Removed configuration directory");
        } else if preserve_config {
            println!("  ⚠️  Preserved configuration directory");
        }

        // Remove logs directory (unless preserved)
        if !preserve_logs && paths.logs.exists() {
            fs::remove_dir_all(&paths.logs).context("Failed to remove logs directory")?;
            println!("  ✓ Removed logs directory");
        } else if preserve_logs {
            println!("  ⚠️  Preserved logs directory");
        }

        // Try to remove prefix if empty
        if paths.prefix.exists() {
            match fs::remove_dir(&paths.prefix) {
                Ok(_) => println!(
                    "✓ Removed installation directory: {}",
                    paths.prefix.display()
                ),
                Err(_) => println!(
                    "  ⚠️  Installation directory not empty: {}",
                    paths.prefix.display()
                ),
            }
        }

        Ok(())
    }
}

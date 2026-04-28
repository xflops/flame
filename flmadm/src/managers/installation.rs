use crate::types::{BuildArtifacts, InstallProfile, InstallationPaths};
use anyhow::{Context, Result};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub struct InstallationManager;

impl InstallationManager {
    pub fn new() -> Self {
        Self
    }

    /// Create all required directories
    pub fn create_directories(&self, paths: &InstallationPaths) -> Result<()> {
        println!("📁 Creating directory structure...");

        for (name, path) in [
            ("bin", &paths.bin),
            ("sbin", &paths.sbin),
            // Note: sdk/python is created by install_python_sdk() to allow existence check
            ("work", &paths.work),
            ("work/sessions", &paths.work.join("sessions")),
            ("work/executors", &paths.work.join("executors")),
            ("logs", &paths.logs),
            ("conf", &paths.conf),
            ("data", &paths.data),
            ("data/cache", &paths.cache),
            ("data/packages", &paths.data.join("packages")),
            ("migrations", &paths.migrations),
            ("migrations/sqlite", &paths.migrations.join("sqlite")),
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

        Ok(())
    }

    /// Install binaries to the target directory
    pub fn install_binaries(
        &self,
        artifacts: &BuildArtifacts,
        paths: &InstallationPaths,
        profiles: &[InstallProfile],
        force_overwrite: bool,
    ) -> Result<()> {
        println!("📦 Installing binaries...");

        // Check which components should be installed based on profiles
        let components_to_install = self.get_components_to_install(profiles);

        let all_binaries = [
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
            ("flmping", &artifacts.flmping, paths.bin.join("flmping")),
            (
                "flmping-service",
                &artifacts.flmping_service,
                paths.bin.join("flmping-service"),
            ),
            ("flmexec", &artifacts.flmexec, paths.bin.join("flmexec")),
            (
                "flmexec-service",
                &artifacts.flmexec_service,
                paths.bin.join("flmexec-service"),
            ),
        ];

        for (name, src, dst) in all_binaries {
            // Skip components that are not in any of the selected profiles
            if !components_to_install.iter().any(|c| c == name) {
                println!("  ⊘ Skipped {} (not in selected profiles)", name);
                continue;
            }

            // Check if the file already exists
            if dst.exists() && !force_overwrite && !self.prompt_overwrite(name)? {
                println!("  ⊘ Skipped {} (already exists)", name);
                continue;
            }

            fs::copy(src, &dst).context(format!("Failed to copy {} binary", name))?;

            // Set executable permissions
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(&dst, perms)
                .context(format!("Failed to set permissions on {}", name))?;

            println!("  ✓ Installed {}", name);
        }

        Ok(())
    }

    /// Get all components that should be installed based on the profiles
    fn get_components_to_install(&self, profiles: &[InstallProfile]) -> Vec<String> {
        let mut components = Vec::new();
        for profile in profiles {
            for component in profile.components() {
                let component_str = component.to_string();
                if !components.contains(&component_str) {
                    components.push(component_str);
                }
            }
        }
        components
    }

    /// Prompt the user whether to overwrite an existing file
    fn prompt_overwrite(&self, component: &str) -> Result<bool> {
        print!("  ⚠️  {} already exists. Overwrite? [y/N]: ", component);
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let response = input.trim().to_lowercase();
        Ok(response == "y" || response == "yes")
    }

    /// Install Python SDK using uv pip install --prefix
    pub fn install_python_sdk(
        &self,
        src_dir: &Path,
        paths: &InstallationPaths,
        profiles: &[InstallProfile],
        force_overwrite: bool,
        python_version: &str,
    ) -> Result<()> {
        let components_to_install = self.get_components_to_install(profiles);
        if !components_to_install.iter().any(|c| c == "flamepy") {
            println!("⊘ Skipped Python SDK (not in selected profiles)");
            return Ok(());
        }

        println!("🐍 Installing Python SDK...");

        let sdk_src = src_dir.join("sdk/python");
        if !sdk_src.exists() {
            anyhow::bail!("Python SDK source not found at: {:?}", sdk_src);
        }

        let lib_exists = paths.lib.exists()
            && fs::read_dir(&paths.lib)
                .map(|entries| entries.count() > 0)
                .unwrap_or(false);

        if lib_exists && !force_overwrite {
            print!(
                "  ⚠️  Python libs already exist at {}. Overwrite? [y/N]: ",
                paths.lib.display()
            );
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            let response = input.trim().to_lowercase();
            if response != "y" && response != "yes" {
                println!("  ⊘ Skipped Python SDK (already exists)");
                return Ok(());
            }
        }

        let uv_path = paths.bin.join("uv");
        if !uv_path.exists() {
            anyhow::bail!("uv not found at {}. Install uv first.", uv_path.display());
        }

        fs::create_dir_all(&paths.lib).context("Failed to create lib directory")?;

        println!("  📦 Building Python wheel...");
        fs::create_dir_all(&paths.wheels).context("Failed to create wheels directory")?;

        let build_output = std::process::Command::new(&uv_path)
            .arg("build")
            .arg("--wheel")
            .arg("--out-dir")
            .arg(&paths.wheels)
            .arg(&sdk_src)
            .output()
            .context("Failed to execute uv build")?;

        if !build_output.status.success() {
            let stderr = String::from_utf8_lossy(&build_output.stderr);
            anyhow::bail!("Failed to build wheel: {}", stderr);
        }
        println!("  ✓ Built wheel to: {}", paths.wheels.display());

        println!("  📥 Installing flamepy to lib...");

        let uv_cache_dir = paths.cache.join("uv");
        fs::create_dir_all(&uv_cache_dir).context("Failed to create cache directory")?;

        let install_output = std::process::Command::new(&uv_path)
            .arg("pip")
            .arg("install")
            .arg("--python")
            .arg(format!("python{}", python_version))
            .arg("--prefix")
            .arg(&paths.prefix)
            .arg("--find-links")
            .arg(&paths.wheels)
            .arg("flamepy")
            .env("UV_CACHE_DIR", &uv_cache_dir)
            .output()
            .context("Failed to execute uv pip install")?;

        if !install_output.status.success() {
            let stderr = String::from_utf8_lossy(&install_output.stderr);
            anyhow::bail!("Failed to install flamepy: {}", stderr);
        }

        println!("  ✓ Installed flamepy to: {}", paths.lib.display());

        Ok(())
    }

    /// Generate flmenv.sh script for environment setup
    pub fn generate_env_script(
        &self,
        paths: &InstallationPaths,
        python_version: &str,
    ) -> Result<()> {
        println!("📜 Generating environment script...");

        let env_script_path = paths.sbin.join("flmenv.sh");

        let site_packages = paths
            .lib
            .join(format!("python{}", python_version))
            .join("site-packages");

        let script_content = format!(
            r#"#!/bin/bash
# Flame Environment Setup Script
# Generated by flmadm install
# Source this file to set up your Flame environment:
#   source {prefix}/sbin/flmenv.sh

# Flame installation prefix
export FLAME_HOME="{prefix}"

# Add Flame binaries to PATH
if [[ ":$PATH:" != *":{prefix}/bin:"* ]]; then
    export PATH="{prefix}/bin:$PATH"
fi

# UV and pip cache directories (shared across containers)
export UV_CACHE_DIR="$FLAME_HOME/data/cache/uv"
export PIP_CACHE_DIR="$FLAME_HOME/data/cache/pip"
export UV_LINK_MODE=copy

# Create cache directories if they don't exist
mkdir -p "$UV_CACHE_DIR" 2>/dev/null
mkdir -p "$PIP_CACHE_DIR" 2>/dev/null

# Python environment for flamepy
FLAME_SITE_PACKAGES="{site_packages}"
FLAME_LD_DIRS=""
if [ -d "$FLAME_SITE_PACKAGES" ]; then
    if [[ ":$PYTHONPATH:" != *":$FLAME_SITE_PACKAGES:"* ]]; then
        export PYTHONPATH="$FLAME_SITE_PACKAGES:$PYTHONPATH"
    fi
    
    # Find all directories containing shared libraries for native extensions
    while IFS= read -r dir; do
        abs_dir=$(cd "$dir" 2>/dev/null && pwd)
        if [ -n "$abs_dir" ] && [[ ":$LD_LIBRARY_PATH:" != *":$abs_dir:"* ]]; then
            export LD_LIBRARY_PATH="$abs_dir:$LD_LIBRARY_PATH"
            FLAME_LD_DIRS="$FLAME_LD_DIRS $abs_dir"
        fi
    done < <(find "$FLAME_SITE_PACKAGES" \( -name "*.so" -o -name "*.dylib" \) -type f 2>/dev/null | xargs -n1 dirname | sort -u)
fi

# Print environment info (only when sourced interactively)
if [[ $- == *i* ]]; then
    echo "Flame environment loaded:"
    echo "  FLAME_HOME=$FLAME_HOME"
    echo "  PATH includes: {prefix}/bin"
    echo "  UV_CACHE_DIR=$UV_CACHE_DIR"
    echo "  PIP_CACHE_DIR=$PIP_CACHE_DIR"
    if [ -d "$FLAME_SITE_PACKAGES" ]; then
        echo "  PYTHONPATH includes: $FLAME_SITE_PACKAGES"
    fi
    if [ -n "$FLAME_LD_DIRS" ]; then
        echo "  LD_LIBRARY_PATH includes: $FLAME_LD_DIRS"
    fi
fi
"#,
            prefix = paths.prefix.display(),
            site_packages = site_packages.display(),
        );

        fs::write(&env_script_path, script_content).context("Failed to write flmenv.sh")?;

        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&env_script_path, perms)
            .context("Failed to set flmenv.sh permissions")?;

        println!("  ✓ Generated: {}", env_script_path.display());
        println!(
            "    To activate: source {}/sbin/flmenv.sh",
            paths.prefix.display()
        );

        Ok(())
    }

    /// Install database migrations
    pub fn install_migrations(
        &self,
        src_dir: &Path,
        paths: &InstallationPaths,
        profiles: &[InstallProfile],
    ) -> Result<()> {
        // Migrations are only needed for control plane
        if !profiles.contains(&InstallProfile::ControlPlane) {
            println!("⊘ Skipped database migrations (not in selected profiles)");
            return Ok(());
        }

        println!("🗄️  Installing database migrations...");

        let migrations_src = src_dir.join("session_manager/migrations/sqlite");
        if !migrations_src.exists() {
            anyhow::bail!("Migrations source not found at: {:?}", migrations_src);
        }

        // Copy all migration files
        for entry in fs::read_dir(&migrations_src).context("Failed to read migrations directory")? {
            let entry = entry.context("Failed to read migration file entry")?;
            let file_name = entry.file_name();
            let src_path = entry.path();
            let dst_path = paths.migrations.join("sqlite").join(&file_name);

            if src_path.is_file() {
                fs::copy(&src_path, &dst_path)
                    .context(format!("Failed to copy migration: {:?}", file_name))?;
            }
        }

        println!("✓ Installed migrations to: {}", paths.migrations.display());
        Ok(())
    }

    /// Install uv tool
    pub fn install_uv(&self, paths: &InstallationPaths, profiles: &[InstallProfile]) -> Result<()> {
        // UV is only needed for worker and client profiles
        let needs_uv = profiles.contains(&InstallProfile::Worker)
            || profiles.contains(&InstallProfile::Client);

        if !needs_uv {
            println!("⊘ Skipped uv installation (not in selected profiles)");
            return Ok(());
        }

        println!("🔧 Installing uv...");

        // Find uv in the system
        let uv_src = self.find_uv_executable().context(
            "uv not found in system. Please install uv first:\n\
             1. curl -LsSf https://astral.sh/uv/install.sh | sh\n\
             2. Or install via your package manager",
        )?;

        let uv_dst = paths.bin.join("uv");

        // Copy uv to installation directory
        fs::copy(&uv_src, &uv_dst).context("Failed to copy uv binary")?;

        // Set executable permissions
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&uv_dst, perms).context("Failed to set permissions on uv")?;

        println!("  ✓ Installed uv from {}", uv_src.display());
        Ok(())
    }

    /// Find uv executable in the system
    fn find_uv_executable(&self) -> Result<std::path::PathBuf> {
        use std::process::Command;

        // Try to find uv using 'which' command
        if let Ok(output) = Command::new("which").arg("uv").output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                let path = path_str.trim();
                if !path.is_empty() {
                    return Ok(std::path::PathBuf::from(path));
                }
            }
        }

        // Fallback: check common locations
        for common_path in [
            "/usr/bin/uv",
            "/usr/local/bin/uv",
            "/opt/homebrew/bin/uv", // macOS Homebrew
        ] {
            let path = std::path::Path::new(common_path);
            if path.exists() {
                return Ok(path.to_path_buf());
            }
        }

        // Try to find in $HOME/.local/bin (common user install location)
        if let Ok(home) = std::env::var("HOME") {
            let user_uv = std::path::PathBuf::from(home).join(".local/bin/uv");
            if user_uv.exists() {
                return Ok(user_uv);
            }
        }

        anyhow::bail!("uv executable not found in system")
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

        if paths.bin.exists() {
            fs::remove_dir_all(&paths.bin).context("Failed to remove bin directory")?;
            println!("  ✓ Removed binaries");
        }

        if paths.lib.exists() {
            fs::remove_dir_all(&paths.lib).context("Failed to remove lib directory")?;
            println!("  ✓ Removed Python libs");
        }

        if paths.wheels.exists() {
            fs::remove_dir_all(&paths.wheels).context("Failed to remove wheels directory")?;
            println!("  ✓ Removed wheels");
        }

        if paths.migrations.exists() {
            fs::remove_dir_all(&paths.migrations)
                .context("Failed to remove migrations directory")?;
            println!("  ✓ Removed migrations");
        }

        if paths.work.exists() {
            fs::remove_dir_all(&paths.work).context("Failed to remove work directory")?;
            println!("  ✓ Removed working directory");
        }

        // Remove events directory (session-manager creates this in prefix)
        let events_dir = paths.prefix.join("events");
        if events_dir.exists() {
            fs::remove_dir_all(&events_dir).context("Failed to remove events directory")?;
            println!("  ✓ Removed events directory");
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    mod get_components_to_install {
        use super::*;

        #[test]
        fn single_profile_control_plane() {
            let manager = InstallationManager::new();
            let profiles = vec![InstallProfile::ControlPlane];
            let components = manager.get_components_to_install(&profiles);

            assert!(components.contains(&"flame-session-manager".to_string()));
            assert!(components.contains(&"flmctl".to_string()));
            assert!(components.contains(&"flmadm".to_string()));
            assert!(!components.contains(&"flamepy".to_string()));
        }

        #[test]
        fn single_profile_worker() {
            let manager = InstallationManager::new();
            let profiles = vec![InstallProfile::Worker];
            let components = manager.get_components_to_install(&profiles);

            assert!(components.contains(&"flame-executor-manager".to_string()));
            assert!(components.contains(&"flamepy".to_string()));
            assert!(!components.contains(&"flame-session-manager".to_string()));
        }

        #[test]
        fn single_profile_client() {
            let manager = InstallationManager::new();
            let profiles = vec![InstallProfile::Client];
            let components = manager.get_components_to_install(&profiles);

            assert!(components.contains(&"flmctl".to_string()));
            assert!(components.contains(&"flmping".to_string()));
            assert!(components.contains(&"flamepy".to_string()));
        }

        #[test]
        fn multiple_profiles_no_duplicates() {
            let manager = InstallationManager::new();
            let profiles = vec![InstallProfile::Worker, InstallProfile::Client];
            let components = manager.get_components_to_install(&profiles);

            let flamepy_count = components.iter().filter(|c| *c == "flamepy").count();
            assert_eq!(flamepy_count, 1);
        }

        #[test]
        fn all_profiles() {
            let manager = InstallationManager::new();
            let profiles = vec![
                InstallProfile::ControlPlane,
                InstallProfile::Worker,
                InstallProfile::Client,
            ];
            let components = manager.get_components_to_install(&profiles);

            assert!(components.contains(&"flame-session-manager".to_string()));
            assert!(components.contains(&"flame-executor-manager".to_string()));
            assert!(components.contains(&"flmctl".to_string()));
            assert!(components.contains(&"flamepy".to_string()));
        }

        #[test]
        fn empty_profiles() {
            let manager = InstallationManager::new();
            let profiles: Vec<InstallProfile> = vec![];
            let components = manager.get_components_to_install(&profiles);

            assert!(components.is_empty());
        }
    }

    mod create_directories {
        use super::*;

        #[test]
        fn creates_all_directories() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            let manager = InstallationManager::new();

            manager.create_directories(&paths).unwrap();

            assert!(paths.bin.exists());
            assert!(paths.work.exists());
            assert!(paths.work.join("sessions").exists());
            assert!(paths.work.join("executors").exists());
            assert!(paths.logs.exists());
            assert!(paths.conf.exists());
            assert!(paths.data.exists());
            assert!(paths.cache.exists());
            assert!(paths.migrations.exists());
            assert!(paths.migrations.join("sqlite").exists());
        }

        #[test]
        fn sets_data_directory_permissions() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            let manager = InstallationManager::new();

            manager.create_directories(&paths).unwrap();

            let metadata = fs::metadata(&paths.data).unwrap();
            let mode = metadata.permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        #[test]
        fn idempotent_creation() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            let manager = InstallationManager::new();

            manager.create_directories(&paths).unwrap();
            manager.create_directories(&paths).unwrap();

            assert!(paths.bin.exists());
        }
    }

    mod remove_installation {
        use super::*;

        #[test]
        fn removes_bin_directory() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            fs::create_dir_all(&paths.bin).unwrap();
            fs::write(paths.bin.join("test"), "test").unwrap();

            let manager = InstallationManager::new();
            manager
                .remove_installation(&paths, false, false, false)
                .unwrap();

            assert!(!paths.bin.exists());
        }

        #[test]
        fn preserves_data_when_requested() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            fs::create_dir_all(&paths.data).unwrap();
            fs::write(paths.data.join("test"), "test").unwrap();

            let manager = InstallationManager::new();
            manager
                .remove_installation(&paths, true, false, false)
                .unwrap();

            assert!(paths.data.exists());
        }

        #[test]
        fn preserves_config_when_requested() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            fs::create_dir_all(&paths.conf).unwrap();
            fs::write(paths.conf.join("test.yaml"), "test").unwrap();

            let manager = InstallationManager::new();
            manager
                .remove_installation(&paths, false, true, false)
                .unwrap();

            assert!(paths.conf.exists());
        }

        #[test]
        fn preserves_logs_when_requested() {
            let temp = tempdir().unwrap();
            let paths = InstallationPaths::new(temp.path().to_path_buf());
            fs::create_dir_all(&paths.logs).unwrap();
            fs::write(paths.logs.join("test.log"), "test").unwrap();

            let manager = InstallationManager::new();
            manager
                .remove_installation(&paths, false, false, true)
                .unwrap();

            assert!(paths.logs.exists());
        }
    }
}

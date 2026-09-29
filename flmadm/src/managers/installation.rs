use crate::types::{available_examples, BuildArtifacts, InstallProfile, InstallationPaths};
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
            ("examples", &paths.examples),
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

    /// Install example binaries under FLAME_HOME/examples when requested.
    pub fn install_examples(
        &self,
        src_dir: &Path,
        paths: &InstallationPaths,
        with_examples: bool,
        force_overwrite: bool,
    ) -> Result<()> {
        if !with_examples {
            println!("⊘ Skipped examples (--with-examples not specified)");
            return Ok(());
        }

        println!("🧪 Installing examples...");
        let target_dir = src_dir.join("target").join("release");

        for example in available_examples() {
            let dst_dir = paths.examples.join(example.relative_dir);
            fs::create_dir_all(&dst_dir).context(format!(
                "Failed to create example directory: {}",
                dst_dir.display()
            ))?;

            println!("  • {}", example.name);
            for binary in example.binaries {
                let src = target_dir.join(binary);
                if !src.exists() {
                    anyhow::bail!(
                        "Example binary not found: {}. Build package '{}' first or run without --skip-build.",
                        src.display(),
                        example.package
                    );
                }

                let dst = dst_dir.join(binary);
                if dst.exists() && !force_overwrite && !self.prompt_overwrite(binary)? {
                    println!("    ⊘ Skipped {} (already exists)", binary);
                    continue;
                }

                fs::copy(&src, &dst)
                    .context(format!("Failed to copy example binary {}", binary))?;

                let perms = fs::Permissions::from_mode(0o755);
                fs::set_permissions(&dst, perms)
                    .context(format!("Failed to set permissions on {}", binary))?;

                println!(
                    "    ✓ Installed {} to {}",
                    binary,
                    dst.strip_prefix(&paths.prefix).unwrap_or(&dst).display()
                );
            }

            let src_example_dir = src_dir.join("examples").join(example.relative_dir);
            for asset in example.assets {
                let src = src_example_dir.join(asset);
                if !src.exists() {
                    anyhow::bail!(
                        "Example asset not found: {} for example '{}'.",
                        src.display(),
                        example.name
                    );
                }

                let dst = dst_dir.join(asset);
                if dst.exists() && !force_overwrite && !self.prompt_overwrite(asset)? {
                    println!("    ⊘ Skipped {} (already exists)", asset);
                    continue;
                }

                fs::copy(&src, &dst).context(format!("Failed to copy example asset {}", asset))?;
                let mode = if asset.ends_with(".sh") { 0o755 } else { 0o644 };
                let perms = fs::Permissions::from_mode(mode);
                fs::set_permissions(&dst, perms)
                    .context(format!("Failed to set permissions on {}", asset))?;

                println!(
                    "    ✓ Installed {} to {}",
                    asset,
                    dst.strip_prefix(&paths.prefix).unwrap_or(&dst).display()
                );
            }
        }

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
            (
                "flame-object-cache",
                &artifacts.object_cache,
                paths.bin.join("flame-object-cache"),
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
    /// Returns the list of Python versions that were successfully installed
    pub fn install_python_sdk(
        &self,
        src_dir: &Path,
        paths: &InstallationPaths,
        profiles: &[InstallProfile],
        force_overwrite: bool,
        python_versions: &[String],
    ) -> Result<Vec<String>> {
        let components_to_install = self.get_components_to_install(profiles);
        if !components_to_install.iter().any(|c| c == "flamepy") {
            println!("⊘ Skipped Python SDK (not in selected profiles)");
            return Ok(Vec::new());
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
                return Ok(Vec::new());
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

        let uv_cache_dir = paths.cache.join("uv");
        fs::create_dir_all(&uv_cache_dir).context("Failed to create cache directory")?;

        let mut installed_versions = Vec::new();
        for python_version in python_versions {
            println!("  📥 Installing flamepy for Python {}...", python_version);

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
                if stderr.contains("No interpreter found")
                    || stderr.contains("not found")
                    || stderr.contains("Cannot find")
                {
                    println!("  ⚠ Python {} not available, skipping", python_version);
                    continue;
                }
                anyhow::bail!(
                    "Failed to install flamepy for Python {}: {}",
                    python_version,
                    stderr
                );
            }

            installed_versions.push(python_version.clone());
            println!(
                "  ✓ Installed flamepy for Python {} to: {}",
                python_version,
                paths.lib.display()
            );
        }

        if installed_versions.is_empty() {
            anyhow::bail!(
                "Failed to install flamepy: no Python interpreters found for versions {:?}",
                python_versions
            );
        }

        Ok(installed_versions)
    }

    /// Generate flmenv.sh script for environment setup
    pub fn generate_env_script(
        &self,
        paths: &InstallationPaths,
        python_versions: &[String],
    ) -> Result<()> {
        println!("📜 Generating environment script...");

        let env_script_path = paths.sbin.join("flmenv.sh");

        let available_versions = python_versions
            .iter()
            .map(|v| format!("\"{}\"", v))
            .collect::<Vec<_>>()
            .join(" ");

        let script_content = format!(
            r#"#!/bin/bash
# Flame Environment Setup Script
# Generated by flmadm install
#
# Usage:
#   source {prefix}/sbin/flmenv.sh [OPTIONS]
#
# Options:
#   --python-version VERSION  Python version to use (e.g., 3.11, 3.12)
#                             If not specified, uses the latest installed version
#
# Examples:
#   source {prefix}/sbin/flmenv.sh                       # Use latest installed Python version
#   source {prefix}/sbin/flmenv.sh --python-version 3.11 # Use Python 3.11
#   source {prefix}/sbin/flmenv.sh --python-version 3.12 # Use Python 3.12
#
# Available Python versions: {available_versions}

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
mkdir -p "$UV_CACHE_DIR" 2>/dev/null || true
mkdir -p "$PIP_CACHE_DIR" 2>/dev/null || true

# Parse arguments
FLAME_PYTHON_VERSION=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --python-version)
            FLAME_PYTHON_VERSION="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

# Find latest installed Python version if not specified
if [ -z "$FLAME_PYTHON_VERSION" ]; then
    FLAME_PYTHON_VERSION=$(ls -1d "$FLAME_HOME"/lib/python*/site-packages 2>/dev/null \
        | sed 's|.*/python\([0-9.]*\)/site-packages|\1|' \
        | sort -V \
        | tail -1)
fi
export FLAME_PYTHON_VERSION

# Python environment for flamepy
FLAME_LD_DIRS=""
FLAME_MATCHED_SITE_PACKAGES=""
if [ -n "$FLAME_PYTHON_VERSION" ]; then
    FLAME_SITE_PACKAGES="{prefix}/lib/python$FLAME_PYTHON_VERSION/site-packages"
    if [ -d "$FLAME_SITE_PACKAGES" ]; then
        if [[ ":$PYTHONPATH:" != *":$FLAME_SITE_PACKAGES:"* ]]; then
            export PYTHONPATH="$FLAME_SITE_PACKAGES:$PYTHONPATH"
            FLAME_MATCHED_SITE_PACKAGES="$FLAME_SITE_PACKAGES"
        fi

        # Find all directories containing shared libraries for native extensions
        while IFS= read -r dir; do
            [ -z "$dir" ] && continue
            abs_dir=$(cd "$dir" 2>/dev/null && pwd)
            if [ -n "$abs_dir" ] && [[ ":$LD_LIBRARY_PATH:" != *":$abs_dir:"* ]]; then
                export LD_LIBRARY_PATH="$abs_dir:$LD_LIBRARY_PATH"
                FLAME_LD_DIRS="$FLAME_LD_DIRS $abs_dir"
            fi
        done < <(find "$FLAME_SITE_PACKAGES" \( -name "*.so" -o -name "*.dylib" \) -type f 2>/dev/null | xargs -r -n1 dirname 2>/dev/null | sort -u)
    fi
fi

# Print environment info (only when sourced interactively)
if [[ $- == *i* ]]; then
    echo "Flame environment loaded:"
    echo "  FLAME_HOME=$FLAME_HOME"
    echo "  PATH includes: {prefix}/bin"
    echo "  UV_CACHE_DIR=$UV_CACHE_DIR"
    echo "  PIP_CACHE_DIR=$PIP_CACHE_DIR"
    if [ -n "$FLAME_MATCHED_SITE_PACKAGES" ]; then
        echo "  PYTHONPATH includes: $FLAME_MATCHED_SITE_PACKAGES (Python $FLAME_PYTHON_VERSION)"
    elif [ -n "$FLAME_PYTHON_VERSION" ]; then
        echo "  Warning: No flamepy installation found for Python $FLAME_PYTHON_VERSION"
        echo "  Available versions: {available_versions}"
    fi
    if [ -n "$FLAME_LD_DIRS" ]; then
        echo "  LD_LIBRARY_PATH includes: $FLAME_LD_DIRS"
    fi
fi
"#,
            prefix = paths.prefix.display(),
            available_versions = available_versions,
        );

        fs::write(&env_script_path, script_content).context("Failed to write flmenv.sh")?;

        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&env_script_path, perms)
            .context("Failed to set flmenv.sh permissions")?;

        println!("  ✓ Generated: {}", env_script_path.display());
        println!(
            "    To activate: source {}/sbin/flmenv.sh [--python-version VERSION]",
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

        let migrations_src = src_dir.join("common/migrations/sqlite");
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

        if paths.examples.exists() {
            fs::remove_dir_all(&paths.examples).context("Failed to remove examples directory")?;
            println!("  ✓ Removed examples");
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
            assert!(paths.examples.exists());
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

    mod install_examples {
        use super::*;

        fn create_example_fixtures(src_dir: &Path) {
            let target_dir = src_dir.join("target/release");
            fs::create_dir_all(&target_dir).unwrap();
            fs::write(target_dir.join("pi"), "client").unwrap();
            fs::write(target_dir.join("pi-service"), "service").unwrap();
            fs::write(target_dir.join("candle-based-example"), "client").unwrap();
            fs::write(target_dir.join("candle-based-example-service"), "service").unwrap();

            let pi_dir = src_dir.join("examples/pi/rust");
            fs::create_dir_all(&pi_dir).unwrap();
            fs::write(pi_dir.join("README.md"), "readme").unwrap();

            let python_pi_dir = src_dir.join("examples/pi/python");
            fs::create_dir_all(&python_pi_dir).unwrap();
            fs::write(python_pi_dir.join(".python-version"), "3.12").unwrap();
            fs::write(python_pi_dir.join("README.md"), "readme").unwrap();
            fs::write(python_pi_dir.join("main.py"), "print('pi')").unwrap();
            fs::write(
                python_pi_dir.join("pyproject.toml"),
                "[project]\nname = \"pi\"\n",
            )
            .unwrap();

            let example_dir = src_dir.join("examples/candle/based");
            fs::create_dir_all(&example_dir).unwrap();
            fs::write(
                example_dir.join("candle-based-example.yaml"),
                "metadata: {}\n",
            )
            .unwrap();
            fs::write(example_dir.join("README.md"), "readme").unwrap();
        }

        #[test]
        fn copies_all_example_binaries_to_examples_dir() {
            let temp = tempdir().unwrap();
            let src_dir = temp.path().join("src");
            create_example_fixtures(&src_dir);

            let paths = InstallationPaths::new(temp.path().join("flame"));
            let manager = InstallationManager::new();
            manager.create_directories(&paths).unwrap();
            manager
                .install_examples(&src_dir, &paths, true, true)
                .unwrap();

            assert!(paths.examples.join("pi/rust/pi").exists());
            assert!(paths.examples.join("pi/rust/pi-service").exists());
            assert!(paths.examples.join("pi/rust/README.md").exists());
            assert!(!paths.examples.join("pi/rust/deploy.sh").exists());
            assert!(paths.examples.join("pi/python/.python-version").exists());
            assert!(paths.examples.join("pi/python/README.md").exists());
            assert!(paths.examples.join("pi/python/main.py").exists());
            assert!(paths.examples.join("pi/python/pyproject.toml").exists());
            assert!(!paths.examples.join("pi/python/pi").exists());

            assert!(paths
                .examples
                .join("candle/based/candle-based-example")
                .exists());
            assert!(paths
                .examples
                .join("candle/based/candle-based-example-service")
                .exists());
            assert!(paths
                .examples
                .join("candle/based/candle-based-example.yaml")
                .exists());
            assert!(paths.examples.join("candle/based/README.md").exists());
            assert!(!paths.bin.join("candle-based-example").exists());
            assert!(!paths.bin.join("pi").exists());
        }

        #[test]
        fn reports_missing_example_binary() {
            let temp = tempdir().unwrap();
            let src_dir = temp.path().join("src");
            fs::create_dir_all(src_dir.join("target/release")).unwrap();
            let paths = InstallationPaths::new(temp.path().join("flame"));
            let manager = InstallationManager::new();
            manager.create_directories(&paths).unwrap();

            let error = manager
                .install_examples(&src_dir, &paths, true, true)
                .unwrap_err();

            assert!(error.to_string().contains("Example binary not found"));
        }

        #[test]
        fn skips_examples_when_flag_is_false() {
            let temp = tempdir().unwrap();
            let src_dir = temp.path().join("src");
            create_example_fixtures(&src_dir);

            let paths = InstallationPaths::new(temp.path().join("flame"));
            let manager = InstallationManager::new();
            manager.create_directories(&paths).unwrap();
            manager
                .install_examples(&src_dir, &paths, false, true)
                .unwrap();

            assert!(!paths.examples.join("pi/rust/pi").exists());
            assert!(!paths
                .examples
                .join("candle/based/candle-based-example")
                .exists());
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
            fs::create_dir_all(&paths.examples).unwrap();
            fs::write(paths.examples.join("example"), "test").unwrap();

            let manager = InstallationManager::new();
            manager
                .remove_installation(&paths, false, false, false)
                .unwrap();

            assert!(!paths.bin.exists());
            assert!(!paths.examples.exists());
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

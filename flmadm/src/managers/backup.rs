use anyhow::{Context, Result};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct BackupManager;

impl BackupManager {
    pub fn new() -> Self {
        Self
    }

    /// Create a backup of the installation
    pub fn create_backup(
        &self,
        paths: &crate::types::InstallationPaths,
        custom_backup_dir: Option<PathBuf>,
        preserve_data: bool,
        preserve_config: bool,
        preserve_logs: bool,
    ) -> Result<PathBuf> {
        // Determine backup directory
        let backup_dir = match custom_backup_dir {
            Some(dir) => dir,
            None => {
                let timestamp = Local::now().format("%Y%m%d_%H%M%S");
                PathBuf::from(format!("{}.backup.{}", paths.prefix.display(), timestamp))
            }
        };

        println!("💾 Creating backup at: {}", backup_dir.display());

        // Create backup directory
        fs::create_dir_all(&backup_dir).context("Failed to create backup directory")?;

        // Backup data (unless preserved)
        if !preserve_data && paths.data.exists() {
            let backup_data = backup_dir.join("data");
            self.copy_directory(&paths.data, &backup_data)
                .context("Failed to backup data directory")?;
            println!("  ✓ Backed up data");
        }

        // Backup config (unless preserved)
        if !preserve_config && paths.conf.exists() {
            let backup_conf = backup_dir.join("conf");
            self.copy_directory(&paths.conf, &backup_conf)
                .context("Failed to backup config directory")?;
            println!("  ✓ Backed up configuration");
        }

        // Backup logs (unless preserved)
        if !preserve_logs && paths.logs.exists() {
            let backup_logs = backup_dir.join("logs");
            self.copy_directory(&paths.logs, &backup_logs)
                .context("Failed to backup logs directory")?;
            println!("  ✓ Backed up logs");
        }

        println!("✓ Backup created at: {}", backup_dir.display());
        Ok(backup_dir)
    }

    /// Backup existing installation before clean install
    pub fn backup_for_clean_install(
        &self,
        paths: &crate::types::InstallationPaths,
    ) -> Result<PathBuf> {
        if !paths.prefix.exists() {
            anyhow::bail!(
                "Installation directory does not exist: {}",
                paths.prefix.display()
            );
        }

        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let backup_dir = PathBuf::from(format!("{}.backup.{}", paths.prefix.display(), timestamp));

        println!("💾 Backing up existing installation...");

        fs::create_dir_all(&backup_dir).context("Failed to create backup directory")?;

        // Backup all important directories
        for (name, src) in [
            ("conf", &paths.conf),
            ("data", &paths.data),
            ("logs", &paths.logs),
        ] {
            if src.exists() {
                let dst = backup_dir.join(name);
                self.copy_directory(src, &dst)
                    .context(format!("Failed to backup {}", name))?;
                println!("  ✓ Backed up {}", name);
            }
        }

        println!("✓ Backup created at: {}", backup_dir.display());
        Ok(backup_dir)
    }

    /// Copy a directory recursively
    fn copy_directory(&self, src: &Path, dst: &Path) -> Result<()> {
        fs::create_dir_all(dst)?;

        for entry in WalkDir::new(src) {
            let entry = entry?;
            let path = entry.path();

            let relative_path = path.strip_prefix(src)?;
            let target_path = dst.join(relative_path);

            if entry.file_type().is_dir() {
                fs::create_dir_all(&target_path)?;
            } else {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(path, &target_path)?;
            }
        }

        Ok(())
    }
}

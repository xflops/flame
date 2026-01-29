use crate::managers::{
    backup::BackupManager, installation::InstallationManager, systemd::SystemdManager,
    user::UserManager,
};
use crate::types::{InstallationPaths, UninstallConfig};
use anyhow::Result;
use dialoguer::Confirm;

pub fn run(config: UninstallConfig) -> Result<()> {
    println!("🗑️  Flame Uninstallation");
    println!("   Target: {}", config.prefix.display());
    println!();

    // Phase 1: Validation
    println!("═══ Phase 1: Validation ═══");
    let paths = validate_and_confirm(&config)?;

    // Phase 2: Stop Services
    println!("\n═══ Phase 2: Stop Services ═══");
    stop_services()?;

    // Phase 3: Create Backup
    let backup_dir = if !config.no_backup {
        println!("\n═══ Phase 3: Create Backup ═══");
        Some(create_backup(&paths, &config)?)
    } else {
        println!("\n═══ Phase 3: Skipping Backup (--no-backup) ═══");
        None
    };

    // Phase 4: Remove Installation
    println!("\n═══ Phase 4: Remove Installation ═══");
    remove_installation(&paths, &config)?;

    // Phase 5: Remove User (if requested)
    if config.remove_user {
        println!("\n═══ Phase 5: Remove User ═══");
        remove_user()?;
    }

    // Phase 6: Summary
    println!("\n═══ Uninstallation Complete ═══");
    print_summary(&paths, &config, backup_dir);

    Ok(())
}

fn validate_and_confirm(config: &UninstallConfig) -> Result<InstallationPaths> {
    // Check if prefix is absolute
    if !config.prefix.is_absolute() {
        anyhow::bail!("Installation prefix must be an absolute path");
    }

    let paths = InstallationPaths::new(config.prefix.clone());

    // Check if installation exists
    if !paths.is_valid_installation() {
        anyhow::bail!(
            "Flame installation not found at: {}\n  Directory does not exist or is not a valid Flame installation.",
            paths.prefix.display()
        );
    }

    // Safety check: prevent removing critical system paths
    let prefix_str = paths.prefix.to_string_lossy().to_lowercase();
    for dangerous_path in [
        "/", "/usr", "/etc", "/home", "/var", "/bin", "/sbin", "/lib",
    ] {
        if prefix_str == dangerous_path {
            anyhow::bail!(
                "Cannot uninstall from system path: {}\n  This safety check prevents accidental system damage.",
                paths.prefix.display()
            );
        }
    }

    // Require "flame" in the path as additional safety
    if !prefix_str.contains("flame") {
        anyhow::bail!(
            "Installation path must contain 'flame' in its name: {}\n  This safety check prevents accidental removal of non-Flame directories.",
            paths.prefix.display()
        );
    }

    println!("✓ Found valid Flame installation");

    // List what will be removed
    println!("\nThe following will be removed:");
    if paths.bin.exists() {
        println!("  • Binaries: {}", paths.bin.display());
    }
    if paths.sdk_python.parent().unwrap().exists() {
        println!(
            "  • Python SDK: {}",
            paths.sdk_python.parent().unwrap().display()
        );
    }
    if paths.work.exists() {
        println!("  • Working directory: {}", paths.work.display());
    }
    if !config.preserve_data && paths.data.exists() {
        println!("  • Data directory: {}", paths.data.display());
    }
    if !config.preserve_config && paths.conf.exists() {
        println!("  • Configuration: {}", paths.conf.display());
    }
    if !config.preserve_logs && paths.logs.exists() {
        println!("  • Logs: {}", paths.logs.display());
    }

    if config.preserve_data || config.preserve_config || config.preserve_logs {
        println!("\nThe following will be preserved:");
        if config.preserve_data {
            println!("  • Data directory");
        }
        if config.preserve_config {
            println!("  • Configuration");
        }
        if config.preserve_logs {
            println!("  • Logs");
        }
    }

    if !config.no_backup {
        let backup_location = config.backup_dir.clone().unwrap_or_else(|| {
            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
            std::path::PathBuf::from(format!("{}.backup.{}", paths.prefix.display(), timestamp))
        });
        println!("\nBackup will be created at: {}", backup_location.display());
    } else {
        println!("\n⚠️  WARNING: No backup will be created (--no-backup)");
        println!("   All data will be permanently deleted!");
    }

    // Confirm with user (unless --force)
    if !config.force {
        println!();
        let confirmed = if config.no_backup {
            // Extra confirmation for no-backup
            println!("⚠️  This is a DESTRUCTIVE operation with NO BACKUP!");
            Confirm::new()
                .with_prompt("Type 'yes' to permanently delete all Flame data")
                .default(false)
                .interact()?
        } else {
            Confirm::new()
                .with_prompt("Proceed with uninstallation?")
                .default(false)
                .interact()?
        };

        if !confirmed {
            println!("Uninstallation cancelled.");
            std::process::exit(0);
        }
    }

    Ok(paths)
}

fn stop_services() -> Result<()> {
    let systemd_manager = SystemdManager::new();

    // Check if systemd services exist
    let fsm_service = std::path::Path::new("/etc/systemd/system/flame-session-manager.service");
    let fem_service = std::path::Path::new("/etc/systemd/system/flame-executor-manager.service");

    if fsm_service.exists() || fem_service.exists() {
        println!("🛑 Stopping Flame services...");
        systemd_manager.remove_services()?;
    } else {
        println!("ℹ️  No systemd services found");
    }

    Ok(())
}

fn create_backup(
    paths: &InstallationPaths,
    config: &UninstallConfig,
) -> Result<std::path::PathBuf> {
    let backup_manager = BackupManager::new();

    backup_manager.create_backup(
        paths,
        config.backup_dir.clone(),
        config.preserve_data,
        config.preserve_config,
        config.preserve_logs,
    )
}

fn remove_installation(paths: &InstallationPaths, config: &UninstallConfig) -> Result<()> {
    let installation_manager = InstallationManager::new();

    installation_manager.remove_installation(
        paths,
        config.preserve_data,
        config.preserve_config,
        config.preserve_logs,
    )
}

fn remove_user() -> Result<()> {
    let user_manager = UserManager::new();
    user_manager.remove_user(false)?;
    Ok(())
}

fn print_summary(
    paths: &InstallationPaths,
    config: &UninstallConfig,
    backup_dir: Option<std::path::PathBuf>,
) {
    println!("\n✅ Flame has been successfully uninstalled!");
    println!();
    println!("Removed Components:");
    println!("  • Binaries");
    println!("  • Python SDK");
    println!("  • Working directories");

    if !config.preserve_systemd_services() {
        println!("  • Systemd service files");
    }

    if !config.preserve_data {
        println!("  • Data directory");
    }
    if !config.preserve_config {
        println!("  • Configuration files");
    }
    if !config.preserve_logs {
        println!("  • Log files");
    }

    if let Some(backup_path) = backup_dir {
        println!();
        println!("Backup Location:");
        println!("  {}", backup_path.display());
        println!();
        println!("To restore from backup:");
        println!(
            "  sudo cp -r {}/* {}/",
            backup_path.display(),
            paths.prefix.display()
        );
    }

    if config.preserve_data || config.preserve_config || config.preserve_logs {
        println!();
        println!("Preserved Files:");
        if config.preserve_data {
            println!("  • Data: {}", paths.data.display());
        }
        if config.preserve_config {
            println!("  • Config: {}", paths.conf.display());
        }
        if config.preserve_logs {
            println!("  • Logs: {}", paths.logs.display());
        }
    }

    println!();
}

// Helper method for UninstallConfig
impl UninstallConfig {
    fn preserve_systemd_services(&self) -> bool {
        // Systemd services are always removed during uninstall
        false
    }
}

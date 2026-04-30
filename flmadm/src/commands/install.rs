use crate::managers::{
    backup::BackupManager, build::BuildManager, config::ConfigGenerator,
    installation::InstallationManager, source::SourceManager, systemd::SystemdManager,
    user::UserManager,
};
use crate::types::{InstallConfig, InstallationPaths};
use anyhow::Result;

pub fn run(config: InstallConfig) -> Result<()> {
    println!("🚀 Flame Installation");
    println!("   Target: {}", config.prefix.display());
    println!("   Profiles:");
    for profile in &config.profiles {
        let profile_name = match profile {
            crate::types::InstallProfile::ControlPlane => "Control Plane",
            crate::types::InstallProfile::Worker => "Worker",
            crate::types::InstallProfile::Cache => "Cache",
            crate::types::InstallProfile::Client => "Client",
        };
        println!("     • {}", profile_name);
    }
    println!();

    // Phase 1: Validation
    println!("═══ Phase 1: Validation ═══");
    validate_config(&config)?;

    // Phase 2: Preparation
    println!("\n═══ Phase 2: Preparation ═══");
    let paths = InstallationPaths::new(config.prefix.clone());

    // Handle clean install
    if config.clean && paths.is_valid_installation() {
        handle_clean_install(&paths)?;
    }

    let mut source_manager = SourceManager::new();
    let src_dir = source_manager.prepare_source(config.src_dir.clone())?;

    // Phase 3: Build (skip if requested)
    if !config.skip_build {
        println!("\n═══ Phase 3: Build ═══");
        let build_manager = BuildManager::new(config.verbose);
        build_manager.check_prerequisites()?;
        let artifacts = build_manager.build_all(&src_dir)?;

        // Phase 4: Installation
        println!("\n═══ Phase 4: Installation ═══");
        install_components(&artifacts, &src_dir, &paths, &config)?;
    } else {
        println!("\n═══ Phase 3: Skipping Build (--skip-build) ═══");

        // Phase 4: Installation
        println!("\n═══ Phase 4: Installation ═══");
        let artifacts = crate::types::BuildArtifacts::from_source_dir(&src_dir, "release")?;
        install_components(&artifacts, &src_dir, &paths, &config)?;
    }

    // Phase 5: Systemd Setup (if requested and needed)
    let has_control_plane = config
        .profiles
        .contains(&crate::types::InstallProfile::ControlPlane);
    let has_worker = config
        .profiles
        .contains(&crate::types::InstallProfile::Worker);
    let has_cache = config
        .profiles
        .contains(&crate::types::InstallProfile::Cache);
    let needs_systemd = has_control_plane || has_worker || has_cache;

    if config.systemd && needs_systemd {
        println!("\n═══ Phase 5: Systemd Setup ═══");
        setup_systemd(&paths, &config)?;
    } else if !config.systemd {
        println!("\n═══ Phase 5: Skipping Systemd (--no-systemd) ═══");
    } else {
        println!("\n═══ Phase 5: Skipping Systemd (no services to install) ═══");
    }

    // Phase 6: Summary
    println!("\n═══ Installation Complete ═══");
    print_summary(&paths, &config);

    Ok(())
}

fn validate_config(config: &InstallConfig) -> Result<()> {
    // Check if prefix is absolute
    if !config.prefix.is_absolute() {
        anyhow::bail!("Installation prefix must be an absolute path");
    }

    // Check if profiles require systemd services
    let has_control_plane = config
        .profiles
        .contains(&crate::types::InstallProfile::ControlPlane);
    let has_worker = config
        .profiles
        .contains(&crate::types::InstallProfile::Worker);
    let has_client_only = config.profiles.len() == 1
        && config
            .profiles
            .contains(&crate::types::InstallProfile::Client);

    // Check if client-only installation is combined with systemd flags
    if has_client_only && config.systemd {
        println!(
            "ℹ️  Note: Client profile doesn't install services. Ignoring systemd configuration."
        );
    }

    if has_client_only && config.enable {
        anyhow::bail!(
            "Cannot use --enable with --client profile only.\n  \
             The client profile doesn't install any services.\n  \
             Use --control-plane and/or --worker to install services."
        );
    }

    // Check if we need root privileges for systemd
    let user_manager = UserManager::new();
    if config.systemd && (has_control_plane || has_worker) && !user_manager.is_root() {
        anyhow::bail!(
            "Root privileges required for system-wide installation with systemd.\n  Run with sudo or use --no-systemd for user-local installation."
        );
    }

    // Check conflicting options
    if config.enable && !config.systemd {
        anyhow::bail!("Cannot use --enable without systemd (--no-systemd conflicts with --enable)");
    }

    // Check if uv is available (required by worker and client profiles)
    let needs_uv = has_worker
        || config
            .profiles
            .contains(&crate::types::InstallProfile::Client);
    if needs_uv {
        match find_uv_executable() {
            Some(uv_path) => {
                println!("✓ Found uv at: {}", uv_path.display());
            }
            None => {
                anyhow::bail!(
                    "uv is not found in PATH (required by worker and client profiles)\n\
                     Please install uv using one of these methods:\n\
                     1. curl -LsSf https://astral.sh/uv/install.sh | sh\n\
                     2. Or install uv via your package manager"
                );
            }
        }
    }

    println!("✓ Configuration validated");
    Ok(())
}

/// Find uv executable in the system PATH
fn find_uv_executable() -> Option<std::path::PathBuf> {
    use std::process::Command;

    // Try to find uv using 'which' command
    if let Ok(output) = Command::new("which").arg("uv").output() {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout);
            let path = path_str.trim();
            if !path.is_empty() {
                return Some(std::path::PathBuf::from(path));
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
            return Some(path.to_path_buf());
        }
    }

    // Try to find in $HOME/.local/bin (common user install location)
    if let Ok(home) = std::env::var("HOME") {
        let user_uv = std::path::PathBuf::from(home).join(".local/bin/uv");
        if user_uv.exists() {
            return Some(user_uv);
        }
    }

    None
}

fn handle_clean_install(paths: &InstallationPaths) -> Result<()> {
    println!("🧹 Clean installation requested");

    // Backup existing installation
    let backup_manager = BackupManager::new();
    let backup_dir = backup_manager.backup_for_clean_install(paths)?;

    println!("   Backup location: {}", backup_dir.display());

    // Stop services if they're running
    let systemd_manager = SystemdManager::new();
    let _ = systemd_manager.remove_services();

    // Remove existing installation
    let installation_manager = InstallationManager::new();
    installation_manager.remove_installation(paths, false, false, false)?;

    println!("✓ Cleaned existing installation");
    Ok(())
}

fn install_components(
    artifacts: &crate::types::BuildArtifacts,
    src_dir: &std::path::Path,
    paths: &InstallationPaths,
    config: &InstallConfig,
) -> Result<()> {
    // Create directories
    let installation_manager = InstallationManager::new();
    installation_manager.create_directories(paths)?;

    // Install binaries
    installation_manager.install_binaries(
        artifacts,
        paths,
        &config.profiles,
        config.force_overwrite,
    )?;

    // Install uv (for worker and client profiles)
    installation_manager.install_uv(paths, &config.profiles)?;

    // Install Python SDK
    installation_manager.install_python_sdk(
        src_dir,
        paths,
        &config.profiles,
        config.force_overwrite,
        &config.python_version,
    )?;

    // Generate environment script
    installation_manager.generate_env_script(paths, &config.python_version)?;

    // Install database migrations
    installation_manager.install_migrations(src_dir, paths, &config.profiles)?;

    // Generate configuration
    let config_generator = ConfigGenerator::new();
    config_generator.generate_config(&paths.prefix)?;

    Ok(())
}

fn setup_systemd(paths: &InstallationPaths, config: &InstallConfig) -> Result<()> {
    let systemd_manager = SystemdManager::new();

    // Install service files
    systemd_manager.install_services(&paths.prefix, &config.profiles)?;

    // Enable and start services if requested
    if config.enable {
        systemd_manager.enable_and_start_services(&config.profiles)?;
    } else {
        let has_control_plane = config
            .profiles
            .contains(&crate::types::InstallProfile::ControlPlane);
        let has_worker = config
            .profiles
            .contains(&crate::types::InstallProfile::Worker);

        println!("ℹ️  Services installed but not enabled. To start services:");
        if has_control_plane && has_worker {
            println!("   sudo systemctl enable --now flame-session-manager flame-executor-manager");
        } else if has_control_plane {
            println!("   sudo systemctl enable --now flame-session-manager");
        } else if has_worker {
            println!("   sudo systemctl enable --now flame-executor-manager");
        }
    }

    Ok(())
}

fn print_summary(paths: &InstallationPaths, config: &InstallConfig) {
    println!("\n✅ Flame has been successfully installed!");
    println!();
    println!("Installation Details:");
    println!("  • Installation prefix: {}", paths.prefix.display());
    println!("  • Installation profiles:");

    // Show which profiles were installed
    for profile in &config.profiles {
        let profile_name = match profile {
            crate::types::InstallProfile::ControlPlane => "Control Plane",
            crate::types::InstallProfile::Worker => "Worker",
            crate::types::InstallProfile::Cache => "Cache",
            crate::types::InstallProfile::Client => "Client",
        };
        println!(
            "    - {}: {}",
            profile_name,
            profile.components().join(", ")
        );
    }

    println!();
    println!("  • Binaries: {}", paths.bin.display());
    println!(
        "  • Configuration: {}",
        paths.conf.join("flame-cluster.yaml").display()
    );

    let has_flamepy = config
        .profiles
        .iter()
        .any(|p| p.includes_component("flamepy"));
    if has_flamepy {
        println!("  • Python libs: {}", paths.lib.display());
    }
    println!();

    let has_control_plane = config
        .profiles
        .contains(&crate::types::InstallProfile::ControlPlane);
    let has_worker = config
        .profiles
        .contains(&crate::types::InstallProfile::Worker);
    let has_cache = config
        .profiles
        .contains(&crate::types::InstallProfile::Cache);

    if config.systemd && (has_control_plane || has_worker || has_cache) {
        println!("Systemd Services:");
        if config.enable {
            if has_control_plane {
                println!("  • flame-session-manager: enabled and running");
            }
            if has_cache {
                println!("  • flame-object-cache: enabled and running");
            }
            if has_worker {
                println!("  • flame-executor-manager: enabled and running");
            }
            println!();
            println!("To check service status:");
            if has_control_plane {
                println!("  sudo systemctl status flame-session-manager");
            }
            if has_cache {
                println!("  sudo systemctl status flame-object-cache");
            }
            if has_worker {
                println!("  sudo systemctl status flame-executor-manager");
            }
        } else {
            if has_control_plane {
                println!("  • flame-session-manager: installed (not enabled)");
            }
            if has_cache {
                println!("  • flame-object-cache: installed (not enabled)");
            }
            if has_worker {
                println!("  • flame-executor-manager: installed (not enabled)");
            }
            println!();
            println!("To start services:");
            if has_control_plane {
                println!("  sudo systemctl enable --now flame-session-manager");
            }
            if has_cache {
                println!("  sudo systemctl enable --now flame-object-cache");
            }
            if has_worker {
                println!("  sudo systemctl enable --now flame-executor-manager");
            }
        }
        println!();
        println!("To view logs:");
        if has_control_plane {
            println!("  sudo journalctl -u flame-session-manager -f");
            println!("  tail -f {}/logs/fsm.log", paths.prefix.display());
        }
        if has_cache {
            println!("  sudo journalctl -u flame-object-cache -f");
            println!("  tail -f {}/logs/foc.log", paths.prefix.display());
        }
        if has_worker {
            println!("  sudo journalctl -u flame-executor-manager -f");
            println!("  tail -f {}/logs/fem.log", paths.prefix.display());
        }
    } else if !config.systemd && (has_control_plane || has_worker || has_cache) {
        println!("Manual Service Management:");
        if has_control_plane {
            println!("  • Start session manager: {}/bin/flame-session-manager --config {}/conf/flame-cluster.yaml", 
                     paths.prefix.display(), paths.prefix.display());
        }
        if has_cache {
            println!("  • Start object cache: {}/bin/flame-object-cache --config {}/conf/flame-cluster.yaml", 
                     paths.prefix.display(), paths.prefix.display());
        }
        if has_worker {
            println!("  • Start executor manager: {}/bin/flame-executor-manager --config {}/conf/flame-cluster.yaml", 
                     paths.prefix.display(), paths.prefix.display());
        }
    }

    println!();
    println!("Next Steps:");
    println!(
        "  1. Review configuration: {}/conf/flame-cluster.yaml",
        paths.prefix.display()
    );
    println!("  2. Add {} to your PATH", paths.bin.display());

    // Provide relevant test command based on what was installed
    if has_control_plane {
        println!(
            "  3. Test the installation: {}/flmctl --version",
            paths.bin.display()
        );
    } else if config
        .profiles
        .contains(&crate::types::InstallProfile::Client)
    {
        println!(
            "  3. Test the installation: {}/flmping --version",
            paths.bin.display()
        );
    }
    println!();
}

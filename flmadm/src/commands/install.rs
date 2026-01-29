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

    // Phase 5: Systemd Setup (if requested)
    if config.systemd {
        println!("\n═══ Phase 5: Systemd Setup ═══");
        setup_systemd(&paths, &config)?;
    } else {
        println!("\n═══ Phase 5: Skipping Systemd (--no-systemd) ═══");
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

    // Check if we need root privileges
    let user_manager = UserManager::new();
    if config.systemd && !user_manager.is_root() {
        anyhow::bail!(
            "Root privileges required for system-wide installation with systemd.\n  Run with sudo or use --no-systemd for user-local installation."
        );
    }

    // Check conflicting options
    if config.enable && !config.systemd {
        anyhow::bail!("Cannot use --enable without systemd (--no-systemd conflicts with --enable)");
    }

    println!("✓ Configuration validated");
    Ok(())
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

    // Create flame user (for system-wide installation)
    let user_manager = UserManager::new();
    if config.systemd && user_manager.is_root() {
        user_manager.create_user()?;
    }

    // Install binaries
    installation_manager.install_binaries(artifacts, paths)?;

    // Install Python SDK
    installation_manager.install_python_sdk(src_dir, paths)?;

    // Generate configuration
    let config_generator = ConfigGenerator::new();
    config_generator.generate_config(&paths.prefix)?;

    Ok(())
}

fn setup_systemd(paths: &InstallationPaths, config: &InstallConfig) -> Result<()> {
    let systemd_manager = SystemdManager::new();

    // Install service files
    systemd_manager.install_services(&paths.prefix)?;

    // Enable and start services if requested
    if config.enable {
        systemd_manager.enable_and_start_services()?;
    } else {
        println!("ℹ️  Services installed but not enabled. To start services:");
        println!("   sudo systemctl enable --now flame-session-manager flame-executor-manager");
    }

    Ok(())
}

fn print_summary(paths: &InstallationPaths, config: &InstallConfig) {
    println!("\n✅ Flame has been successfully installed!");
    println!();
    println!("Installation Details:");
    println!("  • Installation prefix: {}", paths.prefix.display());
    println!("  • Binaries: {}", paths.bin.display());
    println!(
        "  • Configuration: {}",
        paths.conf.join("flame-cluster.yaml").display()
    );
    println!("  • Python SDK: {}", paths.sdk_python.display());
    println!();

    if config.systemd {
        println!("Systemd Services:");
        if config.enable {
            println!("  • flame-session-manager: enabled and running");
            println!("  • flame-executor-manager: enabled and running");
            println!();
            println!("To check service status:");
            println!("  sudo systemctl status flame-session-manager");
            println!("  sudo systemctl status flame-executor-manager");
        } else {
            println!("  • flame-session-manager: installed (not enabled)");
            println!("  • flame-executor-manager: installed (not enabled)");
            println!();
            println!("To start services:");
            println!("  sudo systemctl enable --now flame-session-manager");
            println!("  sudo systemctl enable --now flame-executor-manager");
        }
        println!();
        println!("To view logs:");
        println!("  sudo journalctl -u flame-session-manager -f");
        println!("  tail -f {}/logs/fsm.log", paths.prefix.display());
    } else {
        println!("Manual Service Management:");
        println!("  • Start session manager: {}/bin/flame-session-manager --config {}/conf/flame-cluster.yaml", 
                 paths.prefix.display(), paths.prefix.display());
        println!("  • Start executor manager: {}/bin/flame-executor-manager --config {}/conf/flame-cluster.yaml", 
                 paths.prefix.display(), paths.prefix.display());
    }

    println!();
    println!("Next Steps:");
    println!(
        "  1. Review configuration: {}/conf/flame-cluster.yaml",
        paths.prefix.display()
    );
    println!("  2. Add {}/bin to your PATH", paths.bin.display());
    println!(
        "  3. Test the installation: {}/bin/flmctl --version",
        paths.bin.display()
    );
    println!();
}

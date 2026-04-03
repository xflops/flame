use crate::managers::{
    backup::BackupManager, build::BuildManager, cert::generate_mtls_certs, config::ConfigGenerator,
    installation::InstallationManager, source::SourceManager, systemd::SystemdManager,
    user::UserManager,
};
use crate::types::{InstallConfig, InstallationPaths};
use anyhow::Result;
use std::fs;
use std::os::unix::fs::PermissionsExt;

const DEFAULT_CONFIG: &str = r#"---
cluster:
  name: flame
  endpoint: "http://127.0.0.1:8080"
  storage: "sqlite://flame.db"
  executors:
    shim: host
"#;

pub fn run(config: InstallConfig) -> Result<()> {
    println!("🚀 Flame Installation");
    println!("   Target: {}", config.prefix.display());
    println!("   Profiles:");
    for profile in &config.profiles {
        let profile_name = match profile {
            crate::types::InstallProfile::ControlPlane => "Control Plane",
            crate::types::InstallProfile::Worker => "Worker",
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

    // Phase 5: mTLS Setup (if requested)
    if config.with_mtls {
        println!("\n═══ Phase 5: mTLS Certificate Generation ═══");
        setup_mtls(&paths)?;
    }

    // Phase 6: Systemd Setup (if requested and needed)
    let has_control_plane = config
        .profiles
        .contains(&crate::types::InstallProfile::ControlPlane);
    let has_worker = config
        .profiles
        .contains(&crate::types::InstallProfile::Worker);
    let needs_systemd = has_control_plane || has_worker;

    if config.systemd && needs_systemd {
        let phase = if config.with_mtls { 6 } else { 5 };
        println!("\n═══ Phase {}: Systemd Setup ═══", phase);
        setup_systemd(&paths, &config)?;
    } else if !config.systemd {
        let phase = if config.with_mtls { 6 } else { 5 };
        println!("\n═══ Phase {}: Skipping Systemd (--no-systemd) ═══", phase);
    } else {
        let phase = if config.with_mtls { 6 } else { 5 };
        println!(
            "\n═══ Phase {}: Skipping Systemd (no services to install) ═══",
            phase
        );
    }

    // Final Phase: Summary
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
    )?;

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

fn setup_mtls(paths: &InstallationPaths) -> Result<()> {
    let tls_dir = paths.conf.join("tls");
    fs::create_dir_all(&tls_dir)?;
    println!("  Creating TLS directory: {}", tls_dir.display());

    println!("  Generating mTLS certificates...");
    let certs = generate_mtls_certs()?;

    let ca_cert_path = tls_dir.join("ca.crt");
    let ca_key_path = tls_dir.join("ca.key");
    fs::write(&ca_cert_path, &certs.ca.cert_pem)?;
    fs::write(&ca_key_path, &certs.ca.key_pem)?;
    fs::set_permissions(&ca_key_path, fs::Permissions::from_mode(0o600))?;
    println!("  ✓ CA certificate: {}", ca_cert_path.display());

    let server_cert_path = tls_dir.join("server.crt");
    let server_key_path = tls_dir.join("server.key");
    fs::write(&server_cert_path, &certs.server.cert_pem)?;
    fs::write(&server_key_path, &certs.server.key_pem)?;
    fs::set_permissions(&server_key_path, fs::Permissions::from_mode(0o600))?;
    println!("  ✓ Server certificate: {}", server_cert_path.display());

    let users_dir = tls_dir.join("users");
    fs::create_dir_all(&users_dir)?;

    let root_cert_path = users_dir.join("root.crt");
    let root_key_path = users_dir.join("root.key");
    fs::write(&root_cert_path, &certs.root_user.cert_pem)?;
    fs::write(&root_key_path, &certs.root_user.key_pem)?;
    fs::set_permissions(&root_key_path, fs::Permissions::from_mode(0o600))?;
    println!("  ✓ Root user certificate: {}", root_cert_path.display());

    let executor_cert_path = users_dir.join("flame-executor.crt");
    let executor_key_path = users_dir.join("flame-executor.key");
    fs::write(&executor_cert_path, &certs.flame_executor.cert_pem)?;
    fs::write(&executor_key_path, &certs.flame_executor.key_pem)?;
    fs::set_permissions(&executor_key_path, fs::Permissions::from_mode(0o600))?;
    println!("  ✓ Executor certificate: {}", executor_cert_path.display());

    println!("  Bootstrapping RBAC (root user and role)...");
    bootstrap_rbac(paths)?;

    println!("  Updating configuration with TLS settings...");
    update_config_with_tls(paths)?;

    println!("✓ mTLS setup complete");
    println!();
    println!("  Client certificates for authentication:");
    println!("    • Root user: {}", root_cert_path.display());
    println!("    • Executor:  {}", executor_cert_path.display());
    println!();
    println!("  To use mTLS with flmctl:");
    println!("    export FLAME_CA_FILE={}", ca_cert_path.display());
    println!("    export FLAME_CERT_FILE={}", root_cert_path.display());
    println!("    export FLAME_KEY_FILE={}", root_key_path.display());

    Ok(())
}

fn bootstrap_rbac(paths: &InstallationPaths) -> Result<()> {
    use std::io::Write;

    fs::create_dir_all(&paths.data)?;

    let db_path = paths.data.join("flame.db");

    let bootstrap_sql = r#"
INSERT OR IGNORE INTO roles (name, description, permissions, workspaces, creation_time)
VALUES (
    'root',
    'Root administrator role with full access to all workspaces and resources',
    '["*:*"]',
    '["*"]',
    strftime('%s', 'now')
);

INSERT OR IGNORE INTO roles (name, description, permissions, workspaces, creation_time)
VALUES (
    'flame-executor',
    'Internal role for flame executor manager',
    '["session:*", "task:*", "node:*", "executor:*"]',
    '["*"]',
    strftime('%s', 'now')
);

INSERT OR IGNORE INTO users (name, display_name, email, certificate_cn, enabled, creation_time)
VALUES (
    'root',
    'Root Administrator',
    NULL,
    'root',
    1,
    strftime('%s', 'now')
);

INSERT OR IGNORE INTO users (name, display_name, email, certificate_cn, enabled, creation_time)
VALUES (
    'flame-executor',
    'Flame Executor Manager',
    NULL,
    'flame-executor',
    1,
    strftime('%s', 'now')
);

INSERT OR IGNORE INTO user_roles (user_name, role_name)
VALUES ('root', 'root');

INSERT OR IGNORE INTO user_roles (user_name, role_name)
VALUES ('flame-executor', 'flame-executor');

INSERT OR IGNORE INTO workspaces (name, description, labels, creation_time)
VALUES (
    'default',
    'Default workspace',
    '{}',
    strftime('%s', 'now')
);
"#;

    let bootstrap_path = paths.data.join("bootstrap-rbac.sql");
    let mut file = fs::File::create(&bootstrap_path)?;
    file.write_all(bootstrap_sql.as_bytes())?;

    println!(
        "  ✓ Created RBAC bootstrap SQL: {}",
        bootstrap_path.display()
    );
    println!("    Note: The bootstrap SQL will be executed when the session manager starts.");
    println!("    Database location: {}", db_path.display());

    Ok(())
}

fn update_config_with_tls(paths: &InstallationPaths) -> Result<()> {
    let config_path = paths.conf.join("flame-cluster.yaml");
    let tls_dir = paths.conf.join("tls");

    let config_content = if config_path.exists() {
        fs::read_to_string(&config_path)?
    } else {
        DEFAULT_CONFIG.to_string()
    };

    if config_content.contains("tls:") {
        println!("  ⚠️  TLS configuration already exists, skipping update");
        return Ok(());
    }

    let mut new_config = config_content.clone();

    new_config = new_config.replace(
        "endpoint: \"http://127.0.0.1:8080\"",
        "endpoint: \"https://127.0.0.1:8080\"",
    );
    new_config = new_config.replace(
        "endpoint: http://127.0.0.1:8080",
        "endpoint: https://127.0.0.1:8080",
    );

    let tls_config = format!(
        r#"  tls:
    cert_file: "{}/server.crt"
    key_file: "{}/server.key"
    ca_file: "{}/ca.crt"
    ca_key_file: "{}/ca.key"
    cert_validity: "24h"
"#,
        tls_dir.display(),
        tls_dir.display(),
        tls_dir.display(),
        tls_dir.display()
    );

    if let Some(pos) = new_config.find("  executors:") {
        new_config.insert_str(pos, &tls_config);
    } else if let Some(pos) = new_config.find("storage:") {
        if let Some(end_pos) = new_config[pos..].find('\n') {
            let insert_pos = pos + end_pos + 1;
            new_config.insert_str(insert_pos, &tls_config);
        }
    } else {
        new_config.push_str(&tls_config);
    }

    fs::write(&config_path, &new_config)?;
    println!("  ✓ Updated configuration: {}", config_path.display());

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

    // Only show SDK path if it was installed
    let has_flamepy = config
        .profiles
        .iter()
        .any(|p| p.includes_component("flamepy"));
    if has_flamepy {
        println!("  • Python SDK: {}", paths.sdk_python.display());
    }
    println!();

    let has_control_plane = config
        .profiles
        .contains(&crate::types::InstallProfile::ControlPlane);
    let has_worker = config
        .profiles
        .contains(&crate::types::InstallProfile::Worker);

    if config.systemd && (has_control_plane || has_worker) {
        println!("Systemd Services:");
        if config.enable {
            if has_control_plane {
                println!("  • flame-session-manager: enabled and running");
            }
            if has_worker {
                println!("  • flame-executor-manager: enabled and running");
            }
            println!();
            println!("To check service status:");
            if has_control_plane {
                println!("  sudo systemctl status flame-session-manager");
            }
            if has_worker {
                println!("  sudo systemctl status flame-executor-manager");
            }
        } else {
            if has_control_plane {
                println!("  • flame-session-manager: installed (not enabled)");
            }
            if has_worker {
                println!("  • flame-executor-manager: installed (not enabled)");
            }
            println!();
            println!("To start services:");
            if has_control_plane {
                println!("  sudo systemctl enable --now flame-session-manager");
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
        if has_worker {
            println!("  sudo journalctl -u flame-executor-manager -f");
            println!("  tail -f {}/logs/fem.log", paths.prefix.display());
        }
    } else if !config.systemd && (has_control_plane || has_worker) {
        println!("Manual Service Management:");
        if has_control_plane {
            println!("  • Start session manager: {}/bin/flame-session-manager --config {}/conf/flame-cluster.yaml", 
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
    println!("  2. Add {}/bin to your PATH", paths.bin.display());

    // Provide relevant test command based on what was installed
    if has_control_plane {
        println!(
            "  3. Test the installation: {}/bin/flmctl --version",
            paths.bin.display()
        );
    } else if config
        .profiles
        .contains(&crate::types::InstallProfile::Client)
    {
        println!(
            "  3. Test the installation: {}/bin/flmping --version",
            paths.bin.display()
        );
    }
    println!();
}

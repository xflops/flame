// flmadm is a Unix-only administration tool for installing and managing Flame
// on Linux servers. It manages systemd services, Unix permissions, and other
// Unix-specific functionality.

#[cfg(unix)]
mod commands;
#[cfg(unix)]
mod managers;
#[cfg(unix)]
mod types;

#[cfg(unix)]
use clap::{Parser, Subcommand};
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(unix)]
#[derive(Parser)]
#[command(name = "flmadm")]
#[command(version = VERSION)]
#[command(about = "Flame Administration Tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[cfg(unix)]
#[derive(Subcommand)]
enum Commands {
    /// Install Flame on this machine
    Install {
        /// Source code directory for building Flame
        #[arg(long, value_name = "PATH")]
        src_dir: Option<PathBuf>,

        /// Target installation directory
        #[arg(long, default_value = "/usr/local/flame", value_name = "PATH")]
        prefix: PathBuf,

        /// Install control plane components (flame-session-manager, flmctl, flmadm)
        #[arg(long)]
        control_plane: bool,

        /// Install worker components (flame-executor-manager, flmping-service, flmexec-service, flamepy)
        #[arg(long)]
        worker: bool,

        /// Install client components (flmping, flmexec, flamepy)
        #[arg(long)]
        client: bool,

        /// Install all components (control plane + worker + client)
        #[arg(long)]
        all: bool,

        /// Skip systemd service generation
        #[arg(long)]
        no_systemd: bool,

        /// Enable and start systemd services after installation
        #[arg(long)]
        enable: bool,

        /// Skip building from source (use pre-built binaries)
        #[arg(long)]
        skip_build: bool,

        /// Remove existing installation before installing
        #[arg(long)]
        clean: bool,

        /// Force overwrite existing components without prompting
        #[arg(long)]
        force: bool,

        /// Show detailed build output
        #[arg(long)]
        verbose: bool,

        /// Generate CA and TLS certificates for mTLS authentication
        #[arg(long)]
        with_mtls: bool,
    },

    /// Uninstall Flame from this machine
    Uninstall {
        /// Installation directory to uninstall
        #[arg(long, default_value = "/usr/local/flame", value_name = "PATH")]
        prefix: PathBuf,

        /// Preserve data directory
        #[arg(long)]
        preserve_data: bool,

        /// Preserve configuration files
        #[arg(long)]
        preserve_config: bool,

        /// Preserve log files
        #[arg(long)]
        preserve_logs: bool,

        /// Custom backup directory
        #[arg(long, value_name = "PATH")]
        backup_dir: Option<PathBuf>,

        /// Do not create backup (permanently delete)
        #[arg(long)]
        no_backup: bool,

        /// Skip confirmation prompts
        #[arg(long)]
        force: bool,
    },
    Create(crate::commands::rbac::CreateCmd),
    List(crate::commands::rbac::ListCmd),
    Get(crate::commands::rbac::GetCmd),
    Update(crate::commands::rbac::UpdateCmd),
    Delete(crate::commands::rbac::DeleteCmd),
    Enable(crate::commands::rbac::EnableCmd),
    Disable(crate::commands::rbac::DisableCmd),
}

#[cfg(unix)]
fn main() {
    let _log_guard = match common::init_logger(None) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("Failed to initialize logger: {}", e);
            std::process::exit(types::exit_codes::INSTALL_FAILURE);
        }
    };

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Install {
            src_dir,
            prefix,
            control_plane,
            worker,
            client,
            all,
            no_systemd,
            enable,
            skip_build,
            clean,
            force,
            verbose,
            with_mtls,
        } => {
            // Validate profile flags
            if all && (control_plane || worker || client) {
                eprintln!(
                    "Error: --all cannot be used with --control-plane, --worker, or --client"
                );
                std::process::exit(types::exit_codes::INSTALL_FAILURE);
            }

            // Require explicit profile selection
            if !all && !control_plane && !worker && !client {
                eprintln!("Error: You must specify which components to install:");
                eprintln!(
                    "  --all              Install all components (control plane + worker + client)"
                );
                eprintln!("  --control-plane    Install control plane components only");
                eprintln!("  --worker           Install worker components only");
                eprintln!("  --client           Install client components only");
                eprintln!("\nYou can also combine profiles, for example:");
                eprintln!("  --control-plane --worker    Install control plane and worker");
                std::process::exit(types::exit_codes::INSTALL_FAILURE);
            }

            // Determine which profiles to install
            let profiles = if all {
                // Explicit --all flag: install all profiles
                vec![
                    types::InstallProfile::ControlPlane,
                    types::InstallProfile::Worker,
                    types::InstallProfile::Client,
                ]
            } else {
                // Specific profile flags specified
                let mut profiles = Vec::new();
                if control_plane {
                    profiles.push(types::InstallProfile::ControlPlane);
                }
                if worker {
                    profiles.push(types::InstallProfile::Worker);
                }
                if client {
                    profiles.push(types::InstallProfile::Client);
                }
                profiles
            };

            let config = types::InstallConfig {
                src_dir,
                prefix,
                systemd: !no_systemd,
                enable,
                skip_build,
                clean,
                verbose,
                profiles,
                force_overwrite: force,
                with_mtls,
            };
            commands::install::run(config)
        }
        Commands::Uninstall {
            prefix,
            preserve_data,
            preserve_config,
            preserve_logs,
            backup_dir,
            no_backup,
            force,
        } => {
            let config = types::UninstallConfig {
                prefix,
                preserve_data,
                preserve_config,
                preserve_logs,
                backup_dir,
                no_backup,
                force,
            };
            commands::uninstall::run(config)
        }
        Commands::Create(cmd) => {
            crate::commands::rbac::handle_create(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
        Commands::List(cmd) => {
            crate::commands::rbac::handle_list(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Get(cmd) => {
            crate::commands::rbac::handle_get(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Update(cmd) => {
            crate::commands::rbac::handle_update(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Delete(cmd) => {
            crate::commands::rbac::handle_delete(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Enable(cmd) => {
            crate::commands::rbac::handle_enable(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Disable(cmd) => {
            crate::commands::rbac::handle_disable(&cmd).map_err(|e| anyhow::anyhow!(e))
        }
    };

    match result {
        Ok(_) => std::process::exit(types::exit_codes::SUCCESS),
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(types::exit_codes::INSTALL_FAILURE);
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("flmadm is only supported on Unix-like systems (Linux, macOS).");
    eprintln!(
        "It manages systemd services, Unix permissions, and other Unix-specific functionality."
    );
    std::process::exit(1);
}

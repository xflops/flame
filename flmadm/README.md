# flmadm - Flame Administration Tool

`flmadm` is the administration CLI for Flame, designed for installing, configuring, and managing Flame clusters on bare-metal servers or VMs.

## Overview

Flame provides two separate CLI tools:
- **`flmctl`**: User-facing CLI for submitting jobs and managing sessions
- **`flmadm`**: Administrator CLI for installation and cluster management

## Features

- **Automated Installation**: One-command installation of all Flame components
- **Source Building**: Builds Flame binaries from source (Rust)
- **Python SDK Installation**: Automatically installs the Flame Python SDK
- **Systemd Integration**: Generates and manages systemd service files
- **User Management**: Creates and manages the `flame` user for service isolation
- **Configuration Generation**: Creates default configuration files
- **Clean Uninstallation**: Safely removes Flame with backup support
- **Flexible Installation**: Supports both system-wide and user-local installations

## Installation

To install `flmadm` itself, build it from source:

```bash
cd /path/to/flame
cargo build --release -p flmadm
sudo cp target/release/flmadm /usr/local/bin/
```

Or use an existing installation:

```bash
sudo flmadm install
```

## Usage

### Install Flame

**Basic installation (from GitHub):**
```bash
sudo flmadm install
```

**Install from local source:**
```bash
sudo flmadm install --src-dir /path/to/flame
```

**Custom installation directory:**
```bash
sudo flmadm install --prefix /opt/flame
```

**Install and start services:**
```bash
sudo flmadm install --enable
```

**User-local installation (no systemd):**
```bash
flmadm install --no-systemd --prefix ~/flame
```

**Clean install (backup and reinstall):**
```bash
sudo flmadm install --clean --enable
```

**Verbose build output:**
```bash
sudo flmadm install --verbose
```

### Uninstall Flame

**Basic uninstall (with backup):**
```bash
sudo flmadm uninstall
```

**Uninstall and preserve data:**
```bash
sudo flmadm uninstall --preserve-data --preserve-config
```

**Uninstall from custom location:**
```bash
sudo flmadm uninstall --prefix /opt/flame
```

**Complete removal (no backup):**
```bash
sudo flmadm uninstall --no-backup --remove-user --force
```

**Custom backup location:**
```bash
sudo flmadm uninstall --backup-dir /backups/flame-backup-2026-01-28
```

## Install Options

- `--src-dir <PATH>`: Source code directory for building Flame (default: clone from GitHub)
- `--prefix <PATH>`: Target installation directory (default: `/usr/local/flame`)
- `--no-systemd`: Skip systemd service generation (for user-local installs)
- `--enable`: Enable and start systemd services after installation
- `--skip-build`: Skip building from source (use pre-built binaries)
- `--clean`: Remove existing installation before installing (creates backup)
- `--verbose`: Show detailed build output (useful for debugging build issues)

## Uninstall Options

- `--prefix <PATH>`: Installation directory to uninstall (default: `/usr/local/flame`)
- `--preserve-data`: Preserve data directory (useful for reinstallation)
- `--preserve-config`: Preserve configuration files
- `--preserve-logs`: Preserve log files
- `--backup-dir <PATH>`: Custom backup directory
- `--no-backup`: Do not create backup (PERMANENTLY DELETE - use with caution!)
- `--force`: Skip confirmation prompts
- `--remove-user`: Remove the flame user and group

## Directory Structure

After installation, Flame uses the following directory structure:

```
${PREFIX}/
├── bin/                    # Flame binaries
│   ├── flame-session-manager
│   ├── flame-executor-manager
│   ├── flmctl
│   └── flmadm
├── sdk/python/             # Python SDK
├── work/                   # Working directory
│   ├── sessions/
│   └── executors/
├── logs/                   # Log files
│   ├── fsm.log
│   └── fem.log
├── conf/                   # Configuration
│   └── flame-cluster.yaml
└── data/                   # Data directory (cache, database)
    ├── cache/
    └── sessions.db
```

## Service Management

After installation with systemd, manage services using standard `systemctl` commands:

```bash
# Start services
sudo systemctl start flame-session-manager
sudo systemctl start flame-executor-manager

# Stop services
sudo systemctl stop flame-executor-manager
sudo systemctl stop flame-session-manager

# Enable on boot
sudo systemctl enable flame-session-manager flame-executor-manager

# Check status
sudo systemctl status flame-session-manager
sudo systemctl status flame-executor-manager

# View logs
sudo journalctl -u flame-session-manager -f
tail -f /usr/local/flame/logs/fsm.log
```

## Prerequisites

- **Rust toolchain**: Required for building from source (unless `--skip-build`)
  - Install: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Git**: Required for cloning from GitHub (unless `--src-dir` provided)
- **pip/pip3**: Required for installing Python SDK
- **systemd**: Required for service management (unless `--no-systemd`)
- **Root privileges**: Required for system-wide installation with systemd

## Examples

### Development Workflow

```bash
# Developer working on Flame source
cd ~/projects/flame
flmadm install --src-dir . --prefix ~/flame --no-systemd

# Make changes, rebuild
cargo build --release

# Reinstall with updated binaries
flmadm install --src-dir . --prefix ~/flame --skip-build --no-systemd --clean
```

### Production Deployment

```bash
# Initial installation
sudo flmadm install --enable

# Verify installation
sudo systemctl status flame-session-manager flame-executor-manager
/usr/local/flame/bin/flmctl --version

# Upgrade to new version
sudo systemctl stop flame-executor-manager flame-session-manager
sudo flmadm install --clean
sudo systemctl start flame-session-manager flame-executor-manager
```

### Testing and Cleanup

```bash
# Install for testing
sudo flmadm install --prefix /tmp/flame-test --no-systemd

# Test the installation
/tmp/flame-test/bin/flmctl --version

# Clean up
flmadm uninstall --prefix /tmp/flame-test --no-backup --force
```

## Troubleshooting

### Build Failures

If the build fails, use `--verbose` to see full cargo output:

```bash
sudo flmadm install --verbose
```

Build logs are also saved to `/tmp/flame-install-build.log` for offline debugging.

### Permission Errors

For system-wide installation, run with `sudo`:

```bash
sudo flmadm install
```

For user-local installation, use `--no-systemd`:

```bash
flmadm install --no-systemd --prefix ~/flame
```

### Service Start Failures

Check service status and logs:

```bash
sudo systemctl status flame-session-manager
sudo journalctl -u flame-session-manager -n 50
tail -50 /usr/local/flame/logs/fsm.log
```

### Uninstall Safety

`flmadm` includes safety checks to prevent accidental system damage:
- Cannot uninstall from system paths like `/`, `/usr`, `/etc`
- Installation path must contain "flame" in its name
- Requires confirmation unless `--force` is used
- Creates backup by default before removal

## Exit Codes

- `0`: Success
- `1`: Invalid arguments or configuration
- `2`: Build failure
- `3`: Installation failure (permissions, disk space, etc.)
- `4`: Systemd configuration failure
- `5`: Service start failure

## See Also

- `flmctl`: User CLI for job submission and session management
- [Installation Tutorial](../docs/tutorials/) (coming soon)
- [Flame Documentation](../docs/)
- [RFE333 Functional Specification](../docs/designs/RFE333-flmadm/FS.md)

## License

Same as Flame project license.

# RFE333: Flame Administration Tool

## 1. Motivation

**Background:**
Currently, deploying Flame in non-containerized environments requires manual steps: building binaries, setting up directories, configuring services, and managing the Python SDK. This manual process is error-prone and time-consuming, making it difficult for users to quickly set up Flame on bare-metal servers or VMs. Users need a simple, automated way to install, uninstall, and manage Flame components on a single machine.

The lack of a standardized installation and management tool makes it challenging to:
- Ensure consistent directory structures across deployments
- Properly configure system services (systemd)
- Manage dependencies and environment setup
- Upgrade existing installations
- Cleanly uninstall Flame when needed
- Troubleshoot installation issues

Additionally, there is a need to separate cluster administration commands from user-facing commands:
- `flmctl`: User-facing CLI for submitting jobs, managing sessions, etc.
- `flmadm`: Administrator CLI for installation, configuration, and cluster management

**Target:**
This design aims to provide an automated administration tool (`flmadm`) for Flame that:

1. **Simplicity**: Provide simple commands to install and uninstall Flame with sensible defaults
2. **Flexibility**: Allow users to customize installation paths and source locations
3. **Completeness**: Install all necessary components (binaries, SDK, configuration, systemd services)
4. **Reliability**: Create proper directory structures and set appropriate permissions
5. **Maintainability**: Generate systemd service files for easy service management
6. **Repeatability**: Support both fresh installations and updates from source
7. **Cleanliness**: Support complete uninstallation with data preservation options
8. **Role Separation**: Clear distinction between admin commands (`flmadm`) and user commands (`flmctl`)

## 2. Function Specification

### Configuration

**Installation Configuration:**
- Default installation prefix: `/usr/local/flame`
- Configuration file location: `${PREFIX}/conf/flame-cluster.yaml`
- Working directory: `${PREFIX}/work`
- Logs directory: `${PREFIX}/logs`
- Data directory: `${PREFIX}/data`

**Environment Variables:**
- `FLAME_PREFIX`: Override default installation prefix (optional)
- `RUST_LOG`: Set log level for Flame services (default: info)

### CLI

**Tool Name:** `flmadm` (Flame Administration Tool)

**Purpose:** Administrative CLI for Flame cluster management, including installation, uninstallation, and configuration. Separate from `flmctl` which is used for job submission and session management.

#### Install Subcommand

**Command:**
```bash
flmadm install [OPTIONS]
```

**Options:**
- `--src-dir <PATH>`: Source code directory for building and installing Flame
  - If not specified, clone from GitHub main branch to a temporary directory
  - Must contain a valid Flame repository with Cargo workspace
  
- `--prefix <PATH>`: Target installation directory (default: `/usr/local/flame`)
  - All Flame components will be installed under this directory
  - Must be an absolute path
  - Will be created if it doesn't exist

- `--systemd`: Generate and install systemd service files (default: true)
  - Requires root privileges or sudo
  - Service files will be created in `/etc/systemd/system/`
  
- `--no-systemd`: Skip systemd service generation
  
- `--enable`: Enable and start systemd services after installation (default: false)
  - Implies `--systemd`
  - Services will be enabled to start on boot

- `--skip-build`: Skip building from source, only install pre-built binaries
  - Assumes binaries are already built in `--src-dir`
  
- `--clean`: Remove existing installation before installing (default: false)
  - Stops services if running
  - Backs up configuration files

- `--verbose`: Show detailed build output instead of progress bar
  - Displays full cargo output
  - Useful for debugging build issues

**Usage Examples:**

1. **Basic installation (from GitHub):**
   ```bash
   sudo flmadm install
   ```

2. **Install from local source:**
   ```bash
   sudo flmadm install --src-dir /path/to/flame
   ```

3. **Custom installation directory:**
   ```bash
   sudo flmadm install --prefix /opt/flame
   ```

4. **Install and start services:**
   ```bash
   sudo flmadm install --enable
   ```

5. **Install without systemd:**
   ```bash
   flmadm install --no-systemd --prefix ~/flame
   ```

6. **Clean install:**
   ```bash
   sudo flmadm install --clean --enable
   ```

7. **Install with verbose build output:**
   ```bash
   sudo flmadm install --verbose
   ```

**Exit Codes:**
- `0`: Success
- `1`: Invalid arguments or configuration
- `2`: Build failure
- `3`: Installation failure (permissions, disk space, etc.)
- `4`: Systemd configuration failure
- `5`: Service start failure

#### Uninstall Subcommand

**Command:**
```bash
flmadm uninstall [OPTIONS]
```

**Purpose:** Cleanly remove Flame installation from the system, with options to preserve data and configuration.

**Options:**
- `--prefix <PATH>`: Installation directory to uninstall (default: `/usr/local/flame`)
  - Must match the prefix used during installation
  - Must be an absolute path

- `--preserve-data`: Preserve data directory (cache, database)
  - Keeps `${PREFIX}/data/` intact
  - Useful for reinstallation or upgrades
  - Default: false (data is backed up, not deleted)

- `--preserve-config`: Preserve configuration files
  - Keeps `${PREFIX}/conf/` intact
  - Useful for reinstallation with same configuration
  - Default: false (config is backed up, not deleted)

- `--preserve-logs`: Preserve log files
  - Keeps `${PREFIX}/logs/` intact
  - Default: false (logs are backed up, not deleted)

- `--backup-dir <PATH>`: Custom backup directory for preserved files
  - Default: `${PREFIX}.backup.{timestamp}`
  - Backup includes data, config, and logs unless --no-backup is set

- `--no-backup`: Do not create backup, permanently delete all files
  - Dangerous option, requires confirmation
  - If set, data/config/logs are deleted without backup

- `--force`: Skip confirmation prompts
  - Proceeds with uninstallation without user confirmation
  - Use with caution

- `--remove-user`: Remove the flame user and group
  - Default: false (user/group preserved for reinstallation)
  - Only removes if no other processes are running as flame user

**Usage Examples:**

1. **Basic uninstall (with backup):**
   ```bash
   sudo flmadm uninstall
   ```

2. **Uninstall and preserve data:**
   ```bash
   sudo flmadm uninstall --preserve-data --preserve-config
   ```

3. **Uninstall from custom location:**
   ```bash
   sudo flmadm uninstall --prefix /opt/flame
   ```

4. **Complete removal (no backup):**
   ```bash
   sudo flmadm uninstall --no-backup --remove-user --force
   ```

5. **Uninstall with custom backup location:**
   ```bash
   sudo flmadm uninstall --backup-dir /backups/flame-backup-2026-01-28
   ```

**Uninstall Process:**

1. **Pre-uninstall checks:**
   - Verify installation exists at specified prefix
   - Check if services are running
   - Confirm with user (unless --force)

2. **Stop services (if systemd was used):**
   - Stop flame-executor-manager
   - Stop flame-session-manager
   - Disable services from auto-start

3. **Create backup (unless --no-backup):**
   - Create backup directory: `${PREFIX}.backup.{timestamp}`
   - Copy data directory (unless --preserve-data)
   - Copy configuration (unless --preserve-config)
   - Copy logs (unless --preserve-logs)
   - Display backup location

4. **Remove systemd service files:**
   - Delete `/etc/systemd/system/flame-session-manager.service`
   - Delete `/etc/systemd/system/flame-executor-manager.service`
   - Run `systemctl daemon-reload`

5. **Remove binaries:**
   - Delete `${PREFIX}/bin/flame-session-manager`
   - Delete `${PREFIX}/bin/flame-executor-manager`
   - Delete `${PREFIX}/bin/flmctl` (if symlinked)
   - Remove `${PREFIX}/bin/` directory if empty

6. **Remove Python SDK:**
   - Remove `${PREFIX}/sdk/python/`
   - Remove `${PREFIX}/sdk/` directory if empty

7. **Remove working directory:**
   - Delete `${PREFIX}/work/`

8. **Remove data (unless --preserve-data or backed up):**
   - Delete `${PREFIX}/data/`

9. **Remove configuration (unless --preserve-config or backed up):**
   - Delete `${PREFIX}/conf/`

10. **Remove logs (unless --preserve-logs or backed up):**
    - Delete `${PREFIX}/logs/`

11. **Remove installation root:**
    - If `${PREFIX}` is empty, delete the directory
    - If not empty (preserved files), inform user

12. **Remove user (if --remove-user):**
    - Check if flame user has running processes
    - If safe, run: `userdel flame`
    - Remove flame group: `groupdel flame`

13. **Display uninstall summary:**
    - Components removed
    - Backup location (if created)
    - Preserved files (if any)
    - Manual cleanup steps (if any)

**Exit Codes:**
- `0`: Success
- `1`: Invalid arguments or installation not found
- `2`: Permission denied
- `3`: Services could not be stopped
- `4`: Uninstallation failure
- `5`: User/group removal failure (non-critical, continues)

**Safety Features:**
- Always requires confirmation unless --force
- Creates backup by default before deletion
- Stops services gracefully before removal
- Checks for running processes before removing user
- Validates paths to prevent accidental system deletion
- Preserves data/config by default if user cancels

### Directory Structure

**Installation Layout:**

```
${PREFIX}/
├── bin/                                    # Flame binaries
│   ├── flame-session-manager              # Session manager binary
│   ├── flame-executor-manager             # Executor manager binary
│   ├── flmctl                             # User CLI tool (symlinked)
│   └── flmadm                             # Admin CLI tool
│
├── sdk/                                    # SDK installation
│   └── python/                            # Python SDK
│       ├── flamepy/                       # Package directory
│       └── setup.py                       # Setup script
│
├── work/                                   # Working directory
│   ├── sessions/                          # Session working directories
│   └── executors/                         # Executor working directories
│
├── logs/                                   # Log directory
│   ├── fsm.log                            # Session manager logs
│   └── fem.log                            # Executor manager logs
│
├── conf/                                   # Configuration directory
│   └── flame-cluster.yaml                 # Cluster configuration
│
└── data/                                   # Data directory
    ├── cache/                             # Object cache storage
    └── sessions.db                        # Session database
```

**Directory Permissions:**
- `${PREFIX}/bin`: 0755 (readable/executable by all)
- `${PREFIX}/sdk`: 0755 (readable by all)
- `${PREFIX}/work`: 0755 (writable by flame user)
- `${PREFIX}/logs`: 0755 (writable by flame user)
- `${PREFIX}/conf`: 0755 (readable by all)
- `${PREFIX}/data`: 0700 (readable/writable by flame user only)

### Systemd Configuration

**Service Files:**

Two systemd service files will be generated:
1. `flame-session-manager.service`
2. `flame-executor-manager.service`

**flame-session-manager.service:**
```ini
[Unit]
Description=Flame Session Manager
Documentation=https://github.com/xflops/flame
After=network.target
Wants=network-online.target

[Service]
Type=simple
User=flame
Group=flame
Environment="RUST_LOG=info"
WorkingDirectory=${PREFIX}/work
ExecStart=${PREFIX}/bin/flame-session-manager --config ${PREFIX}/conf/flame-cluster.yaml
StandardOutput=append:${PREFIX}/logs/fsm.log
StandardError=append:${PREFIX}/logs/fsm.log
Restart=on-failure
RestartSec=5s
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

**flame-executor-manager.service:**
```ini
[Unit]
Description=Flame Executor Manager
Documentation=https://github.com/xflops/flame
After=network.target flame-session-manager.service
Wants=network-online.target
Requires=flame-session-manager.service

[Service]
Type=simple
User=flame
Group=flame
Environment="RUST_LOG=info"
WorkingDirectory=${PREFIX}/work
ExecStart=${PREFIX}/bin/flame-executor-manager --config ${PREFIX}/conf/flame-cluster.yaml
StandardOutput=append:${PREFIX}/logs/fem.log
StandardError=append:${PREFIX}/logs/fem.log
Restart=on-failure
RestartSec=5s
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

**Service Management:**
After installation, users can manage services using standard systemd commands:
```bash
# Start services
sudo systemctl start flame-session-manager
sudo systemctl start flame-executor-manager

# Stop services
sudo systemctl stop flame-executor-manager
sudo systemctl stop flame-session-manager

# Enable on boot
sudo systemctl enable flame-session-manager
sudo systemctl enable flame-executor-manager

# Check status
sudo systemctl status flame-session-manager
sudo systemctl status flame-executor-manager

# View logs
sudo journalctl -u flame-session-manager -f
sudo journalctl -u flame-executor-manager -f
```

### Default Configuration

**flame-cluster.yaml Template:**

The installer will generate a default `flame-cluster.yaml` configuration file:

```yaml
# Flame Cluster Configuration
# Generated by flmadm install
---
cluster:
  name: flame
  endpoint: "http://127.0.0.1:8080"
  slot: "cpu=1,mem=2g"
  policy: proportion
  storage: "sqlite://${PREFIX}/data/sessions.db"
executors:
  shim: host
  limits:
    max_executors: 128
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "lo"
  storage: "${PREFIX}/data/cache"
```

### Scope

**In Scope:**
- Implementation of `flmadm` CLI tool for cluster administration
- `flmadm install` subcommand for installation
- `flmadm uninstall` subcommand for clean removal
- Building Flame binaries from source (Rust components)
- Creating standard directory structure
- Installing Python SDK
- Generating default configuration file
- Creating systemd service files
- User and group management (creating/removing `flame` user)
- Basic validation of installation
- Support for both system-wide and user-local installations
- Backup and restore functionality during uninstall
- Data/config preservation options during uninstall

**Out of Scope:**
- Multi-node cluster installation (this is single-node only)
- Kubernetes deployment (use existing installer/)
- Docker/container-based installation (use docker compose)
- Automatic updates or package management
- Configuration wizard or interactive setup
- Database migrations for upgrades
- SSL/TLS certificate generation
- Firewall configuration
- SELinux/AppArmor policy configuration
- Cluster upgrade command (use `flmadm install --clean` for now)

**Limitations:**
- Single-node installation only (cannot install across multiple machines)
- Requires root/sudo for system-wide installation with systemd
- Requires Rust toolchain if building from source
- Does not handle complex network configurations
- No rollback mechanism for failed installations
- Python SDK installed via pip (requires pip to be available)
- Assumes Linux with systemd (no support for other init systems)

### Feature Interaction

**Related Features:**
- **flmadm**: New CLI tool for cluster administration (separate from flmctl)
- **flmctl**: User CLI for job submission (built and installed by flmadm)
- **Session Manager**: Binary installed and configured
- **Executor Manager**: Binary installed and configured
- **Object Cache**: Storage directory created and configured
- **Python SDK**: Installed in standard location

**Updates Required:**
1. **New Component: flmadm**: Create new Rust binary for administration commands
   - Add `flmadm/` directory in workspace
   - Implement `install` and `uninstall` subcommands
   - Share common installation utilities with other components
2. **common**: Add installation utilities (directory creation, file copying, templating, backup/restore)
3. **Python SDK setup.py**: Ensure it can be installed from non-standard locations
4. **Documentation**: Add installation tutorial and flmadm command reference

**Integration Points:**
- Uses Cargo to build Rust components (including flmadm itself)
- Uses pip to install Python SDK
- Integrates with systemd for service management
- Generates configuration files compatible with existing Flame components
- Creates directory structure expected by all Flame components
- Manages flame user/group for service isolation

**Compatibility:**
- Backward compatible with existing Flame installations (can upgrade)
- Configuration file format remains unchanged
- No breaking changes to existing components
- flmadm can install/uninstall different versions of Flame

**Breaking Changes:**
- None (flmadm is a new tool, flmctl unchanged)

## 3. Implementation Detail

### Architecture

The installer and administration functions are implemented in a new `flmadm` CLI tool. This tool is separate from `flmctl` (user CLI) and provides cluster administration capabilities.

**Install Process:**

```
┌─────────────────┐
│  flmadm install │
└────────┬────────┘
         │
         ├──► Phase 1: Validation
         │    - Check permissions
         │    - Validate paths
         │    - Check dependencies
         │
         ├──► Phase 2: Preparation
         │    - Clone/locate source
         │    - Create directories
         │    - Create flame user
         │
         ├──► Phase 3: Build
         │    - Build Rust binaries (including flmadm itself)
         │    - Prepare Python SDK
         │
         ├──► Phase 4: Installation
         │    - Copy binaries
         │    - Install Python SDK
         │    - Generate config files
         │
         ├──► Phase 5: Systemd Setup
         │    - Generate service files
         │    - Set permissions
         │    - Enable services (optional)
         │
         └──► Phase 6: Verification
              - Test binaries
              - Validate configuration
              - Report status
```

**Uninstall Process:**

```
┌───────────────────┐
│ flmadm uninstall  │
└────────┬──────────┘
         │
         ├──► Phase 1: Validation
         │    - Check installation exists
         │    - Confirm with user
         │
         ├──► Phase 2: Service Cleanup
         │    - Stop services
         │    - Disable services
         │    - Remove service files
         │
         ├──► Phase 3: Backup
         │    - Create backup directory
         │    - Backup data/config/logs
         │
         ├──► Phase 4: Removal
         │    - Remove binaries
         │    - Remove SDK
         │    - Remove working directories
         │
         ├──► Phase 5: User Cleanup
         │    - Remove flame user (optional)
         │    - Remove flame group (optional)
         │
         └──► Phase 6: Summary
              - Report removed components
              - Show backup location
              - Manual cleanup steps
```

### Components

**1. flmadm Binary**
- **Location**: `flmadm/` (new workspace component)
- **Subcommands**: `install`, `uninstall`
- **Responsibilities**:
  - Entry point for administration commands
  - Command-line argument parsing
  - Subcommand routing

**2. Install Command Module**
- **Location**: `flmadm/src/commands/install.rs`
- **Responsibilities**:
  - Parse install command-line arguments
  - Orchestrate installation phases
  - Handle errors and cleanup
  - Display progress and status messages
  - Validate prerequisites

**3. Uninstall Command Module**
- **Location**: `flmadm/src/commands/uninstall.rs`
- **Responsibilities**:
  - Parse uninstall command-line arguments
  - Confirm uninstallation with user
  - Stop and remove systemd services
  - Create backups of data/config
  - Remove installed files and directories
  - Remove flame user/group (optional)
  - Display uninstall summary

**4. InstallConfig**
- **Responsibilities**:
  - Store installation parameters
  - Provide default values
  - Validate configuration
  - Resolve paths (expand environment variables, canonicalize)

**5. UninstallConfig**
- **Responsibilities**:
  - Store uninstallation parameters
  - Provide default values
  - Validate configuration
  - Handle preserve/backup options

**6. SourceManager**
- **Responsibilities**:
  - Clone repository from GitHub if needed
  - Locate source directory
  - Validate source structure (check for Cargo.toml, expected directories)
  - Manage temporary directories

**7. BuildManager**
- **Responsibilities**:
  - Build Rust binaries using Cargo (including flmadm)
  - Display progress bar during build (default)
  - Capture and suppress verbose cargo output unless --verbose flag is set
  - Parse cargo output to update progress (e.g., "Compiling X/Y packages")
  - Verify build artifacts
  - Handle build errors (show relevant error messages even without --verbose)
  - Support release and debug builds

**8. InstallationManager**
- **Responsibilities**:
  - Create directory structure
  - Copy binaries to target locations
  - Install Python SDK using pip
  - Set file permissions
  - Generate configuration files
  - Handle file conflicts

**9. BackupManager**
- **Responsibilities**:
  - Create backup directories
  - Backup data, configuration, and logs
  - Restore from backup on failure
  - Validate backup integrity

**10. SystemdManager**
- **Responsibilities**:
  - Generate systemd service files from templates
  - Install service files to /etc/systemd/system/
  - Remove service files during uninstall
  - Reload systemd daemon
  - Enable/start/stop services
  - Verify service status

**11. UserManager**
- **Responsibilities**:
  - Check if flame user/group exists
  - Create flame user/group if needed
  - Remove flame user/group during uninstall (with safety checks)
  - Verify user permissions
  - Handle different installation scenarios (system vs user)

**12. ConfigGenerator**
- **Responsibilities**:
  - Generate flame-cluster.yaml from template
  - Substitute variables (${PREFIX})
  - Validate generated configuration
  - Preserve existing configuration if present

### Data Structures

**InstallConfig:**
```rust
pub struct InstallConfig {
    pub src_dir: Option<PathBuf>,      // Source directory
    pub prefix: PathBuf,                // Installation prefix
    pub systemd: bool,                  // Generate systemd files
    pub enable: bool,                   // Enable and start services
    pub skip_build: bool,               // Skip build phase
    pub clean: bool,                    // Clean install
    pub verbose: bool,                  // Show verbose build output
}

impl Default for InstallConfig {
    fn default() -> Self {
        Self {
            src_dir: None,
            prefix: PathBuf::from("/usr/local/flame"),
            systemd: true,
            enable: false,
            skip_build: false,
            clean: false,
            verbose: false,
        }
    }
}
```

**InstallationPaths:**
```rust
pub struct InstallationPaths {
    pub prefix: PathBuf,
    pub bin: PathBuf,               // ${PREFIX}/bin
    pub sdk_python: PathBuf,        // ${PREFIX}/sdk/python
    pub work: PathBuf,              // ${PREFIX}/work
    pub logs: PathBuf,              // ${PREFIX}/logs
    pub conf: PathBuf,              // ${PREFIX}/conf
    pub data: PathBuf,              // ${PREFIX}/data
    pub cache: PathBuf,             // ${PREFIX}/data/cache
}

impl InstallationPaths {
    pub fn new(prefix: PathBuf) -> Self {
        Self {
            bin: prefix.join("bin"),
            sdk_python: prefix.join("sdk/python"),
            work: prefix.join("work"),
            logs: prefix.join("logs"),
            conf: prefix.join("conf"),
            data: prefix.join("data"),
            cache: prefix.join("data/cache"),
            prefix,
        }
    }
}
```

**UninstallConfig:**
```rust
pub struct UninstallConfig {
    pub prefix: PathBuf,              // Installation prefix to uninstall
    pub preserve_data: bool,          // Preserve data directory
    pub preserve_config: bool,        // Preserve configuration
    pub preserve_logs: bool,          // Preserve logs
    pub backup_dir: Option<PathBuf>,  // Custom backup directory
    pub no_backup: bool,              // Skip backup creation
    pub force: bool,                  // Skip confirmation prompts
    pub remove_user: bool,            // Remove flame user/group
}

impl Default for UninstallConfig {
    fn default() -> Self {
        Self {
            prefix: PathBuf::from("/usr/local/flame"),
            preserve_data: false,
            preserve_config: false,
            preserve_logs: false,
            backup_dir: None,
            no_backup: false,
            force: false,
            remove_user: false,
        }
    }
}
```

**BuildArtifacts:**
```rust
pub struct BuildArtifacts {
    pub session_manager: PathBuf,   // Path to flame-session-manager binary
    pub executor_manager: PathBuf,  // Path to flame-executor-manager binary
    pub flmctl: PathBuf,           // Path to flmctl binary
    pub flmadm: PathBuf,           // Path to flmadm binary
}
```

### Algorithms

**Installation Algorithm:**

```
1. Parse and validate command-line arguments
   - Create InstallConfig from CLI options
   - Validate paths (prefix must be absolute)
   - Check for conflicting options

2. Pre-installation checks
   - Check if running as root (for system-wide install)
   - Check disk space availability
   - Check for existing installation
   - If --clean, backup and remove existing installation

3. Source preparation
   - If --src-dir provided:
     - Validate source directory exists
     - Check for required files (Cargo.toml, etc.)
   - Else:
     - Create temporary directory
     - Clone from GitHub main branch
     - Checkout main branch

4. Build phase (skip if --skip-build)
   - cd to source directory
   - For each binary (flame-session-manager, flame-executor-manager, flmctl, flmadm):
     - If --verbose: 
       - Run cargo build with output to stdout
       - Display all cargo compilation messages
     - Else (default):
       - Run cargo build and capture output
       - Parse output for progress information
       - Display progress bar: "Building [binary-name] [===>  ] X/Y packages"
       - Only show errors/warnings if build fails
   - Verify build artifacts exist (including flmadm binary)
   - Report total build time

5. Create directory structure
   - Create ${PREFIX}/bin
   - Create ${PREFIX}/sdk/python
   - Create ${PREFIX}/work (with subdirs: sessions, executors)
   - Create ${PREFIX}/logs
   - Create ${PREFIX}/conf
   - Create ${PREFIX}/data (with subdirs: cache, packages)
   - Set appropriate permissions

6. Create flame user (for system-wide install)
   - Check if flame user exists
   - If not, create with: useradd --system --no-create-home flame
   - Create flame group if needed

7. Install binaries
   - Copy flame-session-manager to ${PREFIX}/bin/
   - Copy flame-executor-manager to ${PREFIX}/bin/
   - Copy flmctl to ${PREFIX}/bin/ (or create symlink)
   - Copy flmadm to ${PREFIX}/bin/
   - Set executable permissions (0755)
   - Set ownership to flame:flame (for system-wide)

8. Install Python SDK
   - cd to sdk/python in source directory
   - Run: pip install -e . --target ${PREFIX}/sdk/python
   - Verify installation

9. Generate configuration
   - Check if ${PREFIX}/conf/flame-cluster.yaml exists
   - If exists and not --clean: backup and preserve
   - If not exists or --clean: generate from template
   - Substitute ${PREFIX} with actual prefix path
   - Set appropriate permissions (0644)
   - Set ownership to flame:flame (for system-wide)

10. Generate systemd service files (if --systemd)
    - Generate flame-session-manager.service
    - Generate flame-executor-manager.service
    - Substitute ${PREFIX} in templates
    - Copy to /etc/systemd/system/
    - Run: systemctl daemon-reload

11. Enable and start services (if --enable)
    - Run: systemctl enable flame-session-manager
    - Run: systemctl enable flame-executor-manager
    - Run: systemctl start flame-session-manager
    - Wait for session manager to be ready
    - Run: systemctl start flame-executor-manager
    - Verify services are running

12. Post-installation verification
    - Check binary paths and permissions
    - Verify configuration file is valid
    - Test binary execution: ${PREFIX}/bin/flmctl --version
    - If systemd: check service status

13. Display installation summary
    - Installation prefix
    - Binaries installed
    - Configuration location
    - Service status (if systemd)
    - Next steps for user

14. Cleanup
    - Remove temporary directories
    - Remove build artifacts (if cloned)
```

**Uninstallation Algorithm:**

```
1. Parse and validate command-line arguments
   - Create UninstallConfig from CLI options
   - Validate paths (prefix must exist and be absolute)
   - Check for conflicting options (e.g., --no-backup with --backup-dir)

2. Pre-uninstall validation
   - Check if ${PREFIX} exists
   - Verify it's a Flame installation (check for expected files/directories)
   - Display what will be uninstalled
   - Confirm with user (unless --force)
   - If user cancels, exit gracefully

3. Check and stop services
   - Check if systemd services exist
   - If services are running:
     - Stop flame-executor-manager first
     - Stop flame-session-manager
     - Wait for clean shutdown (max 30 seconds)
     - If services don't stop, warn user and continue

4. Create backup (unless --no-backup)
   - Determine backup directory:
     - If --backup-dir provided: use it
     - Else: use ${PREFIX}.backup.{timestamp}
   - Create backup directory
   - If NOT --preserve-data:
     - Copy ${PREFIX}/data/ to backup
   - If NOT --preserve-config:
     - Copy ${PREFIX}/conf/ to backup
   - If NOT --preserve-logs:
     - Copy ${PREFIX}/logs/ to backup
   - Display backup location

5. Remove systemd service files
   - Check if service files exist:
     - /etc/systemd/system/flame-session-manager.service
     - /etc/systemd/system/flame-executor-manager.service
   - If found:
     - Disable services: systemctl disable flame-*
     - Remove service files
     - Run: systemctl daemon-reload

6. Remove binaries
   - Remove ${PREFIX}/bin/flame-session-manager
   - Remove ${PREFIX}/bin/flame-executor-manager
   - Remove ${PREFIX}/bin/flmctl
   - Remove ${PREFIX}/bin/flmadm
   - If ${PREFIX}/bin/ is empty, remove directory

7. Remove Python SDK
   - Remove ${PREFIX}/sdk/python/
   - Remove ${PREFIX}/sdk/ if empty

8. Remove working directory
   - Remove ${PREFIX}/work/sessions/
   - Remove ${PREFIX}/work/executors/
   - Remove ${PREFIX}/work/

9. Remove data directory (unless --preserve-data)
   - If --preserve-data:
     - Keep ${PREFIX}/data/ intact
     - Log that data was preserved
   - Else:
     - Remove ${PREFIX}/data/cache/
     - Remove ${PREFIX}/data/sessions.db
     - Remove ${PREFIX}/data/

10. Remove configuration directory (unless --preserve-config)
    - If --preserve-config:
      - Keep ${PREFIX}/conf/ intact
      - Log that config was preserved
    - Else:
      - Remove ${PREFIX}/conf/flame-cluster.yaml
      - Remove ${PREFIX}/conf/

11. Remove logs directory (unless --preserve-logs)
    - If --preserve-logs:
      - Keep ${PREFIX}/logs/ intact
      - Log that logs were preserved
    - Else:
      - Remove ${PREFIX}/logs/fsm.log
      - Remove ${PREFIX}/logs/fem.log
      - Remove ${PREFIX}/logs/

12. Remove installation root (if empty)
    - Check if ${PREFIX} is empty
    - If empty: remove ${PREFIX}/
    - If not empty: 
      - List remaining files/directories
      - Inform user that prefix was not removed due to remaining files

13. Remove flame user and group (if --remove-user)
    - Check if flame user exists
    - Check for running processes owned by flame user
    - If no processes:
      - Run: userdel flame
      - Run: groupdel flame (if group exists)
    - If processes exist:
      - Warn user that user was not removed
      - List processes preventing removal

14. Display uninstall summary
    - Components removed:
      - Binaries
      - Services
      - SDK
      - Working directories
      - Data (if removed)
      - Configuration (if removed)
      - Logs (if removed)
      - User/group (if removed)
    - Backup location (if created)
    - Preserved components (if any)
    - Manual cleanup required (if any)
```

**Uninstall Error Handling:**
- Installation not found: Display error, exit with code 1
- Permission denied: Suggest running with sudo, exit with code 2
- Services won't stop: Warn user, continue with uninstall, exit with code 3
- Backup creation failed: Abort uninstall, exit with code 4
- User removal failed: Warn user, continue with rest of uninstall, exit with code 0 (non-critical)

**Uninstall Safety Checks:**
```
1. Path validation:
   - Prevent removing critical system paths (/, /usr, /etc, /home, etc.)
   - Require ${PREFIX} to contain "flame" in path
   - Verify ${PREFIX} contains expected Flame structure

2. Confirmation prompts:
   - Always confirm unless --force
   - Show list of what will be removed
   - Show backup location if creating backup
   - For --no-backup: require additional confirmation ("Type 'yes' to confirm")

3. User removal safety:
   - Check for running processes before removing user
   - List processes if found
   - Only remove if no processes are running as flame user
```

**Error Handling:**
- Each phase has specific error handling
- Failed build: Print cargo error output, exit with code 2
- Permission denied: Suggest running with sudo, exit with code 3
- Systemd failure: Print systemctl error, exit with code 4
- Service start failure: Check logs, exit with code 5
- On critical failure: Attempt cleanup, restore backup if exists

**Backup Strategy (for --clean):**
```
1. If ${PREFIX} exists:
   - Create backup directory: ${PREFIX}.backup.{timestamp}
   - Copy ${PREFIX}/conf/ to backup (preserve configuration)
   - Copy ${PREFIX}/data/ to backup (preserve data)
   - Inform user of backup location

2. On installation failure:
   - Offer to restore from backup
   - Restore if user confirms
```

**Build Progress Bar Algorithm:**
```
1. For each binary to build:
   - Display: "Building [binary-name]..."
   
2. Spawn cargo build process with stdout/stderr captured

3. Parse cargo output in real-time:
   - Match lines like "   Compiling [package] v[version]"
   - Extract current package number and total from cargo metadata
   - Calculate progress percentage: (current / total) * 100
   
4. Update progress bar:
   - Format: "Building [binary-name] [===>    ] 45/120 (37%)"
   - Use ANSI escape codes to update line in-place
   - Update every time a new package compilation starts
   
5. On completion:
   - Display: "Building [binary-name] [========] 120/120 (100%) ✓"
   - Show elapsed time
   
6. On error:
   - Stop progress bar
   - Display relevant error messages from cargo
   - Show last 20 lines of cargo output for context
   - Suggest using --verbose flag for full output
   
7. If --verbose flag is set:
   - Skip all progress bar logic
   - Stream cargo output directly to stdout in real-time
```

**Progress Bar Implementation Notes:**
- Use indicatif crate or similar for cross-platform progress bar rendering
- Handle terminal width gracefully (minimum 80 chars)
- Disable progress bar if not a TTY (e.g., piped output, CI environments)
- Always show progress bar for interactive terminals by default
- Save full cargo output to temporary log file (e.g., /tmp/flame-install-build.log)
- On error, inform user of log file location for debugging

### System Considerations

**Performance:**
- Build phase is CPU-intensive (Rust compilation)
- Expected build time: 5-10 minutes on modern hardware
- Installation (copy) phase is fast: <30 seconds
- No runtime performance impact

**Scalability:**
- Single-node installation only
- Not applicable to distributed installations
- For clusters, use Kubernetes installer

**Reliability:**
- Atomic operations where possible (move vs copy)
- Backup existing installation before clean install
- Rollback capability on failure
- Verify each phase before proceeding

**Resource Usage:**
- Disk space: ~500MB for binaries + source (if cloned)
- Memory: Minimal during installation
- Network: Required if cloning from GitHub
- CPU: High during build phase

**Security:**
- Requires root/sudo for system-wide installation
- Creates dedicated flame user for service isolation
- Sets restrictive permissions on data directory (0700)
- Binaries are readable but not writable by regular users
- Configuration files readable by all but writable only by root
- No secrets stored in configuration (must be configured separately)

**Observability:**
- Progress bar during build phase (suppresses verbose cargo output by default)
- Verbose mode available via --verbose flag for debugging build issues
- Progress indicators for other long-running operations (clone, install)
- Clear error messages with remediation suggestions (always shown even without --verbose)
- Build time reporting
- Installation summary at completion
- Service status check after installation

**Operational:**
- Straightforward single-command installation
- Easy to script and automate
- Can be run idempotently (with --clean)
- Systemd integration for production use
- Standard directory layout for easy maintenance

### Dependencies

**External Dependencies:**
- **Rust toolchain**: Required for building from source (unless --skip-build)
  - `cargo` command must be available
  - Version: 1.70.0 or later
  
- **Git**: Required for cloning from GitHub (if --src-dir not provided)
  
- **pip**: Required for installing Python SDK
  - Python 3.8 or later
  
- **systemd**: Required for service management (unless --no-systemd)
  - `systemctl` command must be available
  
- **Standard Unix tools**: mkdir, cp, chmod, chown, useradd

**Internal Dependencies:**
- All Flame components (built from source)
- Python SDK (installed from source)
- Common Rust libraries for installation utilities

**Additional Rust Crates for Installer:**
- `indicatif`: Progress bar rendering (for build progress)
- `console`: Terminal color and formatting support
- `tempfile`: Temporary directory management (for cloning)
- `which`: Check for required command availability (cargo, git, etc.)

**Version Requirements:**
- Rust: 1.70.0+
- Python: 3.8+
- Git: 2.0+
- systemd: 219+ (standard on modern Linux distributions)

## 4. Use Cases

### Basic Use Cases

**Example 1: Fresh Installation (System-Wide)**

- **Description**: Administrator installs Flame on a fresh server
- **Step-by-step workflow**:
  1. Administrator runs: `sudo flmadm install --enable`
  2. Installer clones Flame from GitHub main branch (with progress indicator)
  3. Builds all binaries (5-10 minutes):
     - Shows progress bar: "Building flame-session-manager [===>  ] 45/120 (37%)"
     - No verbose cargo output (clean UI)
     - Displays completion: "Building flame-session-manager ✓ (3m 24s)"
     - Repeats for flame-executor-manager, flmctl, and flmadm
  4. Creates directory structure under `/usr/local/flame`
  5. Creates `flame` user and group
  6. Installs binaries to `/usr/local/flame/bin` (including flmadm)
  7. Installs Python SDK
  8. Generates default configuration
  9. Creates systemd service files
  10. Enables and starts services
  11. Displays installation summary
- **Expected outcome**: 
  - Flame is fully installed and running
  - Services start automatically on boot
  - Users can use `flmctl` for job management
  - Administrators can use `flmadm` for cluster management
  - Configuration at `/usr/local/flame/conf/flame-cluster.yaml`
  - Clean installation experience without overwhelming cargo output

**Example 2: Development Installation (User-Local)**

- **Description**: Developer installs Flame in their home directory for development
- **Step-by-step workflow**:
  1. Developer navigates to Flame source: `cd ~/projects/flame`
  2. Runs: `flmadm install --src-dir . --prefix ~/flame --no-systemd`
  3. Installer validates source directory
  4. Builds binaries in release mode
  5. Creates directory structure under `~/flame`
  6. Installs binaries and SDK
  7. Generates configuration
  8. Skips systemd (user-local installation)
  9. Displays installation summary
- **Expected outcome**:
  - Flame installed in `~/flame`
  - No system services created
  - Developer can start services manually
  - Can rebuild and reinstall easily

**Example 3: Custom Installation Path**

- **Description**: Administrator wants to install Flame in `/opt/flame`
- **Step-by-step workflow**:
  1. Runs: `sudo flmadm install --prefix /opt/flame --enable`
  2. Installer proceeds with custom prefix
  3. All directories created under `/opt/flame`
  4. Systemd services configured with custom paths
  5. Services started
- **Expected outcome**:
  - Flame installed in `/opt/flame`
  - Services use `/opt/flame` paths
  - Standard systemd management applies

**Example 4: Upgrade Existing Installation**

- **Description**: Administrator upgrades Flame to latest version
- **Step-by-step workflow**:
  1. Stops services: `sudo systemctl stop flame-executor-manager flame-session-manager`
  2. Runs: `sudo flmadm install --clean`
  3. Installer backs up configuration and data
  4. Removes old binaries
  5. Installs new binaries
  6. Preserves configuration and data
  7. Updates systemd service files if needed
  8. Asks whether to start services
- **Expected outcome**:
  - Flame upgraded to latest version
  - Configuration and data preserved
  - Services ready to start

### Advanced Use Cases

**Example 5: Installation from Specific Branch**

- **Description**: Developer wants to install from a feature branch
- **Step-by-step workflow**:
  1. Clone specific branch: `git clone -b feature-branch https://github.com/xflops/flame.git`
  2. Build: `cd flame && cargo build --release`
  3. Install: `sudo flmadm install --src-dir . --skip-build --enable`
  4. Installer uses pre-built binaries
  5. Installs everything else normally
- **Expected outcome**:
  - Feature branch version installed
  - Services running with custom build

**Example 6: Multi-Instance Installation**

- **Description**: Administrator wants to run multiple Flame instances on same machine
- **Step-by-step workflow**:
  1. Install first instance: `sudo flmadm install --prefix /opt/flame1 --no-systemd`
  2. Install second instance: `sudo flmadm install --prefix /opt/flame2 --no-systemd`
  3. Manually configure different ports in each `flame-cluster.yaml`
  4. Create custom systemd services for each instance (manual)
  5. Start services manually
- **Expected outcome**:
  - Two independent Flame installations
  - Each with different ports
  - Manual service management required

**Example 7: Offline Installation**

- **Description**: Install Flame on a machine without internet access
- **Step-by-step workflow**:
  1. On internet-connected machine: Clone Flame source and build
  2. Package built binaries and source: `tar czf flame-install.tar.gz flame/`
  3. Transfer to offline machine
  4. Extract: `tar xzf flame-install.tar.gz`
  5. Install: `sudo flmadm install --src-dir flame --skip-build --enable`
  6. Installer uses provided binaries
- **Expected outcome**:
  - Flame installed without internet access
  - All components functional

**Example 8: Installation Verification**

- **Description**: Verify installation completed successfully
- **Step-by-step workflow**:
  1. After installation, check version: `${PREFIX}/bin/flmctl --version`
  2. Check admin tool: `${PREFIX}/bin/flmadm --version`
  3. Check configuration: `cat ${PREFIX}/conf/flame-cluster.yaml`
  4. Check services: `sudo systemctl status flame-session-manager flame-executor-manager`
  5. Check logs: `tail ${PREFIX}/logs/fsm.log ${PREFIX}/logs/fem.log`
  6. Test Python SDK: `python -c "import flamepy; print(flamepy.__version__)"`
  7. Submit test job: `${PREFIX}/bin/flmctl submit --help`
- **Expected outcome**:
  - All components verified
  - System ready for use

**Example 9: Debugging Build Failure**

- **Description**: Administrator encounters a build error and needs detailed output
- **Step-by-step workflow**:
  1. First attempt: `sudo flmadm install`
  2. Build fails during flame-executor-manager compilation
  3. Installer shows:
     - Progress bar stops at failure point
     - Last 20 lines of cargo error output
     - Message: "Build failed. For full output, check /tmp/flame-install-build.log or rerun with --verbose"
  4. Administrator reruns with verbose: `sudo flmadm install --verbose`
  5. This time, sees full cargo output streaming in real-time
  6. Identifies issue (e.g., missing system dependency)
  7. Fixes issue and reruns installation
- **Expected outcome**:
  - Default mode provides clean UX but enough info for common errors
  - Verbose mode gives full diagnostic output when needed
  - Build log saved for offline debugging

**Example 10: Clean Uninstall with Backup**

- **Description**: Administrator wants to remove Flame but keep a backup
- **Step-by-step workflow**:
  1. Administrator runs: `sudo flmadm uninstall`
  2. Tool prompts for confirmation, showing what will be removed
  3. Administrator confirms
  4. Uninstaller stops services gracefully
  5. Creates backup at `/usr/local/flame.backup.2026-01-28-143022`
  6. Removes all binaries, SDK, working directories
  7. Removes systemd service files
  8. Displays summary with backup location
- **Expected outcome**:
  - Flame completely removed
  - Backup created with all data and configuration
  - Can restore by copying backup back

**Example 11: Uninstall Preserving Data**

- **Description**: Administrator wants to reinstall but keep data
- **Step-by-step workflow**:
  1. Runs: `sudo flmadm uninstall --preserve-data --preserve-config`
  2. Uninstaller confirms, showing preserved items
  3. Stops services
  4. Removes binaries, SDK, working directories
  5. Keeps `/usr/local/flame/data/` and `/usr/local/flame/conf/` intact
  6. Creates backup of logs only
  7. Displays summary
  8. Administrator can reinstall: `sudo flmadm install`
  9. New installation reuses existing data and config
- **Expected outcome**:
  - Binaries updated
  - Data and configuration preserved
  - No data loss

**Example 12: Complete Removal (No Backup)**

- **Description**: Administrator wants to permanently remove all traces of Flame
- **Step-by-step workflow**:
  1. Runs: `sudo flmadm uninstall --no-backup --remove-user --force`
  2. Tool warns about permanent deletion, requires typing "yes"
  3. Administrator types "yes"
  4. Uninstaller stops services
  5. Removes all files (no backup created)
  6. Removes flame user and group
  7. Removes systemd service files
  8. Displays summary
- **Expected outcome**:
  - Flame completely removed
  - No backup created
  - flame user/group removed
  - System clean

## 5. References

### Related Documents
- GitHub Issue: https://github.com/xflops/flame/issues/333
- Installation Tutorial: (to be created after implementation)
- AGENTS.md: Project development guidelines

### External References
- systemd service documentation: https://www.freedesktop.org/software/systemd/man/systemd.service.html
- Cargo build documentation: https://doc.rust-lang.org/cargo/commands/cargo-build.html
- pip install documentation: https://pip.pypa.io/en/stable/cli/pip_install/

### Implementation References
- flmadm implementation: `flmadm/src/main.rs` (new component)
- flmadm commands: `flmadm/src/commands/` (install, uninstall)
- flmctl implementation: `flmctl/src/main.rs` (user CLI)
- Common utilities: `common/src/` (shared installation/backup utilities)
- Python SDK: `sdk/python/`
- Systemd service examples: `ci/supervisord/` (for reference, not systemd but service config)

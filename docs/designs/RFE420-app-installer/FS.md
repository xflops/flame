# RFE420: Application Installer in Executor Manager

## 1. Motivation

**Background:**

Currently, application package installation is handled by the application itself (e.g., `flamepy.runner.runpy.FlameRunpyService`) during `on_session_enter()`. This approach has significant limitations:

1. **Duplicate Installation**: When multiple executors bind to sessions using the same application, each executor downloads and installs the package independently. For applications with large dependencies, this wastes bandwidth and storage.

2. **Concurrent Installation Conflicts**: Multiple executors starting simultaneously can attempt concurrent `pip install` operations on the same package, leading to race conditions and potential installation failures.

3. **Application Complexity**: Each application (runner) must implement its own package installation logic, increasing complexity and code duplication. The current `_install_package_from_url()` method in `runpy.py` is ~130 lines of complex installation handling.

4. **No Shared State**: There's no centralized knowledge of which applications are already installed, forcing unnecessary re-installations even when packages are already available.

**Target:**

Move application package installation from the application layer to the executor manager, achieving:

1. **Single Installation**: Per-application locking ensures only one installation occurs, regardless of how many executors need the same application.
2. **Shared Results**: After installation, all executors receive the environment configuration (e.g., `PYTHONPATH`) without performing installation themselves.
3. **Simplified Applications**: Applications no longer need to handle package installation—they can assume packages are already available.
4. **Faster Startup**: Subsequent executors skip installation entirely and immediately proceed to task execution.

## 2. Function Specification

### Configuration

Add an optional `installer` field to the application configuration specifying the installer type:

```yaml
# Application specification with installer
application:
  name: my-ml-app
  shim: Host
  command: python
  arguments: ["-m", "mymodule"]
  url: "file:///opt/packages/my-ml-app.tar.gz"
  installer: python    # Installer type: python, or empty/omitted for no installation
```

**Supported Installer Types:**

| Type | Description | Install Location | Returned Environment Variables |
|------|-------------|------------------|-------------------------------|
| `python` | Python package installer using `uv` | `$FLAME_HOME/data/apps/<app_name>/` | `PYTHONPATH`, `LD_LIBRARY_PATH` |

**Prerequisites (handled by `flmadm install`):**

- `flamepy` and dependencies installed in `$FLAME_HOME/lib/python/`
- `uv` available in `$FLAME_HOME/bin/` or system PATH

**Python Installer Behavior:**

1. Downloads package from `url` field
2. Extracts archive to `$FLAME_HOME/data/apps/<app_name>/`
3. Runs `uv pip install --target $FLAME_HOME/data/apps/<app_name>/lib .`
4. Returns environment variables:
   - `PYTHONPATH=$FLAME_HOME/data/apps/<app_name>/lib`
   - `LD_LIBRARY_PATH` (if native extensions detected)

**Default Behavior:**

If `installer` is not specified or empty, **no installation is performed**. This applies to:
- WASM applications (shim: Wasm) - packages are self-contained in the `.wasm` file
- Pre-installed applications - binaries already available on the system (e.g., `flmping`, `flmexec`)
- Applications that handle their own installation internally

The executor manager simply skips the installation step and proceeds directly to shim creation.

### Proto Definition

Extend `ApplicationSpec` in `rpc/protos/types.proto`:

```protobuf
message ApplicationSpec {
  // ... existing fields ...
  optional string url = 12;        // Package URL (existing)
  optional string installer = 13;  // Installer type: "python" (new)
}
```

### API

**No external API changes required.** The installation is transparent to clients. Existing APIs continue to work unchanged:
- `RegisterApplication` / `UpdateApplication` - Accept optional `installer` field
- `GetApplication` - Returns `installer` field if configured
- Session and task APIs remain unchanged

### CLI

**flmctl** application management:

```bash
# Register application with installer (via YAML or JSON)
flmctl apply -f my-app.yaml

# View application with installer config
flmctl get app my-app -o yaml
```

### Scope

**In Scope:**
- Application package download and extraction
- Package installation with per-application locking
- Environment variable propagation to shims (`PYTHONPATH`, `LD_LIBRARY_PATH`)
- Installation logging and error handling
- Support for `file://` and `http://`/`https://` package URLs

**Out of Scope:**
- Container image building/pulling (handled by existing infrastructure)
- Virtual environment management per-executor (shim responsibility)
- Package versioning/dependency resolution (delegated to uv)
- Package uninstallation on session leave (future enhancement)
- Other installer types (future: `node`, `rust`, etc.)

**Limitations:**
- Installation occurs on first executor bind; subsequent binds wait for completion
- Network failures during download will cause executor binding to fail
- Package installation is node-local; multi-node clusters install per-node

### Feature Interaction

**Related Features:**
- **flmadm install**: Installs `uv` and `flamepy` as prerequisites
- **Host shim (`executor_manager/src/shims/host_shim.rs`)**: Calls the shared ApplicationManager while creating the host service and merges the returned environment
- **Session Binding (`idle.rs`)**: Passes the shared ApplicationManager to `create_service`
- **Runner (`flamepy/runner/runpy.py`)**: Simplified to remove installation logic

**Updates Required:**
- `runpy.py`: Remove `_install_package_from_url()` method; rely on pre-installed packages
- `idle.rs`: Pass the shared ApplicationManager to `Shim::create_service`
- `types.proto`: Add `installer` field to `ApplicationSpec`
- `common/src/apis/types.rs`: Add `installer` field to `Application`
- `flmadm`: Ensure `uv` is installed during `flmadm install`

**Integration Points:**
- `HostShim::create_service` invokes ApplicationManager before launching the process
- Environment variables flow: ApplicationManager → HostShim → Application process
- CRI and WASM shims do not invoke ApplicationManager

**Compatibility:**
- Backward compatible: Applications without `installer` field work unchanged
- WASM applications: No installer needed; `.wasm` file is self-contained
- Applications with `url` but no `installer`: Continue to handle installation internally (e.g., `runpy.py`)

**Breaking Changes:**
- None. Existing applications continue to work.

## 3. Implementation Detail

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Executor Manager                                   │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                        ApplicationManager                              │  │
│  │  ┌─────────────────────────────────────────────────────────────────┐  │  │
│  │  │                    installed_apps                                │  │  │
│  │  │  HashMap<ApplicationID, Arc<RwLock<AppInstaller>>>              │  │  │
│  │  │                                                                  │  │  │
│  │  │  "my-app" -> AppInstaller {                                     │  │  │
│  │  │                state: Installed,                                 │  │  │
│  │  │                env_vars: { PYTHONPATH: "...",                   │  │  │
│  │  │                            LD_LIBRARY_PATH: "..." }             │  │  │
│  │  │              }                                                   │  │  │
│  │  │                                                                  │  │  │
│  │  │  "other-app" -> AppInstaller {                                  │  │  │
│  │  │                   state: Installing,                             │  │  │
│  │  │                   waiters: [Notify, Notify, ...]                │  │  │
│  │  │                 }                                                │  │  │
│  │  └─────────────────────────────────────────────────────────────────┘  │  │
│  │                                                                        │  │
│  │  install(&app) -> Result<EnvVars, FlameError>                         │  │
│  │    1. Check installer type (e.g., "python")                           │  │
│  │    2. Check if already installed -> return env_vars                   │  │
│  │    3. Acquire per-app lock                                            │  │
│  │    4. Download package from app.url                                   │  │
│  │    5. Extract to $FLAME_HOME/data/apps/<app_name>/                    │  │
│  │    6. Run type-specific installer (e.g., uv pip install)              │  │
│  │    7. Compute & store env_vars (PYTHONPATH, LD_LIBRARY_PATH)          │  │
│  │    8. Return env_vars                                                 │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                              │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                          IdleState                                     │  │
│  │                                                                        │  │
│  │  execute() {                                                           │  │
│  │    ssn = bind_executor()                                              │  │
│  │    shim = shims::new_ptr(&executor, Some(&ssn.application))           │  │
│  │    shim.create_service(app_manager.clone()).await?                    │  │
│  │    shim.on_session_enter(&ssn)                                        │  │
│  │  }                                                                     │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                              │
│  HostShim::create_service(app_manager)                                       │
│    1. env_vars = app_manager.install(&app)                                   │
│    2. create the executor work directory                                     │
│    3. launch the process with env_vars                                        │
│    4. connect to its Instance socket                                          │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Directory Layout

```
$FLAME_HOME/
├── bin/
│   └── uv                              # Installed by flmadm
├── lib/
│   └── python3.x/
│       └── site-packages/
│           └── flamepy/                # Installed by flmadm
├── data/
│   ├── cache/
│   │   └── uv/                         # uv cache (shared across apps)
│   └── apps/
│       └── <app_name>/
│           ├── src/                    # Extracted package source
│           │   ├── pyproject.toml
│           │   ├── mymodule/
│           │   └── ...
│           └── deps/                   # Installed dependencies (--target)
│               ├── numpy/
│               ├── pandas/
│               ├── torch/
│               │   └── lib/            # Native .so files
│               └── ...
└── logs/
    └── install/
        └── <app_name>.log              # Installation logs
```

**Key Directories:**
- `$FLAME_HOME/lib/python3.x/site-packages/` - Base dependencies (flamepy, etc.) installed by flmadm, shared across all apps
- `<app_name>/src/` - Extracted user package source code
- `<app_name>/deps/` - App-specific dependencies installed via `uv pip install --target` (isolated per-app)

**PYTHONPATH Order:** `deps/:src/:$FLAME_HOME/lib/python3.x/site-packages/`
- App-specific deps take precedence (allows version overrides)
- Base deps (flamepy) shared to avoid re-download

### Components

**ApplicationManager** (`executor_manager/src/app_manager.rs`):
- Singleton per executor-manager process
- Manages application installation state
- Dispatches to type-specific installers via `Installer` trait
- Provides per-application locking

**Installer Trait** (`executor_manager/src/app_manager/installer.rs`):
- Defines the interface for all installer implementations
- Each installer type implements this trait
- Returns environment variables to be exported to the executor

**PythonInstaller** (`executor_manager/src/app_manager/python.rs`):
- Implements `Installer` trait for `installer: python`
- Uses `uv pip install --target` for isolation
- Computes `PYTHONPATH` and `LD_LIBRARY_PATH`

**AppInstaller**:
- Tracks installation state: `NotInstalled`, `Installing`, `Installed`, `Failed`
- Stores resulting environment variables
- Maintains list of waiters for concurrent requests

### Data Structures

```rust
/// Installer trait - implemented by each installer type
#[async_trait]
pub trait Installer: Send + Sync {
    /// Install the package and return environment variables
    async fn install(
        &self,
        app_name: &str,
        src_path: &Path,
        flame_home: &Path,
    ) -> Result<HashMap<String, String>, FlameError>;
    
    /// Get the installer type name (for logging)
    fn name(&self) -> &'static str;
}

/// Supported installer types
#[derive(Clone, Debug, PartialEq)]
pub enum InstallerType {
    Python,
    // Future: Node, Rust, etc.
}

impl InstallerType {
    /// Create the corresponding Installer implementation
    pub fn create_installer(&self) -> Box<dyn Installer> {
        match self {
            InstallerType::Python => Box::new(PythonInstaller::new()),
        }
    }
}

impl FromStr for InstallerType {
    type Err = FlameError;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "python" => Ok(InstallerType::Python),
            _ => Err(FlameError::InvalidConfig(format!("Unknown installer type: {}", s))),
        }
    }
}

/// Python installer implementation
pub struct PythonInstaller;

impl PythonInstaller {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Installer for PythonInstaller {
    async fn install(
        &self,
        app_name: &str,
        src_path: &Path,
        flame_home: &Path,
    ) -> Result<HashMap<String, String>, FlameError> {
        // Implementation detailed below
    }
    
    fn name(&self) -> &'static str {
        "python"
    }
}

/// Installation state for an application
#[derive(Clone, Debug)]
pub enum InstallState {
    NotInstalled,
    Installing,
    Installed,
    Failed(String),  // Error message
}

/// Installed application metadata
pub struct AppInstaller {
    pub name: String,
    pub installer_type: InstallerType,
    pub state: InstallState,
    pub install_path: PathBuf,
    pub env_vars: HashMap<String, String>,
    pub installed_at: Option<DateTime<Utc>>,
}

/// Application manager singleton
pub struct ApplicationManager {
    /// Per-application installation state with lock
    apps: MutexPtr<HashMap<String, Arc<RwLock<AppInstaller>>>>,
    /// FLAME_HOME directory
    flame_home: PathBuf,
}

impl ApplicationManager {
    /// Ensure application is installed, returning environment variables
    pub async fn install(
        &self,
        app: &ApplicationContext,
    ) -> Result<HashMap<String, String>, FlameError>;
    
    /// Check if application is installed
    pub fn is_installed(&self, app_name: &str) -> bool;
    
    /// Get environment variables for installed application
    pub fn get_env_vars(&self, app_name: &str) -> Option<HashMap<String, String>>;
}
```

### Algorithms

**Installation Flow:**

```rust
pub async fn install(&self, app: &ApplicationContext) -> Result<HashMap<String, String>, FlameError> {
    // 0. Skip if no installer configured (WASM apps, pre-installed apps)
    let installer_type = match &app.installer {
        None => {
            tracing::debug!("No installer configured for app <{}>, skipping", app.name);
            return Ok(HashMap::new());
        }
        Some(installer_str) => installer_str.parse::<InstallerType>()?,
    };
    
    // 1. Fast path: check if already installed
    {
        let apps = lock_ptr!(self.apps)?;
        if let Some(installed) = apps.get(&app.name) {
            let installed = installed.read().await;
            if installed.state == InstallState::Installed {
                return Ok(installed.env_vars.clone());
            }
        }
    }
    
    // 2. Acquire or create per-app entry
    let app_entry = {
        let mut apps = lock_ptr!(self.apps)?;
        apps.entry(app.name.clone())
            .or_insert_with(|| Arc::new(RwLock::new(AppInstaller::new(&app.name, installer_type.clone()))))
            .clone()
    };
    
    // 3. Acquire write lock (blocks concurrent installers)
    let mut installed = app_entry.write().await;
    
    // 4. Double-check after acquiring lock
    if installed.state == InstallState::Installed {
        return Ok(installed.env_vars.clone());
    }
    
    if let InstallState::Failed(msg) = &installed.state {
        return Err(FlameError::Internal(msg.clone()));
    }
    
    // 5. Mark as installing
    installed.state = InstallState::Installing;
    
    // 6. Download package from url
    let url = app.url.as_ref()
        .ok_or_else(|| FlameError::InvalidConfig("installer requires url".to_string()))?;
    let package_path = self.download_package(url, &app.name).await?;
    
    // 7. Extract archive
    let src_path = self.flame_home.join("data/apps").join(&app.name).join("src");
    self.extract_package(&package_path, &src_path)?;
    
    // 8. Create installer and run installation via trait
    let installer = installer_type.create_installer();
    tracing::info!("Running {} installer for app <{}>", installer.name(), app.name);
    let env_vars = installer.install(&app.name, &src_path, &self.flame_home).await?;
    
    // 9. Update state
    installed.state = InstallState::Installed;
    installed.install_path = src_path;
    installed.env_vars = env_vars.clone();
    installed.installed_at = Some(Utc::now());
    
    Ok(env_vars)
}
```

**PythonInstaller Implementation:**

```rust
#[async_trait]
impl Installer for PythonInstaller {
    async fn install(
        &self,
        app_name: &str,
        src_path: &Path,
        flame_home: &Path,
    ) -> Result<HashMap<String, String>, FlameError> {
        let deps_path = flame_home.join("data/apps").join(app_name).join("deps");
        let cache_path = flame_home.join("data/cache/uv");
        let log_path = flame_home.join("logs/install").join(format!("{}.log", app_name));
        
        // Ensure directories exist
        fs::create_dir_all(&deps_path)?;
        fs::create_dir_all(&cache_path)?;
        fs::create_dir_all(log_path.parent().unwrap())?;
        
        // Build uv command
        let uv_path = flame_home.join("bin/uv");
        let uv_cmd = if uv_path.exists() {
            uv_path.to_string_lossy().to_string()
        } else {
            "uv".to_string()  // Fall back to system uv
        };
        
        let mut cmd = tokio::process::Command::new(&uv_cmd);
        cmd.args([
            "pip", "install",
            "--target", deps_path.to_str().unwrap(),
            ".",
        ])
        .current_dir(src_path)
        .env("UV_CACHE_DIR", &cache_path);
        
        // Run with output to log file
        let log_file = fs::File::create(&log_path)?;
        let output = cmd
            .stdout(std::process::Stdio::from(log_file.try_clone()?))
            .stderr(std::process::Stdio::from(log_file))
            .output()
            .await?;
        
        if !output.status.success() {
            return Err(FlameError::Internal(format!(
                "Python installation failed for app <{}>. See log: {}",
                app_name, log_path.display()
            )));
        }
        
        // Compute environment variables
        let mut env_vars = HashMap::new();
        
        // PYTHONPATH: include deps, src, and base flame lib directories
        // Order matters - app-specific paths first, then system paths
        //   1. deps/ - app-specific installed dependencies
        //   2. src/  - user's package source
        //   3. $FLAME_HOME/lib/python3.x/site-packages/ - flamepy and base deps (avoid re-download)
        let base_site_packages = Self::find_base_site_packages(flame_home);
        let mut python_paths = vec![
            deps_path.to_string_lossy().to_string(),
            src_path.to_string_lossy().to_string(),
        ];
        if let Some(base_sp) = base_site_packages {
            python_paths.push(base_sp);
        }
        env_vars.insert("PYTHONPATH".to_string(), python_paths.join(":"));
        
        // LD_LIBRARY_PATH for native extensions (.so files)
        let ld_paths = Self::find_native_lib_paths(&deps_path);
        if !ld_paths.is_empty() {
            env_vars.insert("LD_LIBRARY_PATH".to_string(), ld_paths.join(":"));
        }
        
        tracing::info!(
            "Python installation completed for app <{}>: PYTHONPATH={}, LD_LIBRARY_PATH={}",
            app_name,
            env_vars.get("PYTHONPATH").unwrap_or(&String::new()),
            env_vars.get("LD_LIBRARY_PATH").unwrap_or(&String::new())
        );
        
        Ok(env_vars)
    }
    
    fn name(&self) -> &'static str {
        "python"
    }
}

impl PythonInstaller {
    /// Find base site-packages directory under $FLAME_HOME/lib/
    /// This contains flamepy and common dependencies installed by flmadm
    fn find_base_site_packages(flame_home: &Path) -> Option<String> {
        let lib_path = flame_home.join("lib");
        if !lib_path.exists() {
            return None;
        }
        
        // Look for python3.x/site-packages directory
        for entry in fs::read_dir(&lib_path).ok()?.flatten() {
            let python_dir = entry.path();
            if python_dir.is_dir() && entry.file_name().to_string_lossy().starts_with("python") {
                let site_packages = python_dir.join("site-packages");
                if site_packages.exists() {
                    return Some(site_packages.to_string_lossy().to_string());
                }
            }
        }
        None
    }
    
    /// Find directories containing .so files (native extensions)
    fn find_native_lib_paths(deps_path: &Path) -> Vec<String> {
        let mut paths = HashSet::new();
        
        fn scan_dir(dir: &Path, paths: &mut HashSet<String>, depth: usize) {
            if depth > 4 { return; }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_dir(&path, paths, depth + 1);
                    } else if path.extension().map(|e| e == "so").unwrap_or(false) {
                        if let Some(parent) = path.parent() {
                            paths.insert(parent.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
        
        scan_dir(deps_path, &mut paths, 0);
        paths.into_iter().collect()
    }
}
```

### System Considerations

**Performance:**
- First executor pays installation cost; subsequent executors have O(1) lookup
- Per-application lock minimizes contention (different apps install in parallel)
- Package download occurs once per application per node

**Scalability:**
- Horizontal: Each executor-manager node maintains independent ApplicationManager
- Vertical: HashMap with RwLock supports thousands of applications
- Memory: ~1KB per installed application metadata

**Reliability:**
- Failed installations are cached; retry requires application re-registration
- Waiters are notified even on failure to prevent indefinite blocking
- Installation timeout prevents hanging on network issues

**Resource Usage:**
- Disk: Packages stored in `$FLAME_HOME/data/apps/{app_name}/`
- Memory: Minimal (metadata only, not package contents)
- CPU: Installation runs subprocess; no sustained CPU usage

**Security:**
- Package URLs validated before download
- Installation runs with executor-manager process permissions
- Only predefined installer types (python) are supported; no arbitrary command execution

**Observability:**
- Logging: Installation start/complete/failure with timing
- Metrics: `app_install_total`, `app_install_duration_seconds`, `app_install_failures_total`
- Tracing: Span per installation with app name and URL

**Operational:**
- Installation logs written to `$FLAME_HOME/logs/install/{app_name}.log`
- Failed installations can be retried by updating application
- No automatic cleanup; manual cleanup via `flmadm` commands (future)

### Dependencies

**External Dependencies:**
- `reqwest` (existing): HTTP client for package download
- `flate2`, `tar` (existing): Archive extraction
- `tokio::process`: Subprocess execution for installer

**Internal Dependencies:**
- `common::apis::ApplicationContext`: Extended with `installer` field
- `executor_manager::shims`: Receives environment variables
- `stdng::MutexPtr`: Locking primitives

### Code Changes

**New Files:**
- `executor_manager/src/app_manager/mod.rs` - ApplicationManager, InstallState, AppInstaller
- `executor_manager/src/app_manager/installer.rs` - Installer trait definition
- `executor_manager/src/app_manager/python.rs` - PythonInstaller implementation

**Modified Files:**

`rpc/protos/types.proto`:
```protobuf
// Extend ApplicationSpec
message ApplicationSpec {
  // ... existing fields 1-12 ...
  optional string installer = 13;  // Installer type: "python"
}
```

`common/src/apis/types.rs`:
```rust
// Extend Application and ApplicationContext
pub struct Application {
    // ... existing fields ...
    pub installer: Option<String>,  // e.g., "python"
}

pub struct ApplicationContext {
    // ... existing fields ...
    pub installer: Option<String>,
}
```

`executor_manager/src/states/idle.rs`:
```rust
let shim_ptr = shims::new_ptr(&self.executor, Some(&ssn.application))?;
shim_ptr
    .lock()
    .await
    .create_service(self.app_manager.clone())
    .await?;
```

`executor_manager/src/shims/host_shim.rs`:
```rust
impl Shim for HostShim {
    async fn create_service(
        &mut self,
        app_manager: Arc<ApplicationManager>,
    ) -> Result<(), FlameError> {
        let install_env_vars = app_manager.install(&self.app).await?;
        // Create the work directory and launch with install_env_vars.
    }
}
```

`sdk/python/src/flamepy/runner/runpy.py`:
```python
# Remove _install_package_from_url() method entirely
# Simplify on_session_enter():
def on_session_enter(self, context: SessionContext) -> bool:
    # Package is already installed by executor manager
    # Skip: self._install_package_from_url(context.application.url)
    
    # Continue with existing logic...
    common_data_bytes = context.common_data()
    # ...
```

## 4. Use Cases

### Use Case 1: First Executor Binds to Python Application

**Description:** First executor binds to a session with a new Python application that requires package installation.

**Step-by-step workflow:**
1. Client creates session with application "my-ml-app" (`installer: python`, `url` configured)
2. Executor becomes idle and calls `bind_executor()`
3. Session manager returns `SessionContext` with `ApplicationContext` containing installer type and URL
4. `IdleState::execute()` constructs `HostShim` and calls `create_service(app_manager)`
5. `HostShim::create_service()` calls ApplicationManager, which:
   - Parses `installer: python` → `InstallerType::Python`
   - Creates entry with `state: Installing`
   - Downloads package from URL
   - Extracts to `$FLAME_HOME/data/apps/my-ml-app/src/`
   - Runs `uv pip install --target $FLAME_HOME/data/apps/my-ml-app/lib .`
   - Computes `PYTHONPATH` and `LD_LIBRARY_PATH`
   - Updates entry with `state: Installed`, stores env_vars
6. HostShim launches the application process with `PYTHONPATH` and `LD_LIBRARY_PATH` set
7. Application starts immediately (packages pre-installed)

**Expected outcome:** Application starts successfully with all dependencies available.

### Use Case 2: Concurrent Executors Bind to Same Application

**Description:** Multiple executors simultaneously bind to sessions using the same application.

**Step-by-step workflow:**
1. Three executors (E1, E2, E3) become idle simultaneously
2. All call `bind_executor()` and receive `SessionContext` for "my-ml-app"
3. All construct HostShim and call `create_service(app_manager)` concurrently
4. ApplicationManager:
   - E1 acquires write lock first, starts installation
   - E2, E3 block on write lock
   - E1 completes installation, releases lock
   - E2 acquires lock, sees `state: Installed`, returns env_vars immediately
   - E3 acquires lock, sees `state: Installed`, returns env_vars immediately
5. All three HostShim instances launch applications with the returned env_vars

**Expected outcome:** Only one installation occurs; all executors get correct env_vars.

### Use Case 3: WASM Application (No Installer)

**Description:** Executor binds to session with WASM application that has no installer configured.

**Step-by-step workflow:**
1. Executor binds to session with "my-wasm-app" (shim: Wasm, no `installer` field)
2. `IdleState::execute()` constructs WasmShim and calls `create_service(app_manager)`
3. WasmShim ignores ApplicationManager and loads the component from `app.command`
4. Application executes tasks directly

**Expected outcome:** No installation; WASM application runs immediately.

### Use Case 4: Pre-installed Host Application (No Installer)

**Description:** Executor binds to session with pre-installed application (e.g., `flmping`).

**Step-by-step workflow:**
1. Executor binds to session with "flmping" application (no `installer`, no `url`)
2. `IdleState::execute()` constructs HostShim and calls `create_service(app_manager)`
3. HostShim calls ApplicationManager, which returns an empty environment because no URL is configured
4. HostShim spawns the system-installed binary
5. Application starts using pre-installed binaries

**Expected outcome:** No installation; application starts immediately.

### Use Case 5: Installation Failure Recovery

**Description:** Package installation fails and subsequent executor attempts.

**Step-by-step workflow:**
1. First executor calls `HostShim::create_service(app_manager)` for an app with `installer: python`
2. Installation fails (e.g., network error, invalid package, uv not found)
3. ApplicationManager stores `state: Failed("uv pip install failed. See log: ...")`
4. Returns the error from `create_service`; the first executor fails binding
5. A second executor calls `HostShim::create_service(app_manager)`
6. ApplicationManager sees `state: Failed`, returns cached error immediately
7. Administrator fixes issue and updates application via `flmctl apply`
8. Session manager notifies executor manager of application update
9. ApplicationManager clears cached state for this application
10. Next executor successfully installs and binds

**Expected outcome:** Failed state is cached; recovery requires application update.

## 5. References

### Related Documents

- [RFE420 GitHub Issue](https://github.com/xflops/flame/issues/420)
- [RFE284-flmrun Design](../RFE284-flmrun/RFE284-flmrun.md) - Original runner package design
- [RFE280-runner Working Directory](../RFE280-runner/runner-working-directory-change.md) - Working directory conventions

### Implementation References

- Current installation logic: `sdk/python/src/flamepy/runner/runpy.py` lines 152-281
- Executor state machine: `executor_manager/src/states/`
- Shim creation: `executor_manager/src/shims/mod.rs`
- Application types: `common/src/apis/types.rs`
- Proto definitions: `rpc/protos/types.proto`
- flmadm installation: `flmadm/src/managers/installation.rs`

### External References

- [uv documentation](https://github.com/astral-sh/uv)

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct SystemdManager;

impl SystemdManager {
    pub fn new() -> Self {
        Self
    }

    /// Generate and install systemd service files
    pub fn install_services(&self, prefix: &Path) -> Result<()> {
        println!("⚙️  Installing systemd service files...");

        let prefix_str = prefix.to_str().unwrap();

        // Generate service files
        let fsm_service = self.generate_session_manager_service(prefix_str);
        let fem_service = self.generate_executor_manager_service(prefix_str);

        // Write to /etc/systemd/system/
        let fsm_path = PathBuf::from("/etc/systemd/system/flame-session-manager.service");
        let fem_path = PathBuf::from("/etc/systemd/system/flame-executor-manager.service");

        fs::write(&fsm_path, fsm_service)
            .context("Failed to write flame-session-manager.service")?;
        fs::write(&fem_path, fem_service)
            .context("Failed to write flame-executor-manager.service")?;

        // Reload systemd daemon
        self.daemon_reload()?;

        println!("✓ Installed systemd service files");
        Ok(())
    }

    /// Remove systemd service files
    pub fn remove_services(&self) -> Result<()> {
        println!("🗑️  Removing systemd service files...");

        let fsm_path = PathBuf::from("/etc/systemd/system/flame-session-manager.service");
        let fem_path = PathBuf::from("/etc/systemd/system/flame-executor-manager.service");

        // Stop services first
        let _ = self.stop_service("flame-executor-manager");
        let _ = self.stop_service("flame-session-manager");

        // Disable services
        let _ = self.disable_service("flame-executor-manager");
        let _ = self.disable_service("flame-session-manager");

        // Remove service files
        if fsm_path.exists() {
            fs::remove_file(&fsm_path).context("Failed to remove flame-session-manager.service")?;
        }
        if fem_path.exists() {
            fs::remove_file(&fem_path)
                .context("Failed to remove flame-executor-manager.service")?;
        }

        // Reload systemd daemon
        self.daemon_reload()?;

        println!("✓ Removed systemd service files");
        Ok(())
    }

    /// Enable and start systemd services
    pub fn enable_and_start_services(&self) -> Result<()> {
        println!("🚀 Enabling and starting Flame services...");

        // Enable services
        self.enable_service("flame-session-manager")?;
        self.enable_service("flame-executor-manager")?;

        // Start session manager first
        self.start_service("flame-session-manager")?;

        // Wait a bit for session manager to be ready
        std::thread::sleep(std::time::Duration::from_secs(2));

        // Start executor manager
        self.start_service("flame-executor-manager")?;

        // Verify services are running
        std::thread::sleep(std::time::Duration::from_secs(1));
        self.check_service_status("flame-session-manager")?;
        self.check_service_status("flame-executor-manager")?;

        println!("✓ Services are running");
        Ok(())
    }

    fn daemon_reload(&self) -> Result<()> {
        let output = Command::new("systemctl")
            .arg("daemon-reload")
            .output()
            .context("Failed to reload systemd daemon")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to reload systemd daemon: {}", stderr);
        }

        Ok(())
    }

    fn enable_service(&self, service: &str) -> Result<()> {
        let output = Command::new("systemctl")
            .args(["enable", service])
            .output()
            .context(format!("Failed to enable {}", service))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to enable {}: {}", service, stderr);
        }

        Ok(())
    }

    fn disable_service(&self, service: &str) -> Result<()> {
        let _output = Command::new("systemctl")
            .args(["disable", service])
            .output()
            .context(format!("Failed to disable {}", service))?;

        // Ignore errors for disable (service might not be enabled)
        Ok(())
    }

    fn start_service(&self, service: &str) -> Result<()> {
        let output = Command::new("systemctl")
            .args(["start", service])
            .output()
            .context(format!("Failed to start {}", service))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to start {}: {}", service, stderr);
        }

        println!("✓ Started {}", service);
        Ok(())
    }

    fn stop_service(&self, service: &str) -> Result<()> {
        let output = Command::new("systemctl")
            .args(["stop", service])
            .output()
            .context(format!("Failed to stop {}", service))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Don't fail if service wasn't running
            if !stderr.contains("not loaded") {
                println!("⚠️  Warning: Failed to stop {}: {}", service, stderr);
            }
        } else {
            println!("✓ Stopped {}", service);
        }

        Ok(())
    }

    fn check_service_status(&self, service: &str) -> Result<()> {
        let output = Command::new("systemctl")
            .args(["is-active", service])
            .output()
            .context(format!("Failed to check {} status", service))?;

        let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if status == "active" {
            println!("✓ {} is active", service);
            Ok(())
        } else {
            anyhow::bail!("{} is not active (status: {})", service, status);
        }
    }

    fn generate_session_manager_service(&self, prefix: &str) -> String {
        format!(
            r#"[Unit]
Description=Flame Session Manager
Documentation=https://github.com/xflops-io/flame
After=network.target
Wants=network-online.target

[Service]
Type=simple
User=flame
Group=flame
Environment="RUST_LOG=info"
WorkingDirectory={prefix}/work
ExecStart={prefix}/bin/flame-session-manager --config {prefix}/conf/flame-cluster.yaml
StandardOutput=append:{prefix}/logs/fsm.log
StandardError=append:{prefix}/logs/fsm.log
Restart=on-failure
RestartSec=5s
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"#,
            prefix = prefix
        )
    }

    fn generate_executor_manager_service(&self, prefix: &str) -> String {
        format!(
            r#"[Unit]
Description=Flame Executor Manager
Documentation=https://github.com/xflops-io/flame
After=network.target flame-session-manager.service
Wants=network-online.target
Requires=flame-session-manager.service

[Service]
Type=simple
User=flame
Group=flame
Environment="RUST_LOG=info"
WorkingDirectory={prefix}/work
ExecStart={prefix}/bin/flame-executor-manager --config {prefix}/conf/flame-cluster.yaml
StandardOutput=append:{prefix}/logs/fem.log
StandardError=append:{prefix}/logs/fem.log
Restart=on-failure
RestartSec=5s
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"#,
            prefix = prefix
        )
    }
}

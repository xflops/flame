use anyhow::{Context, Result};
use std::process::Command;

pub struct UserManager;

impl UserManager {
    pub fn new() -> Self {
        Self
    }

    /// Check if the flame user exists
    pub fn user_exists(&self) -> Result<bool> {
        let output = Command::new("id")
            .arg("flame")
            .output()
            .context("Failed to check if flame user exists")?;

        Ok(output.status.success())
    }

    /// Create the flame user and group for system installation
    pub fn create_user(&self) -> Result<()> {
        if self.user_exists()? {
            println!("✓ Flame user already exists");
            return Ok(());
        }

        println!("👤 Creating flame user and group...");

        // Create flame group
        let output = Command::new("groupadd")
            .args(["--system", "flame"])
            .output()
            .context("Failed to create flame group")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Group might already exist, check if that's the error
            if !stderr.contains("already exists") {
                anyhow::bail!("Failed to create flame group: {}", stderr);
            }
        }

        // Create flame user
        let output = Command::new("useradd")
            .args([
                "--system",
                "--no-create-home",
                "--gid",
                "flame",
                "--shell",
                "/usr/sbin/nologin",
                "flame",
            ])
            .output()
            .context("Failed to create flame user")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create flame user: {}", stderr);
        }

        println!("✓ Created flame user and group");
        Ok(())
    }

    /// Remove the flame user and group
    pub fn remove_user(&self, force: bool) -> Result<()> {
        if !self.user_exists()? {
            return Ok(());
        }

        // Check for running processes
        if !force {
            let output = Command::new("ps")
                .args(["-U", "flame", "-o", "pid,cmd", "--no-headers"])
                .output()
                .context("Failed to check for flame user processes")?;

            let processes = String::from_utf8_lossy(&output.stdout);
            if !processes.trim().is_empty() {
                println!("⚠️  Warning: flame user has running processes:");
                println!("{}", processes);
                println!("   User was not removed. Stop these processes first.");
                return Ok(());
            }
        }

        println!("🗑️  Removing flame user and group...");

        // Remove user
        let output = Command::new("userdel")
            .arg("flame")
            .output()
            .context("Failed to remove flame user")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("⚠️  Warning: Failed to remove flame user: {}", stderr);
        }

        // Remove group
        let output = Command::new("groupdel")
            .arg("flame")
            .output()
            .context("Failed to remove flame group")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("⚠️  Warning: Failed to remove flame group: {}", stderr);
        } else {
            println!("✓ Removed flame user and group");
        }

        Ok(())
    }

    /// Check if we're running as root
    pub fn is_root(&self) -> bool {
        users::get_current_uid() == 0
    }

    /// Set ownership of a path to flame:flame
    pub fn set_ownership(&self, path: &std::path::Path) -> Result<()> {
        if !self.is_root() {
            return Ok(()); // Skip if not root
        }

        let output = Command::new("chown")
            .args(["-R", "flame:flame", path.to_str().unwrap()])
            .output()
            .context("Failed to set ownership")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to set ownership: {}", stderr);
        }

        Ok(())
    }
}

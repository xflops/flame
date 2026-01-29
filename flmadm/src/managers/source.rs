use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub struct SourceManager {
    temp_dir: Option<tempfile::TempDir>,
}

impl SourceManager {
    pub fn new() -> Self {
        Self { temp_dir: None }
    }

    /// Get or prepare the source directory
    /// If src_dir is provided, validate and use it
    /// Otherwise, clone from GitHub
    pub fn prepare_source(&mut self, src_dir: Option<PathBuf>) -> Result<PathBuf> {
        match src_dir {
            Some(dir) => self.validate_source_dir(dir),
            None => self.clone_from_github(),
        }
    }

    fn validate_source_dir(&self, dir: PathBuf) -> Result<PathBuf> {
        if !dir.exists() {
            anyhow::bail!("Source directory does not exist: {:?}", dir);
        }

        let cargo_toml = dir.join("Cargo.toml");
        if !cargo_toml.exists() {
            anyhow::bail!(
                "Not a valid Flame source directory (missing Cargo.toml): {:?}",
                dir
            );
        }

        // Check for key components
        for component in [
            "session_manager",
            "executor_manager",
            "flmctl",
            "sdk/python",
        ] {
            let component_path = dir.join(component);
            if !component_path.exists() {
                anyhow::bail!("Missing required component: {}", component);
            }
        }

        println!("✓ Using source directory: {}", dir.display());
        Ok(dir)
    }

    fn clone_from_github(&mut self) -> Result<PathBuf> {
        println!("📥 Cloning Flame from GitHub (main branch)...");

        // Check if git is available
        which::which("git")
            .context("git command not found. Please install git or provide --src-dir")?;

        // Create temporary directory
        let temp_dir = tempfile::tempdir().context("Failed to create temporary directory")?;

        let clone_path = temp_dir.path().to_path_buf();

        // Clone the repository
        let output = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--branch",
                "main",
                "https://github.com/xflops-io/flame.git",
                clone_path.to_str().unwrap(),
            ])
            .output()
            .context("Failed to execute git clone")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Git clone failed: {}", stderr);
        }

        println!("✓ Successfully cloned Flame repository");

        self.temp_dir = Some(temp_dir);
        Ok(clone_path)
    }
}

impl Drop for SourceManager {
    fn drop(&mut self) {
        if self.temp_dir.is_some() {
            println!("🧹 Cleaning up temporary source directory");
        }
    }
}

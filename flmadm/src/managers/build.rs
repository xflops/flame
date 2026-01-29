use crate::types::BuildArtifacts;
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct BuildManager {
    verbose: bool,
}

impl BuildManager {
    pub fn new(verbose: bool) -> Self {
        Self { verbose }
    }

    /// Build all Flame binaries from source
    pub fn build_all(&self, src_dir: &Path) -> Result<BuildArtifacts> {
        // Check if cargo is available
        which::which("cargo").context("cargo command not found. Please install Rust toolchain")?;

        println!("🔨 Building Flame components...");
        let start = Instant::now();

        if self.verbose {
            self.build_verbose(src_dir)?;
        } else {
            self.build_with_progress(src_dir)?;
        }

        let duration = start.elapsed();
        println!("✓ Build completed in {:.1}s", duration.as_secs_f64());

        // Return build artifacts
        BuildArtifacts::from_source_dir(&src_dir.to_path_buf(), "release")
    }

    fn build_verbose(&self, src_dir: &Path) -> Result<()> {
        println!("Building in release mode (verbose output)...\n");

        let output = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(src_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()
            .context("Failed to execute cargo build")?;

        if !output.status.success() {
            anyhow::bail!("Build failed. Check output above for details.");
        }

        Ok(())
    }

    fn build_with_progress(&self, src_dir: &Path) -> Result<()> {
        // Create a progress bar
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );
        pb.set_message("Building Flame components...");

        // Start the build process
        let output = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(src_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Failed to execute cargo build")?;

        pb.finish_with_message("Build completed");

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Show last 30 lines of error output
            let error_lines: Vec<&str> = stderr.lines().collect();
            let show_lines = error_lines.iter().rev().take(30).rev();

            eprintln!("\n❌ Build failed. Last 30 lines of output:\n");
            for line in show_lines {
                eprintln!("{}", line);
            }

            anyhow::bail!("\nBuild failed. Run with --verbose for full output.");
        }

        Ok(())
    }

    /// Verify that required build tools are available
    pub fn check_prerequisites(&self) -> Result<()> {
        // Check for cargo
        if which::which("cargo").is_err() {
            anyhow::bail!(
                "cargo not found. Please install Rust toolchain:\n  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
            );
        }

        // Check Rust version
        let output = Command::new("cargo")
            .args(["--version"])
            .output()
            .context("Failed to check cargo version")?;

        let version = String::from_utf8_lossy(&output.stdout);
        println!("✓ Found {}", version.trim());

        Ok(())
    }
}

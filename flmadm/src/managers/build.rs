use anyhow::Result;
use std::path::Path;

pub struct BuildManager;

impl BuildManager {
    pub fn new(_verbose: bool) -> Self {
        Self
    }

    pub fn check_prerequisites(&self) -> Result<()> {
        Ok(())
    }

    pub fn build_all(&self, _src_dir: &Path) -> Result<crate::types::BuildArtifacts> {
        Err(anyhow::anyhow!("build_all not implemented in stub"))
    }
}

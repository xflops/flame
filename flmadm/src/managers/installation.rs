use crate::types::{BuildArtifacts, InstallationPaths};
use anyhow::Result;

pub struct InstallationManager;

impl InstallationManager {
    pub fn new() -> Self {
        Self {}
    }
    pub fn create_directories(&self, _paths: &InstallationPaths) -> Result<()> {
        Ok(())
    }
    pub fn install_binaries(
        &self,
        _artifacts: &BuildArtifacts,
        _paths: &InstallationPaths,
        _profiles: &Vec<crate::types::InstallProfile>,
        _force: bool,
    ) -> Result<()> {
        Ok(())
    }
    pub fn install_uv(
        &self,
        _paths: &InstallationPaths,
        _profiles: &Vec<crate::types::InstallProfile>,
    ) -> Result<()> {
        Ok(())
    }
    pub fn install_python_sdk(
        &self,
        _src_dir: &std::path::Path,
        _paths: &InstallationPaths,
        _profiles: &Vec<crate::types::InstallProfile>,
        _force: bool,
    ) -> Result<()> {
        Ok(())
    }
    pub fn install_migrations(
        &self,
        _src_dir: &std::path::Path,
        _paths: &InstallationPaths,
        _profiles: &Vec<crate::types::InstallProfile>,
    ) -> Result<()> {
        Ok(())
    }
    pub fn remove_installation(
        &self,
        _paths: &InstallationPaths,
        _keep_data: bool,
        _keep_config: bool,
        _keep_logs: bool,
    ) -> Result<()> {
        Ok(())
    }
}

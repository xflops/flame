use crate::types::InstallProfile;
use anyhow::Result;

pub struct SystemdManager;

impl SystemdManager {
    pub fn new() -> Self {
        Self {}
    }
    pub fn remove_services(&self) -> Result<()> {
        Ok(())
    }
    pub fn install_services(
        &self,
        _prefix: &std::path::PathBuf,
        _profiles: &[InstallProfile],
    ) -> Result<()> {
        Ok(())
    }
    pub fn enable_and_start_services(&self, _profiles: &[InstallProfile]) -> Result<()> {
        Ok(())
    }
}

use crate::types::InstallationPaths;
use anyhow::Result;
use std::path::PathBuf;

pub struct BackupManager;

impl BackupManager {
    pub fn new() -> Self {
        Self {}
    }

    pub fn backup_for_clean_install(&self, _paths: &InstallationPaths) -> Result<PathBuf> {
        Ok(PathBuf::from("/tmp/flame-backup"))
    }
    pub fn create_backup(
        &self,
        _paths: &crate::types::InstallationPaths,
        _backup_dir: Option<PathBuf>,
        _preserve_data: bool,
        _preserve_config: bool,
        _preserve_logs: bool,
    ) -> Result<PathBuf> {
        Ok(PathBuf::from("/tmp/flame-backup"))
    }
}

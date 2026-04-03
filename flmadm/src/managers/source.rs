use anyhow::Result;
use std::path::PathBuf;

pub struct SourceManager;

impl SourceManager {
    pub fn new() -> Self {
        Self {}
    }
    pub fn prepare_source(&mut self, src_dir: Option<PathBuf>) -> Result<PathBuf> {
        Ok(src_dir.unwrap_or_else(|| PathBuf::from(".")))
    }
}

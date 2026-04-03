use anyhow::Result;

pub struct ConfigGenerator;

impl ConfigGenerator {
    pub fn new() -> Self {
        Self {}
    }
    pub fn generate_config(&self, _prefix: &std::path::PathBuf) -> Result<()> {
        Ok(())
    }
}

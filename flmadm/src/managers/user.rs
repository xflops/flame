pub struct UserManager;

impl UserManager {
    pub fn new() -> Self {
        Self {}
    }
    pub fn is_root(&self) -> bool {
        true
    }
}

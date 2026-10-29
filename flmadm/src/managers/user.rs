pub struct UserManager;

impl UserManager {
    pub fn new() -> Self {
        Self
    }

    /// Check if we're running as root
    pub fn is_root(&self) -> bool {
        nix::unistd::getuid().is_root()
    }
}

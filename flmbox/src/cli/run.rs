/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! `flmbox run`.

use flame_rs::apis::FlameError;
use sha2::{Digest, Sha256};

use crate::sandbox::{Sandbox, DEFAULT_IMAGE};

#[derive(clap::Args)]
pub struct Args {
    /// Sandbox id; generated when absent
    #[arg(long)]
    name: Option<String>,
    /// Image reference
    #[arg(default_value = DEFAULT_IMAGE)]
    image: String,
}

/// Creates and starts the sandbox and prints its id.
pub async fn run(sandbox: &Sandbox, args: Args) -> Result<(), FlameError> {
    let id = args.name.unwrap_or_else(generated_id);
    sandbox.create(&id, &args.image).await?;
    println!("{id}");
    Ok(())
}

/// `box-` and eight hex digits of a hash of the clock and the process id.
fn generated_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{now}-{}", std::process::id()));
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("box-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        let (a, b) = (generated_id(), generated_id());
        assert!(a.starts_with("box-") && a.len() == 12);
        assert!(crate::sandbox::validate_id(&a).is_ok());
        assert_ne!(a, b);
    }
}

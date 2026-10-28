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

//! `flmbox pull`.

use flame_rs::apis::FlameError;

use crate::images::PullPolicy;
use crate::sandbox::Sandbox;

#[derive(clap::Args)]
pub struct Args {
    /// Image reference, e.g. ubuntu:26.04 or ghcr.io/org/img@sha256:...
    image: String,

    /// When to ask the registry for an image that is already cached
    #[arg(long, value_enum, default_value_t = PullPolicy::Always)]
    policy: PullPolicy,
}

/// Prints the digest of the pulled image; the progress goes to stderr.
pub async fn run(sandbox: &Sandbox, args: Args) -> Result<(), FlameError> {
    let root = sandbox.pull(&args.image, args.policy).await?;
    println!("{}", root.digest);
    Ok(())
}

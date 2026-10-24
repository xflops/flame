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

//! `flmbox kill`.

use flame_rs::apis::FlameError;

use super::require;
use crate::sandbox::Sandbox;

#[derive(clap::Args)]
pub struct Args {
    /// Sandbox id
    id: String,
}

pub async fn run(sandbox: &Sandbox, args: Args) -> Result<(), FlameError> {
    require(sandbox, &args.id).await?;
    sandbox.kill(&args.id).await
}

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

//! The `flmbox` service binary.

use std::sync::Arc;

use flame_rs as flame;
use flmbox::config::Config;
use flmbox::image::ImageStore;
use flmbox::network::network_for;
use flmbox::runner::TokioRunner;
use flmbox::sandbox::{executor_id_from_env, SandboxManager};
use flmbox::service::FlmboxService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env()?;
    let executor_id = executor_id_from_env();

    // Created once per executor and kept across session bindings.
    let images = Arc::new(ImageStore::new(&cfg));
    let runner = Arc::new(TokioRunner);
    let network = network_for(&cfg, runner.clone(), cfg.state_dir.clone());
    let manager = SandboxManager::new(cfg, &executor_id, images, runner, network);
    if let Err(e) = manager.cleanup_stale().await {
        tracing::warn!("failed to clean up stale sandboxes: {e}");
    }

    flame::run(FlmboxService::new(manager)).await?;

    tracing::debug!("flmbox service was stopped.");

    Ok(())
}

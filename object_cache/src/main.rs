/*
Copyright 2025 The Flame Authors.
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

use std::path::PathBuf;

use clap::Parser;

use common::ctx::FlameClusterContext;

mod cache;
mod eviction;
mod storage;

#[derive(Parser)]
#[command(name = "flame-object-cache")]
#[command(about = "Standalone object cache server for Flame")]
struct Args {
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log_guard = common::init_logger(Some("foc"))?;

    let args = Args::parse();

    let config_path = args.config.map(|p| p.to_string_lossy().to_string());
    tracing::info!(
        "Loading config from: {:?}",
        config_path.as_deref().unwrap_or("default locations")
    );
    let ctx = FlameClusterContext::from_file(config_path)?;

    let cache_config = ctx
        .cache
        .ok_or("Cache configuration not found in config file")?;

    tracing::info!("Starting flame-object-cache server");
    tracing::info!("Endpoint: {}", cache_config.endpoint);
    if let Some(ref storage) = cache_config.storage {
        tracing::info!("Storage: {}", storage);
    }

    cache::run(&cache_config).await?;

    Ok(())
}

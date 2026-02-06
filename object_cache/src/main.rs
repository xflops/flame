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

use clap::Parser;
use serde_derive::{Deserialize, Serialize};

use common::ctx::FlameCache;
use common::FlameError;
use flame_cache;

const DEFAULT_FLAME_CACHE_ENDPOINT: &str = "grpc://127.0.0.1:9090";
const DEFAULT_FLAME_CACHE_NETWORK_INTERFACE: &str = "eth0";

#[derive(Parser)]
#[command(name = "flame-object-cache")]
#[command(author = "XFLOPS <support@xflops.io>")]
#[command(version = "0.1.0")]
#[command(about = "Flame Object Cache Service", long_about = None)]
struct Cli {
    /// Configuration file path (YAML format)
    #[arg(long, short = 'c')]
    config: Option<String>,

    /// Cache endpoint (e.g., grpc://127.0.0.1:9090)
    #[arg(long)]
    endpoint: Option<String>,

    /// Network interface name (e.g., eth0)
    #[arg(long)]
    network_interface: Option<String>,

    /// Storage path for persistent cache
    #[arg(long)]
    storage: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheConfigYaml {
    pub endpoint: Option<String>,
    pub network_interface: Option<String>,
    pub storage: Option<String>,
}

fn load_config_from_file(path: &str) -> Result<FlameCache, FlameError> {
    use std::fs;

    if !std::path::Path::new(path).is_file() {
        return Err(FlameError::InvalidConfig(format!("<{path}> is not a file")));
    }

    let contents = fs::read_to_string(path)
        .map_err(|e| FlameError::Internal(format!("Failed to read config file: {}", e)))?;

    let yaml_config: CacheConfigYaml = serde_yaml::from_str(&contents)
        .map_err(|e| FlameError::Internal(format!("Failed to parse YAML: {}", e)))?;

    Ok(FlameCache {
        endpoint: yaml_config
            .endpoint
            .unwrap_or_else(|| DEFAULT_FLAME_CACHE_ENDPOINT.to_string()),
        network_interface: yaml_config
            .network_interface
            .unwrap_or_else(|| DEFAULT_FLAME_CACHE_NETWORK_INTERFACE.to_string()),
        storage: yaml_config.storage,
    })
}

fn build_config_from_cli(cli: &Cli) -> Result<FlameCache, FlameError> {
    // Priority: CLI args > environment variables > defaults
    let endpoint = cli
        .endpoint
        .clone()
        .or_else(|| std::env::var("FLAME_CACHE_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_FLAME_CACHE_ENDPOINT.to_string());

    let network_interface = cli
        .network_interface
        .clone()
        .or_else(|| std::env::var("FLAME_CACHE_NETWORK_INTERFACE").ok())
        .unwrap_or_else(|| DEFAULT_FLAME_CACHE_NETWORK_INTERFACE.to_string());

    let storage = cli
        .storage
        .clone()
        .or_else(|| std::env::var("FLAME_CACHE_STORAGE").ok());

    Ok(FlameCache {
        endpoint,
        network_interface,
        storage,
    })
}

#[tokio::main]
async fn main() -> Result<(), FlameError> {
    // Initialize logger
    let _ = common::init_logger();

    let cli = Cli::parse();

    // Load configuration
    let cache_config = if let Some(config_path) = &cli.config {
        // Load from config file
        tracing::info!("Loading configuration from: {}", config_path);
        load_config_from_file(config_path)?
    } else {
        // Build from CLI args and environment variables
        tracing::info!("Building configuration from CLI arguments and environment variables");
        build_config_from_cli(&cli)?
    };

    tracing::info!(
        "Cache configuration: endpoint={}, network_interface={}, storage={:?}",
        cache_config.endpoint,
        cache_config.network_interface,
        cache_config.storage
    );

    // Run the cache server
    flame_cache::run(&cache_config).await
}

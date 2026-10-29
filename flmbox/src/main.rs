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

//! `flmbox`: sandboxes from the command line, or as a Flame service.
//!
//! ```text
//! flmbox pull ubuntu:26.04
//! flmbox run --name dev ubuntu:26.04
//! flmbox exec dev -- ls -la /
//! flmbox kill dev
//! flmbox -d            # a Flame service instance, started by the host shim
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

mod api;
mod cli;
mod config;
mod images;
mod runtime;
mod sandbox;
mod service;

#[cfg(test)]
mod testutil;

use crate::cli::Cli;
use crate::config::FlameSandbox;
use crate::images::{parse_reference, ImageStore};
use crate::sandbox::{Sandbox, DEFAULT_IMAGE};
use crate::service::{executor_id_from_env, FlmboxService};
use flame_rs as flame;
use flame_rs::perf_log;

/// Compiles the image-reference grammar on another thread.
///
/// The grammar is a large regular expression that the first parse in a process compiles, which takes
/// tens of milliseconds. Left to the first sandbox, that delay lands on its creation; started here it
/// overlaps the service's startup and the wait for the first session.
fn warm_up_reference_parser() {
    std::thread::spawn(|| {
        let _ = parse_reference(DEFAULT_IMAGE);
    });
}

#[tokio::main]
async fn main() -> ExitCode {
    warm_up_reference_parser();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flmbox: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Logs only errors unless RUST_LOG asks for more, e.g. RUST_LOG=flame::perf=debug for the
    // performance timeline.
    flame::apis::init_logger()?;
    perf_log!("flmbox_started");
    cli.check()?;
    let mut cfg = FlameSandbox::load(cli.config.as_deref())?;
    if let Some(network) = cli.network {
        cfg.network = network;
    }
    let mut images = ImageStore::new(&cfg);
    if !cli.daemon {
        images = images.with_progress();
    }
    let sandbox = Sandbox::new(cfg, Arc::new(images));

    if cli.daemon {
        daemon(sandbox).await?;
        return Ok(ExitCode::SUCCESS);
    }
    match cli.command {
        Some(command) => Ok(command.run(&sandbox).await?),
        None => Ok(ExitCode::SUCCESS),
    }
}

/// The service: created once per executor and kept across session bindings.
async fn daemon(sandbox: Sandbox) -> Result<(), Box<dyn std::error::Error>> {
    let service = FlmboxService::new(sandbox, &executor_id_from_env());
    let start = std::time::Instant::now();
    service.cleanup_stale().await;
    perf_log!(
        "flmbox_cleanup_stale",
        elapsed_ms = start.elapsed().as_secs_f64() * 1e3,
    );
    perf_log!("flmbox_serving");
    flame::run(service).await?;
    tracing::debug!("flmbox service was stopped.");
    Ok(())
}

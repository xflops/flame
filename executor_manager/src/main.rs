/*
Copyright 2023 The Flame Authors.
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

use common::ctx::FlameClusterContext;
use common::FlameError;

mod appmgr;
mod client;
mod executor;
mod manager;
mod shims;
mod states;
mod stream_handler;

#[derive(Parser)]
#[command(name = "flame-executor-manager")]
#[command(author = "XFLOPS <support@xflops.io>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Executor Manager", long_about = None)]
struct Cli {
    #[arg(long)]
    config: Option<String>,
    #[arg(long)]
    slots: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), FlameError> {
    let _log_guard = common::init_logger(Some("fem"))?;

    let cli = Cli::parse();
    let ctx = FlameClusterContext::from_file(cli.config)?;

    tracing::info!("flame-executor-manager is starting ...");

    if let Some(pprof_config) = &ctx.cluster.pprof {
        let port = pprof_config.port;
        tokio::spawn(async move {
            common::pprof::run_pprof_server(Some(port)).await;
        });
    }

    let result = manager::run(&ctx).await;
    if let Err(e) = &result {
        tracing::error!("Executor manager exited with error: {e}");
    } else {
        tracing::info!("Executor manager exited successfully.");
    }

    tracing::info!("flame-executor-manager stopped.");

    result
}

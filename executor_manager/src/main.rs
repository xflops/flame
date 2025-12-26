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
use futures::future::join_all;
use tokio::runtime::{Builder, Runtime};

use crate::manager::ExecutorManager;
use common::ctx::FlameContext;
use common::FlameError;
use object_cache::cache;

mod client;
mod executor;
mod manager;
mod shims;
mod states;

#[derive(Parser)]
#[command(name = "flame-executor-manager")]
#[command(author = "Klaus Ma <klaus@xflops.cn>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Executor Manager", long_about = None)]
struct Cli {
    #[arg(long)]
    flame_conf: Option<String>,
    #[arg(long)]
    slots: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), FlameError> {
    common::init_logger();

    let cli = Cli::parse();
    let ctx = FlameContext::from_file(cli.flame_conf)?;

    let mut handlers = vec![];
    let build_runtime = |name: &str, threads: usize| -> Result<Runtime, FlameError> {
        Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name(name)
            .enable_all()
            .build()
            .map_err(|e| FlameError::Internal(format!("failed to build runtime <{name}>: {e}")))
    };

    // The manager thread will start one thread for each executor.
    let max_executors = ctx.executors.limits.max_executors as usize;
    let manager_rt = build_runtime("manager", max_executors + 1)?;

    // Start executor manager thread.
    {
        let ctx = ctx.clone();
        let handler = manager_rt.spawn(async move {
            // Create the executor manager by the context.
            let mut manager = ExecutorManager::new(&ctx).await?;
            manager.run().await
        });
        handlers.push(handler);
    }

    // // Start object cache thread.
    // {
    //     let cache_config = ctx.clone().cache;
    //     if let Some(cache_config) = cache_config {
    //         let objcache = cache::new_ptr(&cache_config)?;
    //         objcache.run().await?;
    //     } else {
    //         tracing::warn!("No object cache configuration found, skipping object cache thread.");
    //     }
    // }

    // Waiting for all thread to exit.
    let _ = join_all(handlers).await;

    Ok(())
}

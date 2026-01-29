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

use std::thread;
use std::{future::Future, pin::Pin, task::Context, task::Poll, thread::JoinHandle};

use clap::Parser;
use tokio::runtime::{Builder, Runtime};

use common::ctx::FlameClusterContext;
use common::FlameError;

mod client;
mod executor;
mod manager;
mod shims;
mod states;

// Import cache functionality
use flame_cache;

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
    common::init_logger();

    let cli = Cli::parse();
    let ctx = FlameClusterContext::from_file(cli.config)?;

    let mut handlers = vec![];
    let build_runtime = |name: &str, threads: usize| -> Result<Runtime, FlameError> {
        Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name(name)
            .enable_all()
            .build()
            .map_err(|e| FlameError::Internal(format!("failed to build runtime <{name}>: {e}")))
    };

    // Start the object cache thread if cache configuration is present.
    if let Some(ref cache_config) = ctx.cache {
        let cache_rt = build_runtime("cache", 2)?;
        let cache_config = cache_config.clone();
        let handler = thread::spawn(move || {
            let _ = cache_rt.block_on(async move {
                match flame_cache::run(&cache_config).await {
                    Ok(_) => {
                        tracing::info!("Object cache exited successfully.");
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!("Object cache exited with error: {e}");
                        Err(e)
                    }
                }
            });
        });
        handlers.push(handler);
        tracing::info!("Object cache thread started.");
    } else {
        tracing::info!("No cache configuration found, object cache will not be started.");
    }

    // The manager thread will start one thread for each executor.
    let max_executors = ctx.executors.limits.max_executors as usize;
    let manager_rt = build_runtime("manager", max_executors + 1)?;

    // Start the executor manager thread.
    {
        let ctx = ctx.clone();
        let handler = thread::spawn(move || {
            let _ = manager_rt.block_on(async move {
                match manager::run(&ctx).await {
                    Ok(_) => {
                        tracing::info!("Executor manager exited successfully.");
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!("Executor manager exited with error: {e}");
                        Err(e)
                    }
                }
            });
        });
        handlers.push(handler);
    }

    // Waiting for any thread to exit.
    JoinAny { handlers }.await?;

    Ok(())
}

struct JoinAny {
    handlers: Vec<JoinHandle<()>>,
}

impl Future for JoinAny {
    type Output = Result<(), FlameError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        for handler in &self.handlers {
            if handler.is_finished() {
                tracing::info!("Thread <{}> exited.", handler.thread().name().unwrap());
                return Poll::Ready(Ok(()));
            }
        }

        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

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
use futures::future::select_all;
use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

use common::ctx::FlameClusterContext;
use common::FlameError;

mod apiserver;
mod controller;
mod events;
mod model;
mod provider;
pub mod scheduler;
mod storage;

#[derive(Parser)]
#[command(name = "flame-session-manager")]
#[command(author = "Klaus Ma <klaus@xflops.cn>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Session Manager", long_about = None)]
struct Cli {
    #[arg(long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), FlameError> {
    common::init_logger()?;

    let cli = Cli::parse();
    let ctx = FlameClusterContext::from_file(cli.config)?;

    tracing::info!("flame-session-manager is starting ...");

    let mut handlers = vec![];

    let storage = storage::new_ptr(&ctx).await?;

    // Load data from engine, e.g. sqlite.
    storage.load_data().await?;

    let controller = controller::new_ptr(storage.clone());
    let build_runtime = |name: &str, threads: usize| -> Result<Runtime, FlameError> {
        Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name(name)
            .enable_all()
            .build()
            .map_err(|e| FlameError::Internal(format!("failed to build runtime <{name}>: {e}")))
    };

    let frontend_rt = build_runtime("frontend", 1)?;
    let backend_rt = build_runtime("backend", 1)?;
    let scheduler_rt = build_runtime("scheduler", 1)?;
    let provider_rt = build_runtime("provider", 1)?;

    // Start provider thread.
    #[allow(clippy::let_underscore_future)]
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let _ = provider_rt.spawn(async move {
            let provider = provider::new("none", controller)?;
            provider.run(ctx).await
        });
        // handlers.push(handler);
    }

    // Start apiserver frontend thread.
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let handler = frontend_rt.spawn(async move {
            let apiserver = apiserver::new_frontend(controller);
            apiserver.run(ctx).await
        });
        handlers.push(handler);
    }

    // Start apiserver backend thread.
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let handler = backend_rt.spawn(async move {
            let apiserver = apiserver::new_backend(controller);
            apiserver.run(ctx).await
        });
        handlers.push(handler);
    }

    // Start scheduler thread.
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let handler = scheduler_rt.spawn(async move {
            let scheduler = scheduler::new(controller);
            scheduler.run(ctx).await
        });
        handlers.push(handler);
    }

    tracing::info!("flame-session-manager started.");

    // Register default applications.
    #[allow(clippy::let_underscore_future)]
    let _: JoinHandle<Result<(), FlameError>> = tokio::spawn(async move {
        for (name, attr) in common::default_applications() {
            controller.register_application(name, attr).await?;
        }

        Ok(())
    });

    let (res, idx, _) = select_all(handlers).await;
    tracing::info!("Thread <{idx}> exited with result: {res:?}");

    Ok(())
}

#[async_trait::async_trait]
pub trait FlameThread: Send + Sync + 'static {
    async fn run(&self, ctx: FlameClusterContext) -> Result<(), FlameError>;
}

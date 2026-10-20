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

use common::ctx::FlameClusterContext;
use common::FlameError;

mod apiserver;
mod applications;
mod controller;
mod model;
mod notify;
mod provider;
mod scheduler;
use common::storage;

// The frontend, the backend (executor managers and their heartbeats) and the scheduler each run
// on their own runtime, so a stall in one (e.g. blocking storage I/O) cannot starve the others.
// Worker threads per runtime, as a share of the host's cores. The backend serves every executor
// manager and executor, so it gets the most.
fn runtime_threads(divisor: usize, min: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores / divisor).max(min)
}

/// Runs `future` to completion on a dedicated multi-thread runtime and returns a handle that
/// resolves with its result, so it can be awaited next to the tasks of the main runtime.
fn spawn_runtime<F>(
    name: &'static str,
    threads: usize,
    future: F,
) -> tokio::task::JoinHandle<Result<(), FlameError>>
where
    F: std::future::Future<Output = Result<(), FlameError>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("fsm-{name}"))
        .spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(threads)
                .thread_name(format!("fsm-{name}-worker"))
                .enable_all()
                .build()
                .map_err(|e| FlameError::Internal(format!("failed to build {name} runtime: {e}")))
                .and_then(|runtime| runtime.block_on(future));
            let _ = tx.send(result);
        })
        .expect("failed to spawn runtime thread");
    tokio::spawn(async move {
        rx.await
            .unwrap_or_else(|_| Err(FlameError::Internal(format!("{name} runtime panicked"))))
    })
}

#[derive(Parser)]
#[command(name = "flame-session-manager")]
#[command(author = "XFLOPS <support@xflops.io>")]
#[command(version)]
#[command(about = "Flame Session Manager", long_about = None)]
struct Cli {
    #[arg(long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), FlameError> {
    let _log_guard = common::init_logger(Some("fsm"))?;

    let cli = Cli::parse();
    let ctx = FlameClusterContext::from_file(cli.config)?;
    let configured_applications = applications::load(&applications::manifest_directory())?;

    tracing::info!("flame-session-manager is starting ...");

    let mut handlers = vec![];

    if let Some(pprof_config) = &ctx.cluster.pprof {
        let port = pprof_config.port;
        tokio::spawn(async move {
            common::pprof::run_pprof_server(Some(port)).await;
        });
    }

    let storage = storage::new_ptr(&ctx).await?;

    // Load data from engine, e.g. sqlite.
    storage.load_data().await?;

    let controller = controller::new_ptr(storage.clone());
    let application_manager = applications::ApplicationManager::new(controller.clone(), storage);

    // Resume durable application removals before configured applications are
    // reconciled or any request-serving task starts.
    application_manager.reconcile_once().await?;

    // Application manifests are authoritative for the names they define. Apps
    // absent from the directory are left untouched.
    applications::reconcile(
        configured_applications,
        {
            let controller = controller.clone();
            move |name, attributes| {
                let controller = controller.clone();
                async move { controller.register_application(name, attributes).await }
            }
        },
        {
            let controller = controller.clone();
            move |name, attributes| {
                let controller = controller.clone();
                async move {
                    let current = controller.get_application(name.clone()).await?;
                    if current.state == common::apis::ApplicationState::Disabled {
                        return Ok(());
                    }
                    if applications::matches_attributes(&current, &attributes) {
                        return Ok(());
                    }
                    controller.update_application(name, attributes).await
                }
            }
        },
    )
    .await?;

    // Start application lifecycle reconciliation.
    {
        let application_manager = application_manager.clone();
        let ctx = ctx.clone();
        let handler = tokio::spawn(async move { application_manager.run(ctx).await });
        handlers.push(handler);
    }

    // Start provider task.
    #[allow(clippy::let_underscore_future)]
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let _ = tokio::spawn(async move {
            let provider = provider::new("none", controller)?;
            provider.run(ctx).await
        });
    }

    // Start apiserver frontend task.
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let handler = spawn_runtime("frontend", runtime_threads(4, 2), async move {
            let apiserver = apiserver::new_frontend(controller);
            apiserver.run(ctx).await
        });
        handlers.push(handler);
    }

    // Start apiserver backend task.
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let handler = spawn_runtime("backend", runtime_threads(2, 4), async move {
            let apiserver = apiserver::new_backend(controller);
            apiserver.run(ctx).await
        });
        handlers.push(handler);
    }

    // Start scheduler task.
    {
        let controller = controller.clone();
        let ctx = ctx.clone();
        let handler = spawn_runtime("scheduler", runtime_threads(8, 2), async move {
            let scheduler = scheduler::new(controller);
            scheduler.run(ctx).await
        });
        handlers.push(handler);
    }

    tracing::info!("flame-session-manager started.");

    let (res, idx, _) = select_all(handlers).await;
    tracing::info!("Thread <{idx}> exited with result: {res:?}");

    Ok(())
}

#[async_trait::async_trait]
pub trait FlameThread: Send + Sync + 'static {
    async fn run(&self, ctx: FlameClusterContext) -> Result<(), FlameError>;
}

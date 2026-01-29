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

use std::error::Error;

use clap::{Parser, Subcommand};
use flame_rs::apis::FlameContext;

mod apis;
mod create;
mod helper;
mod list;
mod migrate;
mod register;
mod unregister;
mod update;
mod view;

#[derive(Parser)]
#[command(name = "flmctl")]
#[command(author = "Klaus Ma <klaus1982.cn@gmail.com>")]
#[command(version = "0.5.0")]
#[command(about = "Flame command line", long_about = None)]
struct Cli {
    /// The configuration of flmctl
    #[arg(long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// View the object of Flame
    View {
        /// The name of application
        #[arg(short, long)]
        application: Option<String>,

        /// The id of session
        #[arg(short, long)]
        session: Option<String>,

        /// The id of task
        #[arg(short, long)]
        task: Option<String>,

        /// The output format of the view
        #[arg(short, long)]
        output_format: Option<String>,
    },
    /// Update the object of Flame
    Update {
        /// The yaml file of the application
        #[arg(short, long)]
        application: Option<String>,
    },
    /// List the objects of Flame
    List {
        /// List the applications of Flame
        #[arg(short, long)]
        application: bool,
        /// List the sessions of Flame
        #[arg(short, long)]
        session: bool,
        /// List the executors of Flame
        #[arg(short, long)]
        executor: bool,
    },
    /// Close the session in Flame
    Close {
        /// The id of session
        #[arg(short, long)]
        session: String,
    },
    /// Create a session in Flame
    Create {
        /// The name of Application
        #[arg(short, long)]
        app: String,
        /// The slots requirements of each task
        #[arg(short, long)]
        slots: u32,
    },
    /// Migrate Flame metadata
    Migrate {
        /// The url of Flame database
        #[arg(short, long)]
        url: String,
        /// The target SQL schema of Flame database
        #[arg(short, long)]
        sql: String,
    },
    /// Register an application
    Register {
        /// The yaml file of the application
        #[arg(short, long)]
        file: String,
    },
    /// Unregister the application from Flame
    Unregister {
        /// The name of the application
        #[arg(short, long)]
        application: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame_rs::apis::init_logger()?;

    let cli = Cli::parse();
    let ctx = FlameContext::from_file(cli.config)?;

    match &cli.command {
        Some(Commands::List {
            application,
            session,
            executor,
        }) => list::run(&ctx, *application, *session, *executor).await?,
        Some(Commands::Close { .. }) => {
            todo!()
        }
        Some(Commands::Create { app, slots }) => create::run(&ctx, app, slots).await?,
        Some(Commands::View {
            application,
            session,
            task,
            output_format,
        }) => view::run(&ctx, output_format, application, session, task).await?,
        Some(Commands::Migrate { url, sql }) => migrate::run(&ctx, url, sql).await?,
        Some(Commands::Register { file }) => register::run(&ctx, file).await?,
        Some(Commands::Unregister { application }) => unregister::run(&ctx, application).await?,
        Some(Commands::Update { application }) => update::run(&ctx, application).await?,
        _ => helper::run().await?,
    };

    Ok(())
}

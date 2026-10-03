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

mod api;

use std::error::Error;

use clap::Parser;
use flame_rs::apis::FlameError;
use flame_rs::client::{SessionOptions, TaskResult};
use flame_rs::{self as flame};
use futures::future::try_join_all;

use api::{PiRequest, PiResponse};

#[derive(Parser)]
#[command(name = "pi")]
#[command(author = "Klaus Ma <klaus1982.cn@gmail.com>")]
#[command(version = "0.1.0")]
#[command(about = "Flame Pi Example", long_about = None)]
struct Cli {
    /// Application name deployed with flmctl deploy.
    #[arg(short, long)]
    app: String,
    /// Number of tasks to run.
    #[arg(long, default_value_t = DEFAULT_TASK_NUM)]
    task_num: u32,
    /// Number of random points sampled by each task.
    #[arg(long, default_value_t = DEFAULT_TASK_INPUT)]
    task_input: u32,
}

const DEFAULT_TASK_NUM: u32 = 10;
const DEFAULT_TASK_INPUT: u32 = 10000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    validate_args(&cli)?;

    let ssn = flame::create_session(SessionOptions::new(cli.app.clone())).await?;

    let request = PiRequest {
        samples: cli.task_input,
    };
    let handles =
        try_join_all((0..cli.task_num).map(|_| ssn.run::<_, PiResponse>(&request))).await?;
    let tasks = try_join_all(handles).await?;
    let area = count_inside(&tasks)?;

    let total = cli.task_num as u64 * cli.task_input as u64;
    let pi = 4_f64 * area as f64 / total as f64;
    println!("pi = 4*({area}/{total}) = {pi}");

    ssn.close().await?;

    Ok(())
}

fn validate_args(cli: &Cli) -> Result<(), FlameError> {
    if cli.task_num == 0 {
        return Err(FlameError::InvalidConfig(
            "--task-num must be greater than 0".to_string(),
        ));
    }
    if cli.task_input == 0 {
        return Err(FlameError::InvalidConfig(
            "--task-input must be greater than 0".to_string(),
        ));
    }
    Ok(())
}

fn count_inside(tasks: &[TaskResult<PiResponse>]) -> Result<u64, FlameError> {
    let mut area = 0_u64;
    for task in tasks {
        if !task.is_succeed() {
            return Err(FlameError::Internal(format_task_error(task)));
        }
        area += task
            .output
            .as_ref()
            .map(|output| output.inside as u64)
            .unwrap_or(0);
    }
    Ok(area)
}

fn format_task_error(task: &TaskResult<PiResponse>) -> String {
    task.error_message
        .clone()
        .unwrap_or_else(|| format!("task {} ended in state {}", task.task, task.state))
}

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

use futures::future::try_join_all;
use std::error::Error;
use std::sync::{Arc, Mutex};

use chrono::Local;
use clap::Parser;
use indicatif::HumanCount;
use serde_derive::{Deserialize, Serialize};

use flame_rs as flame;
use flame_rs::apis::{FlameContext, FlameError};
use flame_rs::client::{SessionAttributes, Task, TaskInformer};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Script {
    language: String,
    code: String,
    input: Option<Vec<u8>>,
}

#[derive(Parser)]
#[command(name = "flmexec")]
#[command(author = "Klaus Ma <klaus1982.cn@gmail.com>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Executor CLI", long_about = None)]
struct Cli {
    #[arg(long)]
    config: Option<String>,
    #[arg(short, long)]
    slots: Option<u32>,
    #[arg(short, long)]
    task_num: Option<i32>,
    /// The code to execute on worker nodes
    #[arg(short, long)]
    code: String,
    /// The language of the code
    #[arg(short, long, default_value = "shell", value_parser = parse_language)]
    language: String,
    /// The input to the code slice
    #[arg(short, long)]
    input: Option<Vec<u8>>,
}

fn parse_language(s: &str) -> Result<String, String> {
    if s.to_lowercase() == "shell" || s.to_lowercase() == "python" {
        Ok(s.to_string())
    } else {
        Err(format!("Invalid language: {s}"))
    }
}

const DEFAULT_APP: &str = "flmexec";
const DEFAULT_SLOTS: u32 = 1;
const DEFAULT_TASK_NUM: i32 = 10;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let ctx = FlameContext::from_file(cli.config)?;

    let slots = cli.slots.unwrap_or(DEFAULT_SLOTS);
    let task_num = cli.task_num.unwrap_or(DEFAULT_TASK_NUM);

    let current_cluster = ctx.get_current_cluster()?;
    let conn = flame::client::connect(&current_cluster.endpoint).await?;

    let ssn_creation_start_time = Local::now();
    let ssn_attr = SessionAttributes {
        id: format!("{DEFAULT_APP}-{}", stdng::rand::short_name()),
        application: DEFAULT_APP.to_string(),
        slots,
        common_data: None,
        min_instances: 0,    // Default: no minimum guarantee
        max_instances: None, // Default: unlimited
    };
    let ssn = conn.create_session(&ssn_attr).await?;
    let ssn_creation_end_time = Local::now();

    let ssn_creation_time =
        ssn_creation_end_time.timestamp_millis() - ssn_creation_start_time.timestamp_millis();
    println!("Session <{}> was created in <{ssn_creation_time} ms>, start to run <{}> tasks in the session:\n", ssn.id, HumanCount(task_num as u64));

    let mut tasks = vec![];
    let tasks_creations_start_time = Local::now();

    let info = Arc::new(Mutex::new(ExecInfo {}));

    let script = Script {
        language: cli.language.clone(),
        code: cli.code.clone(),
        input: cli.input.clone(),
    };

    let input = serde_json::to_string(&script)?;

    for _ in 0..task_num {
        tasks.push(ssn.run_task(Some(input.clone().into()), info.clone()));
    }

    try_join_all(tasks).await?;
    let tasks_creation_end_time = Local::now();

    let tasks_creation_time =
        tasks_creation_end_time.timestamp_millis() - tasks_creations_start_time.timestamp_millis();

    println!(
        "\n\n<{}> tasks was completed in <{} ms>.\n",
        HumanCount(task_num as u64),
        HumanCount(tasks_creation_time as u64)
    );

    ssn.close().await?;

    Ok(())
}

struct ExecInfo {}

impl TaskInformer for ExecInfo {
    fn on_update(&mut self, task: Task) {
        if task.is_completed() {
            println!(
                "Task {:<10}: {:?}",
                task.id,
                task.output.unwrap_or_default()
            );
        }
    }

    fn on_error(&mut self, e: FlameError) {
        println!("Got an error: {e}")
    }
}

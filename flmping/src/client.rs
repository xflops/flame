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

mod apis;

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use byte_unit::Byte;
use clap::Parser;
use flame_rs::apis::FlameError;
use flame_rs::client::{SessionAttributes, Task, TaskInformer};
use futures::future::try_join_all;
use indicatif::HumanCount;

use crate::apis::{PingRequest, PingResponse};

use flame::apis::FlameContext;
use flame_rs::{self as flame, new_ptr};

#[derive(Parser)]
#[command(name = "flmping")]
#[command(author = "Xflops <support@xflops.io>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Ping", long_about = None)]
struct Cli {
    #[arg(long)]
    /// The flame configuration file
    flame_conf: Option<String>,
    #[arg(short, long, default_value = "1")]
    /// The number of slots to use
    slots: i32,
    /// The number of tasks to run
    #[arg(short, long, default_value = "10")]
    task_num: i32,
    #[arg(short, long)]
    /// The common data to pass to the tasks
    common_data: Option<String>,
    /// The duration (milliseconds) to sleep for the tasks
    #[arg(short, long)]
    duration: Option<u64>,
    /// The memory (bytes) to allocate to the tasks
    #[arg(short, long)]
    memory: Option<String>,
}

const DEFAULT_APP: &str = "flmping";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let ctx = FlameContext::from_file(cli.flame_conf)?;

    let slots = cli.slots;
    let task_num = cli.task_num;
    let memory = match cli.memory {
        Some(s) => Some(Byte::parse_str(&s, true)?.as_u64()),
        None => None,
    };

    let conn = flame::client::connect(&ctx.endpoint).await?;

    let ssn_creation_start_time = Instant::now();
    let ssn_attr = SessionAttributes {
        application: DEFAULT_APP.to_string(),
        slots,
        common_data: cli.common_data.map(|s| s.into()),
    };
    let ssn = conn.create_session(&ssn_attr).await?;
    let ssn_creation_end_time = Instant::now();

    let ssn_creation_time = ssn_creation_end_time
        .duration_since(ssn_creation_start_time)
        .as_millis();
    println!("Session <{}> was created in <{ssn_creation_time} ms>, start to run <{}> tasks in the session:\n", ssn.id, HumanCount(task_num as u64));

    let mut tasks = vec![];
    let tasks_creations_start_time = Instant::now();

    let info = new_ptr!(OutputInfor::new());

    for _ in 0..task_num {
        let input = PingRequest {
            duration: cli.duration,
            memory: memory,
        }
        .try_into()?;
        tasks.push(ssn.run_task(Some(input), info.clone()));
    }

    try_join_all(tasks).await?;
    let tasks_creation_end_time = Instant::now();

    let tasks_creation_time = tasks_creation_end_time
        .duration_since(tasks_creations_start_time)
        .as_millis();

    println!(
        "\n\n<{}> tasks was completed in <{} ms>.\n",
        HumanCount(task_num as u64),
        HumanCount(tasks_creation_time as u64)
    );

    ssn.close().await?;

    Ok(())
}

struct OutputInfor {}

impl OutputInfor {
    fn new() -> Self {
        println!("{:<10}{:<10}{:<15}Output", "Session", "Task", "State");

        Self {}
    }
}

impl TaskInformer for OutputInfor {
    fn on_update(&mut self, task: Task) {
        if task.is_completed() {
            let output = task.output.unwrap_or_default();
            if let Ok(output) = <PingResponse>::try_from(output) {
                println!(
                    "{:<10}{:<10}{:<15}{:?}",
                    task.ssn_id, task.id, task.state, output.message
                );
            }
        }
    }

    fn on_error(&mut self, _: FlameError) {
        print!("Got an error")
    }
}

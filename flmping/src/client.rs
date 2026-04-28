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
use std::time::Instant;

use byte_unit::Byte;
use clap::Parser;
use comfy_table::presets::NOTHING;
use comfy_table::Table;
use flame_rs::apis::FlameError;
use flame_rs::client::{Session, SessionAttributes, Task, TaskInformer};
use futures::future::try_join_all;
use indicatif::HumanCount;
use stdng::{lock_ptr, new_ptr, MutexPtr};

use crate::apis::{PingRequest, PingResponse};

use flame::apis::FlameContext;
use flame_rs::{self as flame};

#[derive(Parser)]
#[command(name = "flmping")]
#[command(author = "Xflops <support@xflops.io>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Ping", long_about = None)]
struct Cli {
    #[arg(long)]
    /// The flame configuration file
    config: Option<String>,
    #[arg(short, long, default_value = "1")]
    /// The number of slots to use
    slots: u32,
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
    /// Performance benchmark mode (summary only, no per-task output)
    #[arg(short, long, default_value = "false")]
    perf: bool,
    /// Session priority (higher value = higher priority, default: 0 = lowest)
    #[arg(short = 'P', long, default_value = "0")]
    priority: u32,
}

const DEFAULT_APP: &str = "flmping";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let ctx = FlameContext::from_file(cli.config)?;

    let slots = cli.slots;
    let task_num = cli.task_num;
    let memory = match cli.memory {
        Some(s) => Some(Byte::parse_str(&s, true)?.as_u64()),
        None => None,
    };

    let current_ctx = ctx.get_current_context()?;

    let conn = flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;

    let ssn_creation_start_time = Instant::now();
    let ssn_attr = SessionAttributes {
        id: format!("{DEFAULT_APP}-{}", stdng::rand::short_name()),
        application: DEFAULT_APP.to_string(),
        slots,
        common_data: cli.common_data.map(|s| s.into()),
        min_instances: 0,
        max_instances: None,
        batch_size: 1,
        priority: cli.priority,
    };
    let ssn = conn.create_session(&ssn_attr).await?;
    let ssn_creation_end_time = Instant::now();

    let ssn_creation_time = ssn_creation_end_time
        .duration_since(ssn_creation_start_time)
        .as_millis();
    println!("Session <{}> was created in <{ssn_creation_time} ms>, start to run <{}> tasks in the session:\n", ssn.id, HumanCount(task_num as u64));

    if cli.perf {
        run_perf_mode(&ssn, task_num, cli.duration, memory).await?;
    } else {
        run_output_mode(&ssn, task_num, cli.duration, memory).await?;
    }

    ssn.close().await?;

    Ok(())
}

async fn run_tasks<T: TaskInformer + 'static>(
    ssn: &Session,
    task_num: i32,
    duration: Option<u64>,
    memory: Option<u64>,
    informer: MutexPtr<T>,
) -> Result<u128, Box<dyn Error>> {
    let input: flame_rs::apis::TaskInput = PingRequest { duration, memory }.try_into()?;
    let start = Instant::now();

    let tasks: Vec<_> = (0..task_num)
        .map(|_| ssn.run_task(Some(input.clone()), informer.clone()))
        .collect();

    try_join_all(tasks).await?;
    Ok(start.elapsed().as_millis())
}

async fn run_perf_mode(
    ssn: &Session,
    task_num: i32,
    duration: Option<u64>,
    memory: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let info = new_ptr(PerfInformer::new());
    let duration_ms = run_tasks(ssn, task_num, duration, memory, info.clone()).await?;

    let info = lock_ptr!(info)?;
    info.print_summary(task_num, duration_ms);

    Ok(())
}

async fn run_output_mode(
    ssn: &Session,
    task_num: i32,
    duration: Option<u64>,
    memory: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let info = new_ptr(OutputInfor::new());
    let duration_ms = run_tasks(ssn, task_num, duration, memory, info.clone()).await?;

    {
        let info = lock_ptr!(info)?;
        info.print();
    }

    println!(
        "\n\n<{}> tasks was completed in <{} ms>.\n",
        HumanCount(task_num as u64),
        HumanCount(duration_ms as u64)
    );

    Ok(())
}

struct OutputInfor {
    table: Table,
}

impl OutputInfor {
    fn new() -> Self {
        let mut table = Table::new();

        table
            .load_preset(NOTHING)
            .set_header(vec!["Session", "Task", "State", "Output"]);

        Self { table }
    }
}

impl TaskInformer for OutputInfor {
    fn on_update(&mut self, task: Task) {
        if task.is_completed() {
            let output = task.output.unwrap_or_default();
            if let Ok(output) = <PingResponse>::try_from(output) {
                self.table.add_row(vec![
                    task.ssn_id.to_string(),
                    task.id.to_string(),
                    task.state.to_string(),
                    output.message.to_string(),
                ]);
            }
        }
    }

    fn on_error(&mut self, _: FlameError) {
        print!("Got an error")
    }
}

impl OutputInfor {
    fn print(&self) {
        println!("{}", self.table);
    }
}

struct PerfInformer {
    succeeded: u32,
    failed: u32,
}

impl PerfInformer {
    fn new() -> Self {
        Self {
            succeeded: 0,
            failed: 0,
        }
    }

    fn print_summary(&self, task_num: i32, duration_ms: u128) {
        let duration_secs = duration_ms as f64 / 1000.0;
        let throughput = if duration_secs > 0.0 {
            self.succeeded as f64 / duration_secs
        } else {
            0.0
        };

        println!("\n{}", "=".repeat(60));
        println!("BENCHMARK RESULTS");
        println!("{}", "=".repeat(60));
        println!("Duration:        {:.2}s", duration_secs);
        println!("Succeeded:       {}/{}", self.succeeded, task_num);
        println!("Failed:          {}", self.failed);
        println!("Throughput:      {:.2} tasks/sec", throughput);
        println!("{}\n", "=".repeat(60));
    }
}

impl TaskInformer for PerfInformer {
    fn on_update(&mut self, task: Task) {
        if task.is_completed() {
            if task.is_succeed() {
                self.succeeded += 1;
            } else {
                self.failed += 1;
            }
        }
    }

    fn on_error(&mut self, _: FlameError) {
        self.failed += 1;
    }
}

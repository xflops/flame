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
use futures::future::try_join_all;
use indicatif::HumanCount;
use serde_derive::{Deserialize, Serialize};

use crate::apis::{PingRequest, PingResponse};

use flame::client::{Session, SessionOptions};
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
    /// Explicit resource request (format: "cpu=N,mem=SIZE,gpu=N").
    /// If omitted, server applies `cluster.resreq` (or a
    /// hardcoded fallback when that is unset).
    #[arg(short, long)]
    resreq: Option<String>,
}

const DEFAULT_APP: &str = "flmping";

#[derive(Debug, Clone, Serialize, Deserialize, flame::FlameMessage)]
struct PingCommonData {
    value: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let task_num = cli.task_num;
    let memory = match cli.memory {
        Some(s) => Some(Byte::parse_str(&s, true)?.as_u64()),
        None => None,
    };

    let conn = flame::connect_with_config(cli.config).await?;

    let ssn_creation_start_time = Instant::now();
    let mut options = SessionOptions::new(DEFAULT_APP).priority(cli.priority);
    if let Some(resreq) = cli.resreq.as_deref() {
        options = options.resreq(resreq);
    }
    if let Some(common_data) = cli.common_data {
        options = options.common_data(&PingCommonData { value: common_data })?;
    }
    let ssn = conn.create_session_with(options).await?;
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

async fn run_tasks(
    ssn: &Session,
    task_num: i32,
    duration: Option<u64>,
    memory: Option<u64>,
) -> Result<(u128, Vec<(String, Option<PingResponse>)>), Box<dyn Error>> {
    let input = PingRequest { duration, memory };
    let start = Instant::now();

    let handles = try_join_all((0..task_num).map(|_| ssn.run::<_, PingResponse>(&input))).await?;
    let outputs = try_join_all(handles.into_iter().map(|handle| async move {
        let task_id = handle.id().clone();
        let output = handle.wait().await?;
        Ok::<_, flame::apis::FlameError>((task_id, output))
    }))
    .await?;

    Ok((start.elapsed().as_millis(), outputs))
}

async fn run_perf_mode(
    ssn: &Session,
    task_num: i32,
    duration: Option<u64>,
    memory: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let (duration_ms, outputs) = run_tasks(ssn, task_num, duration, memory).await?;
    print_perf_summary(outputs.len() as u32, task_num, duration_ms);

    Ok(())
}

async fn run_output_mode(
    ssn: &Session,
    task_num: i32,
    duration: Option<u64>,
    memory: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let (duration_ms, outputs) = run_tasks(ssn, task_num, duration, memory).await?;
    print_outputs(ssn, outputs);

    println!(
        "\n\n<{}> tasks was completed in <{} ms>.\n",
        HumanCount(task_num as u64),
        HumanCount(duration_ms as u64)
    );

    Ok(())
}

fn print_outputs(ssn: &Session, outputs: Vec<(String, Option<PingResponse>)>) {
    let mut table = Table::new();

    table
        .load_preset(NOTHING)
        .set_header(vec!["Session", "Task", "State", "Output"]);

    for (task_id, output) in outputs {
        table.add_row(vec![
            ssn.id.to_string(),
            task_id,
            "Succeed".to_string(),
            output.map(|output| output.message).unwrap_or_default(),
        ]);
    }

    println!("{table}");
}

fn print_perf_summary(succeeded: u32, task_num: i32, duration_ms: u128) {
    let duration_secs = duration_ms as f64 / 1000.0;
    let throughput = if duration_secs > 0.0 {
        succeeded as f64 / duration_secs
    } else {
        0.0
    };

    println!("\n{}", "=".repeat(60));
    println!("BENCHMARK RESULTS");
    println!("{}", "=".repeat(60));
    println!("Duration:        {:.2}s", duration_secs);
    println!("Succeeded:       {}/{}", succeeded, task_num);
    println!("Failed:          {}", task_num - succeeded as i32);
    println!("Throughput:      {:.2} tasks/sec", throughput);
    println!("{}\n", "=".repeat(60));
}

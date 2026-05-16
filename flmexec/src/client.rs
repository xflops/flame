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

use api::{Script, ScriptOutput};
use futures::future::try_join_all;
use std::error::Error;
use std::time::Instant;

use clap::Parser;
use indicatif::HumanCount;

use flame_rs as flame;
use flame_rs::client::SessionOptions;

#[derive(Parser)]
#[command(name = "flmexec")]
#[command(author = "Klaus Ma <klaus1982.cn@gmail.com>")]
#[command(version = "0.5.0")]
#[command(about = "Flame Executor CLI", long_about = None)]
struct Cli {
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
const DEFAULT_TASK_NUM: i32 = 10;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let task_num = cli.task_num.unwrap_or(DEFAULT_TASK_NUM);

    let ssn_creation_start_time = Instant::now();
    let ssn = flame::create_session(SessionOptions::new(DEFAULT_APP)).await?;

    let ssn_creation_time = ssn_creation_start_time.elapsed().as_millis();
    println!("Session <{}> was created in <{ssn_creation_time} ms>, start to run <{}> tasks in the session:\n", ssn.id, HumanCount(task_num as u64));

    let tasks_creations_start_time = Instant::now();

    let script = Script {
        language: cli.language.clone(),
        code: cli.code.clone(),
        input: cli.input.clone(),
    };

    let handles =
        try_join_all((0..task_num).map(|_| ssn.invoke::<_, ScriptOutput>(&script))).await?;
    let task_ids = handles
        .iter()
        .map(|handle| handle.id().clone())
        .collect::<Vec<_>>();
    let outputs = try_join_all(handles).await?;

    for (task_id, output) in task_ids.into_iter().zip(outputs) {
        println!(
            "Task {:<10}: {:?}",
            task_id,
            output.map(|output| output.data).unwrap_or_default()
        );
    }

    let tasks_creation_time = tasks_creations_start_time.elapsed().as_millis();

    println!(
        "\n\n<{}> tasks was completed in <{} ms>.\n",
        HumanCount(task_num as u64),
        HumanCount(tasks_creation_time as u64)
    );

    ssn.close().await?;

    Ok(())
}

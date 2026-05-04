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

mod util;

use std::error::Error;

use clap::Parser;
use futures::future::try_join_all;
use stdng::{lock_ptr, new_ptr};

use flame_rs::apis::{FlameContext, FlameError, TaskInput};
use flame_rs::client::{SessionAttributes, Task, TaskInformer};
use flame_rs::{self as flame};

#[derive(Parser)]
#[command(name = "pi")]
#[command(author = "Klaus Ma <klaus1982.cn@gmail.com>")]
#[command(version = "0.1.0")]
#[command(about = "Flame Pi Example", long_about = None)]
struct Cli {
    #[arg(short, long)]
    app: Option<String>,
    #[arg(short, long)]
    slots: Option<u32>,
    #[arg(long)]
    task_num: Option<u32>,
    #[arg(long)]
    task_input: Option<u32>,
}

const DEFAULT_APP: &str = "pi-app";
const DEFAULT_SLOTS: u32 = 1;
const DEFAULT_TASK_NUM: u32 = 10;
const DEFAULT_TASK_INPUT: u32 = 10000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let app = cli.app.unwrap_or(DEFAULT_APP.to_string());
    let slots = cli.slots.unwrap_or(DEFAULT_SLOTS);
    let task_num = cli.task_num.unwrap_or(DEFAULT_TASK_NUM);
    let task_input = cli.task_input.unwrap_or(DEFAULT_TASK_INPUT);

    let ctx = FlameContext::from_file(None)?;

    let current_ctx = ctx.get_current_context()?;
    let conn = flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;
    let ssn = conn
        .create_session(&SessionAttributes {
            id: format!("{app}-{}", stdng::rand::short_name()),
            application: app,
            slots,
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,
        })
        .await?;

    let informer = new_ptr(PiInfo { area: 0 });
    let mut tasks = vec![];
    for _ in 0..task_num {
        let task_input = util::u32_to_bytes(task_input);
        let task = ssn.run_task(Some(TaskInput::from(task_input)), informer.clone());
        tasks.push(task);
    }

    try_join_all(tasks).await?;

    {
        // Get the number of points in the circle.
        let informer = lock_ptr!(informer)?;
        let pi = 4_f64 * informer.area as f64 / ((task_num as f64) * (task_input as f64));

        println!(
            "pi = 4*({}/{}) = {}",
            informer.area,
            task_num * task_input,
            pi
        );
    }

    ssn.close().await?;

    Ok(())
}

pub struct PiInfo {
    pub area: u64,
}

impl TaskInformer for PiInfo {
    fn on_update(&mut self, task: Task) {
        if let Some(output) = task.output {
            let output = util::bytes_to_u32(output.to_vec()).ok().unwrap_or_default();
            self.area += output as u64;
        }
    }

    fn on_error(&mut self, _: FlameError) {
        print!("Got an error")
    }
}

/*
Copyright 2026 The Flame Authors.
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

//! `flmbench`: burst workloads over one or more sessions, with per-task latency
//! percentiles and cluster health sampling. Every session runs the `flmping`
//! application, so the `flmping` worker must be registered and available.

mod cli;
mod health;
mod report;
mod stats;
mod workload;

use std::error::Error;
use std::time::Duration;

use clap::Parser;

use cli::Cli;
use health::HealthSampler;
use report::{Report, RoundReport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    cli.validate()?;
    let input = workload::request(&cli)?;
    let connection = flame_rs::connect().await?;

    println!(
        "{} session(s) x {} task(s), {} round(s); the first round is cold\n",
        cli.sessions, cli.tasks, cli.rounds
    );

    let mut rounds = Vec::with_capacity(cli.rounds);
    for round in 1..=cli.rounds {
        let sampler = (cli.health_interval > 0).then(|| {
            HealthSampler::start(connection.clone(), Duration::from_secs(cli.health_interval))
        });
        let burst = workload::run_burst(&connection, &cli, &input).await;
        let health = match sampler {
            Some(sampler) => Some(sampler.finish().await),
            None => None,
        };
        let report = RoundReport::new(round, burst?, health);
        println!(
            "round {}: {}/{} succeeded in {:.2}s ({:.1} tasks/s); submitted in {:.2}s ({:.1} tasks/s), drained in {:.2}s",
            report.round,
            report.succeeded,
            report.submitted,
            report.wall_s,
            report.tasks_per_sec,
            report.submit_s,
            report.submit_tasks_per_sec,
            report.drain_s
        );
        if let Some(error) = &report.first_error {
            println!("  first error: {error}");
        }
        rounds.push(report);
    }

    let report = Report::new(&cli, rounds);
    println!("\n{}", report::table(&report.rounds));
    if let Some(median) = report.summary.warm_median_tasks_per_sec {
        println!("warm median: {median:.1} tasks/s");
    }
    if let Some(path) = &cli.output {
        std::fs::write(path, report.to_json()?)?;
        println!("wrote {}", path.display());
    }

    if report.summary.all_succeeded {
        Ok(())
    } else {
        Err("some tasks did not succeed".into())
    }
}

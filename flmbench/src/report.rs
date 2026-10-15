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

use comfy_table::presets::ASCII_MARKDOWN;
use comfy_table::{Cell, CellAlignment, Table};
use serde_derive::Serialize;

use crate::cli::Cli;
use crate::health::HealthSummary;
use crate::stats::{median, Latency};
use crate::workload::Burst;

#[derive(Debug, Serialize)]
pub struct Config {
    pub sessions: usize,
    pub tasks_per_session: usize,
    pub rounds: usize,
    pub duration_ms: Option<u64>,
    pub memory: Option<String>,
    pub priority: u32,
    pub resreq: Option<String>,
    pub health_interval_secs: u64,
}

impl From<&Cli> for Config {
    fn from(cli: &Cli) -> Self {
        Self {
            sessions: cli.sessions,
            tasks_per_session: cli.tasks,
            rounds: cli.rounds,
            duration_ms: cli.duration,
            memory: cli.memory.clone(),
            priority: cli.priority,
            resreq: cli.resreq.clone(),
            health_interval_secs: cli.health_interval,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RoundReport {
    pub round: usize,
    pub kind: &'static str,
    pub submitted: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub create_ms: f64,
    /// First submission until the last `create_task` returned.
    pub submit_s: f64,
    pub submit_tasks_per_sec: f64,
    /// Last submission until the last task finished.
    pub drain_s: f64,
    pub wall_s: f64,
    pub tasks_per_sec: f64,
    /// `create_task` call time.
    pub submit_latency: Option<Latency>,
    /// From the start of submission to the watch callback seeing the task finish.
    pub latency: Option<Latency>,
    pub health: Option<HealthSummary>,
    pub first_error: Option<String>,
}

impl RoundReport {
    pub fn new(round: usize, burst: Burst, health: Option<HealthSummary>) -> Self {
        let wall_s = burst.wall.as_secs_f64();
        let submit_s = burst.submit.as_secs_f64();
        let rate = |tasks: usize, seconds: f64| {
            if seconds > 0.0 {
                tasks as f64 / seconds
            } else {
                0.0
            }
        };
        Self {
            round,
            kind: if round == 1 { "cold" } else { "warm" },
            submitted: burst.submitted,
            succeeded: burst.succeeded,
            failed: burst.failed,
            create_ms: burst.create.as_secs_f64() * 1000.0,
            submit_s,
            submit_tasks_per_sec: rate(burst.submitted, submit_s),
            drain_s: burst.drain.as_secs_f64(),
            wall_s,
            tasks_per_sec: rate(burst.succeeded, wall_s),
            submit_latency: burst.submit_latency,
            latency: burst.latency,
            health,
            first_error: burst.first_error,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub all_succeeded: bool,
    pub cold_tasks_per_sec: Option<f64>,
    pub warm_median_tasks_per_sec: Option<f64>,
    pub warm_min_tasks_per_sec: Option<f64>,
    pub warm_max_tasks_per_sec: Option<f64>,
}

pub fn summarize(rounds: &[RoundReport]) -> Summary {
    let warm: Vec<f64> = rounds
        .iter()
        .filter(|round| round.kind == "warm")
        .map(|round| round.tasks_per_sec)
        .collect();
    Summary {
        all_succeeded: rounds.iter().all(|round| round.failed == 0),
        cold_tasks_per_sec: rounds.first().map(|round| round.tasks_per_sec),
        warm_median_tasks_per_sec: median(&warm),
        warm_min_tasks_per_sec: warm.iter().copied().reduce(f64::min),
        warm_max_tasks_per_sec: warm.iter().copied().reduce(f64::max),
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub config: Config,
    pub rounds: Vec<RoundReport>,
    pub summary: Summary,
}

impl Report {
    pub fn new(cli: &Cli, rounds: Vec<RoundReport>) -> Self {
        let summary = summarize(&rounds);
        Self {
            config: Config::from(cli),
            rounds,
            summary,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

fn number(value: f64) -> String {
    format!("{value:.1}")
}

pub fn table(rounds: &[RoundReport]) -> String {
    let mut table = Table::new();
    table.load_preset(ASCII_MARKDOWN).set_header(vec![
        "Round",
        "Kind",
        "Succeeded",
        "Failed",
        "Submit (s)",
        "Drain (s)",
        "Wall (s)",
        "Tasks/s",
        "p50 (ms)",
        "p90 (ms)",
        "p99 (ms)",
        "Max (ms)",
        "Not ready",
        "Executors",
    ]);
    for round in rounds {
        let latency = |pick: fn(&Latency) -> f64| {
            round
                .latency
                .as_ref()
                .map(|latency| number(pick(latency)))
                .unwrap_or_else(|| "-".to_string())
        };
        let (not_ready, executors) = match &round.health {
            Some(health) => (
                health.not_ready_samples.to_string(),
                health.max_executors.to_string(),
            ),
            None => ("-".to_string(), "-".to_string()),
        };
        table.add_row(vec![
            Cell::new(round.round),
            Cell::new(round.kind),
            Cell::new(format!("{}/{}", round.succeeded, round.submitted)),
            Cell::new(round.failed),
            Cell::new(format!("{:.2}", round.submit_s)),
            Cell::new(format!("{:.2}", round.drain_s)),
            Cell::new(format!("{:.2}", round.wall_s)),
            Cell::new(number(round.tasks_per_sec)),
            Cell::new(latency(|l| l.p50_ms)),
            Cell::new(latency(|l| l.p90_ms)),
            Cell::new(latency(|l| l.p99_ms)),
            Cell::new(latency(|l| l.max_ms)),
            Cell::new(not_ready),
            Cell::new(executors),
        ]);
    }
    for index in 2..14 {
        if let Some(column) = table.column_mut(index) {
            column.set_cell_alignment(CellAlignment::Right);
        }
    }
    table.to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn round(number: usize, succeeded: usize, wall_ms: u64) -> RoundReport {
        RoundReport::new(
            number,
            Burst {
                create: Duration::from_millis(3),
                submit: Duration::from_millis(wall_ms / 5),
                drain: Duration::from_millis(wall_ms - wall_ms / 5),
                wall: Duration::from_millis(wall_ms),
                submitted: 100,
                succeeded,
                failed: 100 - succeeded,
                submit_latency: None,
                latency: None,
                first_error: None,
            },
            None,
        )
    }

    #[test]
    fn throughput_is_successes_over_wall_time() {
        assert_eq!(round(1, 100, 500).tasks_per_sec, 200.0);
        assert_eq!(round(1, 50, 500).tasks_per_sec, 100.0);
    }

    #[test]
    fn submit_and_drain_add_up_to_the_wall_time() {
        let round = round(1, 100, 500);
        assert_eq!(round.submit_s, 0.1);
        assert_eq!(round.drain_s, 0.4);
        assert_eq!(round.submit_tasks_per_sec, 1000.0);
        assert_eq!(round.submit_s + round.drain_s, round.wall_s);
    }

    #[test]
    fn first_round_is_cold_and_the_rest_are_warm() {
        let rounds = vec![round(1, 100, 1000), round(2, 100, 500), round(3, 100, 250)];
        let summary = summarize(&rounds);
        assert_eq!(summary.cold_tasks_per_sec, Some(100.0));
        assert_eq!(summary.warm_median_tasks_per_sec, Some(300.0));
        assert_eq!(summary.warm_min_tasks_per_sec, Some(200.0));
        assert_eq!(summary.warm_max_tasks_per_sec, Some(400.0));
        assert!(summary.all_succeeded);
    }

    #[test]
    fn a_failed_task_fails_the_summary() {
        assert!(!summarize(&[round(1, 99, 100)]).all_succeeded);
    }

    #[test]
    fn json_report_has_config_rounds_and_summary() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["flmbench", "-s", "2", "-t", "50"]).unwrap();
        let report = Report::new(&cli, vec![round(1, 100, 500)]);
        let value: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
        assert_eq!(value["config"]["sessions"], 2);
        assert_eq!(value["config"]["tasks_per_session"], 50);
        assert_eq!(value["rounds"][0]["kind"], "cold");
        assert_eq!(value["rounds"][0]["succeeded"], 100);
        assert!(value["rounds"][0].get("submit_s").is_some());
        assert!(value["rounds"][0].get("drain_s").is_some());
        assert_eq!(value["summary"]["all_succeeded"], true);
    }

    #[test]
    fn table_has_a_row_per_round_and_a_header_for_every_column() {
        let text = table(&[round(1, 100, 500), round(2, 100, 250)]);
        assert_eq!(text.lines().count(), 4);
        assert!(text.contains("cold") && text.contains("warm"));
        let cells = |line: &str| line.matches('|').count();
        let mut lines = text.lines();
        let header = cells(lines.next().unwrap());
        assert!(
            lines.all(|line| cells(line) == header),
            "every row has the header's columns"
        );
        assert!(text.contains("Submit (s)") && text.contains("Drain (s)"));
    }
}

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

use std::path::PathBuf;

use clap::Parser;

/// The application every benchmark session runs; its worker is `flmping-service`.
pub const APPLICATION: &str = "flmping";

#[derive(Parser, Debug, Clone)]
#[command(name = "flmbench")]
#[command(version)]
#[command(about = "Flame benchmark: burst workloads over multiple sessions")]
pub struct Cli {
    /// Sessions created and run concurrently in each round
    #[arg(short = 's', long, default_value_t = 1)]
    pub sessions: usize,
    /// Tasks submitted to each session (all at once)
    #[arg(short = 't', long, default_value_t = 1000)]
    pub tasks: usize,
    /// Rounds to run; the first is reported as cold and the rest as warm
    #[arg(short = 'n', long, default_value_t = 1)]
    pub rounds: usize,
    /// Time (milliseconds) each task sleeps in the worker
    #[arg(short, long)]
    pub duration: Option<u64>,
    /// Memory (for example 64MiB) each task allocates in the worker
    #[arg(short, long)]
    pub memory: Option<String>,
    /// Session priority (higher value = higher priority)
    #[arg(short = 'P', long, default_value_t = 0)]
    pub priority: u32,
    /// Explicit resource request per session (format: "cpu=N,mem=SIZE,gpu=N")
    #[arg(long)]
    pub resreq: Option<String>,
    /// Seconds to wait, once all tasks are created, for them to finish before giving up on the rest
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
    /// Seconds between cluster health samples (0 disables sampling)
    #[arg(long, default_value_t = 3)]
    pub health_interval: u64,
    /// Write every round and the summary to this file as JSON
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

impl Cli {
    pub fn validate(&self) -> Result<(), String> {
        if self.sessions == 0 {
            return Err("--sessions must be at least 1".to_string());
        }
        if self.tasks == 0 {
            return Err("--tasks must be at least 1".to_string());
        }
        if self.rounds == 0 {
            return Err("--rounds must be at least 1".to_string());
        }
        if self.timeout == 0 {
            return Err("--timeout must be at least 1 second".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_run_one_session_of_one_thousand_tasks() {
        let cli = Cli::try_parse_from(["flmbench"]).unwrap();
        assert_eq!((cli.sessions, cli.tasks, cli.rounds), (1, 1000, 1));
        assert_eq!(cli.health_interval, 3);
        assert_eq!(cli.timeout, 600);
        cli.validate().unwrap();
    }

    #[test]
    fn parses_session_and_task_shape_options() {
        let cli = Cli::try_parse_from([
            "flmbench",
            "-s",
            "4",
            "-t",
            "250",
            "-n",
            "3",
            "-d",
            "5",
            "-m",
            "1MiB",
            "-P",
            "2",
            "--resreq",
            "cpu=1,mem=1Gi",
            "-o",
            "out.json",
        ])
        .unwrap();
        assert_eq!((cli.sessions, cli.tasks, cli.rounds), (4, 250, 3));
        assert_eq!(cli.duration, Some(5));
        assert_eq!(cli.memory.as_deref(), Some("1MiB"));
        assert_eq!(cli.priority, 2);
        assert_eq!(cli.resreq.as_deref(), Some("cpu=1,mem=1Gi"));
        assert_eq!(cli.output.unwrap().to_str(), Some("out.json"));
    }

    #[test]
    fn rejects_zero_counts() {
        for args in [["-s", "0"], ["-t", "0"], ["-n", "0"], ["--timeout", "0"]] {
            let cli = Cli::try_parse_from(["flmbench", args[0], args[1]]).unwrap();
            assert!(cli.validate().is_err());
        }
    }
}

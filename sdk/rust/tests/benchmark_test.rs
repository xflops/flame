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

//! Benchmark test for Flame startup latency and steady-state task throughput.
//!
//! A single cold `1 × 1` sample measures the first session round trip,
//! including executor and application startup. A `10 × 1` scale-out sample
//! then measures concurrent startup, followed by an untimed warm-up that keeps
//! all available executors busy before statistics are collected. The four-case
//! matrix runs repeatedly against retained executors and reports min, median,
//! and max wall time and throughput. Each sample must complete within 10 minutes.
//! Runtime-specific topology is selected only through the benchmark environment.
//!

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs::OpenOptions, io::Write};

use comfy_table::{presets::ASCII_MARKDOWN, Cell, CellAlignment, Table};
use futures::future::try_join_all;
use stdng::new_ptr;

use flame::{
    apis::{ExecutorState, FlameClientTls, FlameError, SessionState, TaskState},
    client::{SessionAttributes, Task, TaskInformer},
};
use flame_rs as flame;

const FLAME_ADDR: &str = "https://127.0.0.1:8080";
const FLAME_APP: &str = "flmping";

fn benchmark_endpoint() -> String {
    std::env::var("FLAME_BENCHMARK_ENDPOINT").unwrap_or_else(|_| FLAME_ADDR.to_string())
}

fn benchmark_runtime() -> String {
    std::env::var("FLAME_BENCHMARK_RUNTIME").unwrap_or_else(|_| "Host Shim".to_string())
}

fn benchmark_executor_count() -> Result<usize, FlameError> {
    let count = match std::env::var("FLAME_BENCHMARK_EXECUTORS") {
        Ok(value) => value.parse().map_err(|_| {
            FlameError::InvalidConfig(format!("invalid FLAME_BENCHMARK_EXECUTORS value <{value}>"))
        })?,
        Err(std::env::VarError::NotPresent) => STEADY_STATE_CONCURRENCY,
        Err(error) => return Err(FlameError::InvalidConfig(error.to_string())),
    };
    if count == 0 {
        return Err(FlameError::InvalidConfig(
            "FLAME_BENCHMARK_EXECUTORS must be greater than zero".to_string(),
        ));
    }
    Ok(count)
}

fn get_ca_cert_path() -> String {
    let root = std::env::var("FLAME_ROOT").unwrap_or_else(|_| {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        format!("{}/../..", manifest_dir)
    });
    format!("{}/ci/certs/ca.crt", root)
}

struct BenchmarkCase {
    phase: &'static str,
    session_count: usize,
    tasks_per_session: usize,
    repetitions: usize,
}

const COLD_CASE: BenchmarkCase = BenchmarkCase {
    phase: "cold-start",
    session_count: 1,
    tasks_per_session: 1,
    repetitions: 1,
};
const STEADY_STATE_CONCURRENCY: usize = 10;
const SCALE_OUT_CASE: BenchmarkCase = BenchmarkCase {
    phase: "scale-out",
    session_count: STEADY_STATE_CONCURRENCY,
    tasks_per_session: 1,
    repetitions: 1,
};
const BENCHMARK_MATRIX: &[BenchmarkCase] = &[
    BenchmarkCase {
        phase: "steady-state",
        session_count: 1,
        tasks_per_session: 1,
        repetitions: 5,
    },
    BenchmarkCase {
        phase: "steady-state",
        session_count: 1,
        tasks_per_session: 1000,
        repetitions: 3,
    },
    BenchmarkCase {
        phase: "steady-state",
        session_count: STEADY_STATE_CONCURRENCY,
        tasks_per_session: 1,
        repetitions: 5,
    },
    BenchmarkCase {
        phase: "steady-state",
        session_count: STEADY_STATE_CONCURRENCY,
        tasks_per_session: 1000,
        repetitions: 3,
    },
];
const TIMEOUT_SECS: u64 = 600; // 10 minutes
const EXECUTOR_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Metrics collector for benchmark
struct BenchmarkMetrics {
    succeeded: AtomicU64,
    failed: AtomicU64,
}

impl BenchmarkMetrics {
    fn new() -> Self {
        Self {
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }
}

/// Task informer that tracks completion
struct BenchmarkTaskInformer {
    metrics: Arc<BenchmarkMetrics>,
}

impl BenchmarkTaskInformer {
    fn new(metrics: Arc<BenchmarkMetrics>) -> Self {
        Self { metrics }
    }
}

impl TaskInformer for BenchmarkTaskInformer {
    fn on_update(&mut self, task: Task) {
        match task.state {
            TaskState::Succeed => {
                self.metrics.succeeded.fetch_add(1, Ordering::Relaxed);
            }
            TaskState::Failed => {
                self.metrics.failed.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_error(&mut self, e: FlameError) {
        tracing::error!("Task error: {}", e);
        self.metrics.failed.fetch_add(1, Ordering::Relaxed);
    }
}

struct BenchmarkResult {
    duration: Duration,
    succeeded: u64,
    failed: u64,
}

struct BenchmarkStatistics {
    phase: &'static str,
    session_count: usize,
    tasks_per_session: usize,
    repetitions: usize,
    duration_min: Duration,
    duration_median: Duration,
    duration_max: Duration,
    throughput_min: f64,
    throughput_median: f64,
    throughput_max: f64,
}

/// Run tasks for a single session
async fn run_session(
    conn: &flame::client::Connection,
    session_id: String,
    tasks_per_session: usize,
    metrics: Arc<BenchmarkMetrics>,
) -> Result<(), FlameError> {
    let ssn_attr = SessionAttributes {
        id: session_id,
        application: FLAME_APP.to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        batch_size: 1,
        priority: 0,
        resreq: None,
    };

    let ssn = conn.create_session(&ssn_attr).await?;
    assert_eq!(ssn.state, SessionState::Open);

    // Submit all tasks for this session
    let mut task_handles = Vec::with_capacity(tasks_per_session);
    for _ in 0..tasks_per_session {
        let informer = new_ptr(BenchmarkTaskInformer::new(metrics.clone()));
        let handle = ssn.run_task(None, informer);
        task_handles.push(handle);
    }

    // Wait for all tasks to complete
    try_join_all(task_handles).await?;

    ssn.close().await?;
    Ok(())
}

async fn wait_for_executors_to_settle(conn: &flame::client::Connection) -> Result<(), FlameError> {
    let deadline = Instant::now() + EXECUTOR_SETTLE_TIMEOUT;
    loop {
        let settled = conn
            .list_executor()
            .await?
            .into_iter()
            .filter(|executor| executor.application == FLAME_APP)
            .all(|executor| {
                matches!(
                    executor.state,
                    ExecutorState::Idle | ExecutorState::Released
                )
            });
        if settled {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(FlameError::Network(
                "benchmark executors did not settle after sessions closed".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_retained_executor_count(
    conn: &flame::client::Connection,
    expected: usize,
) -> Result<(), FlameError> {
    let deadline = Instant::now() + EXECUTOR_SETTLE_TIMEOUT;
    loop {
        let idle = conn
            .list_executor()
            .await?
            .into_iter()
            .filter(|executor| {
                executor.application == FLAME_APP && executor.state == ExecutorState::Idle
            })
            .count();
        if idle == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(FlameError::Network(format!(
                "benchmark expected {expected} retained Idle executors, found {idle}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn run_benchmark(
    conn: &flame::client::Connection,
    session_count: usize,
    tasks_per_session: usize,
    sample_id: &str,
) -> Result<BenchmarkResult, FlameError> {
    let metrics = Arc::new(BenchmarkMetrics::new());
    let start = Instant::now();

    let mut session_handles = Vec::with_capacity(session_count);
    for session_index in 0..session_count {
        let conn = conn.clone();
        let metrics = metrics.clone();
        let session_id = format!(
            "benchmark-{sample_id}-{session_count}x{tasks_per_session}-ssn-{session_index}"
        );
        let handle = tokio::spawn(async move {
            run_session(&conn, session_id, tasks_per_session, metrics).await
        });
        session_handles.push(handle);
    }

    for handle in session_handles {
        handle
            .await
            .map_err(|error| FlameError::Internal(error.to_string()))??;
    }

    let result = BenchmarkResult {
        duration: start.elapsed(),
        succeeded: metrics.succeeded.load(Ordering::Relaxed),
        failed: metrics.failed.load(Ordering::Relaxed),
    };
    wait_for_executors_to_settle(conn).await?;
    let total_tasks = session_count * tasks_per_session;
    assert_eq!(result.failed, 0, "benchmark had failed tasks");
    assert_eq!(
        result.succeeded as usize, total_tasks,
        "not all benchmark tasks succeeded"
    );
    assert!(
        result.duration < Duration::from_secs(TIMEOUT_SECS),
        "benchmark case {} × {} exceeded 10 minute timeout: {:.2}s",
        session_count,
        tasks_per_session,
        result.duration.as_secs_f64()
    );
    Ok(result)
}

async fn run_case(
    conn: &flame::client::Connection,
    case: &BenchmarkCase,
) -> Result<BenchmarkStatistics, FlameError> {
    assert!(case.repetitions > 0, "benchmark case needs samples");

    let mut durations = Vec::with_capacity(case.repetitions);
    let mut throughputs = Vec::with_capacity(case.repetitions);
    for sample_index in 0..case.repetitions {
        let sample_id = format!("{}-{sample_index}", case.phase);
        let result =
            run_benchmark(conn, case.session_count, case.tasks_per_session, &sample_id).await?;
        durations.push(result.duration);
        throughputs.push(result.succeeded as f64 / result.duration.as_secs_f64());
    }

    durations.sort_unstable();
    throughputs.sort_by(f64::total_cmp);
    let median_index = case.repetitions / 2;

    Ok(BenchmarkStatistics {
        phase: case.phase,
        session_count: case.session_count,
        tasks_per_session: case.tasks_per_session,
        repetitions: case.repetitions,
        duration_min: durations[0],
        duration_median: durations[median_index],
        duration_max: durations[case.repetitions - 1],
        throughput_min: throughputs[0],
        throughput_median: throughputs[median_index],
        throughput_max: throughputs[case.repetitions - 1],
    })
}

fn format_results(runtime: &str, results: &[BenchmarkStatistics]) -> String {
    let mut table = Table::new();
    table.load_preset(ASCII_MARKDOWN).set_header([
        "Phase",
        "Sessions",
        "Tasks/session",
        "Samples",
        "Wall time ms (min/p50/max)",
        "Tasks/sec (min/p50/max)",
    ]);

    for result in results {
        table.add_row([
            Cell::new(result.phase),
            Cell::new(result.session_count).set_alignment(CellAlignment::Right),
            Cell::new(result.tasks_per_session).set_alignment(CellAlignment::Right),
            Cell::new(result.repetitions).set_alignment(CellAlignment::Right),
            Cell::new(format!(
                "{:.2}/{:.2}/{:.2}",
                result.duration_min.as_secs_f64() * 1000.0,
                result.duration_median.as_secs_f64() * 1000.0,
                result.duration_max.as_secs_f64() * 1000.0,
            ))
            .set_alignment(CellAlignment::Right),
            Cell::new(format!(
                "{:.2}/{:.2}/{:.2}",
                result.throughput_min, result.throughput_median, result.throughput_max,
            ))
            .set_alignment(CellAlignment::Right),
        ]);
    }

    format!("## {} BENCHMARK RESULTS\n\n{table}", runtime.to_uppercase())
}

fn print_results(runtime: &str, results: &[BenchmarkStatistics]) {
    let report = format_results(runtime, results);
    println!("\n{report}");
    if let Ok(summary_path) = std::env::var("GITHUB_STEP_SUMMARY") {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary_path)
            .and_then(|mut summary| writeln!(summary, "{report}"))
        {
            Ok(()) => {}
            Err(error) => eprintln!(
                "failed to append benchmark results to GitHub summary <{summary_path}>: {error}"
            ),
        }
    }
}

#[test]
fn benchmark_results_use_a_markdown_table() {
    let report = format_results(
        "Host Shim",
        &[BenchmarkStatistics {
            phase: "steady",
            session_count: 4,
            tasks_per_session: 1000,
            repetitions: 3,
            duration_min: Duration::from_millis(1000),
            duration_median: Duration::from_millis(1250),
            duration_max: Duration::from_millis(1500),
            throughput_min: 100.0,
            throughput_median: 200.0,
            throughput_max: 300.0,
        }],
    );

    assert!(report.starts_with("## HOST SHIM BENCHMARK RESULTS\n\n|"));
    assert!(report.contains("| steady"));
    assert!(report.contains("1000.00/1250.00/1500.00"));
    assert!(report.contains("100.00/200.00/300.00"));
}

#[tokio::test]
async fn benchmark_task_matrix() -> Result<(), FlameError> {
    tracing_subscriber::fmt::try_init().ok();

    let runtime = benchmark_runtime();
    let executor_count = benchmark_executor_count()?;
    let warm_up_case = BenchmarkCase {
        phase: "warm-up",
        session_count: executor_count,
        tasks_per_session: 100,
        repetitions: 1,
    };

    println!("\n============================================================");
    println!("{runtime} BENCHMARK");
    println!("============================================================\n");

    let tls_config = FlameClientTls {
        ca_file: Some(get_ca_cert_path()),
    };
    let conn = flame::client::connect_with_tls(&benchmark_endpoint(), Some(&tls_config)).await?;

    let mut results = Vec::with_capacity(BENCHMARK_MATRIX.len() + 2);

    // This is the only timed case without a retained flmping executor. Images
    // are prepared by the workflow, so it measures cached-image startup.
    results.push(run_case(&conn, &COLD_CASE).await?);

    // Exercise concurrent startup. This result remains visible because scale-out
    // latency is useful, but it is deliberately kept separate from the stable
    // warm-executor statistics.
    results.push(run_case(&conn, &SCALE_OUT_CASE).await?);

    // The configured per-node executor limit keeps both Host and CRI at the
    // same retained capacity across all phases. Exercise that capacity here,
    // then verify limit enforcement before collecting steady-state data.
    run_case(&conn, &warm_up_case).await?;
    wait_for_retained_executor_count(&conn, executor_count).await?;

    for case in BENCHMARK_MATRIX {
        results.push(run_case(&conn, case).await?);
    }

    print_results(&runtime, &results);

    Ok(())
}

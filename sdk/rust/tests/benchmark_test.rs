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

//! Benchmark test for Flame single-task latency and multi-session throughput.
//!
//! The same parameterized runner measures a matrix of session and task counts,
//! from a 1 × 1 round trip through concurrent throughput. Each matrix case must
//! complete within 10 minutes. Runtime-specific topology is selected only
//! through the benchmark environment.
//!

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    std::env::var("FLAME_BENCHMARK_RUNTIME").unwrap_or_else(|_| "Host".to_string())
}

fn get_ca_cert_path() -> String {
    let root = std::env::var("FLAME_ROOT").unwrap_or_else(|_| {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        format!("{}/../..", manifest_dir)
    });
    format!("{}/ci/certs/ca.crt", root)
}

const BENCHMARK_MATRIX: &[(usize, usize)] = &[(1, 1), (1, 1000), (10, 1), (10, 1000)];
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
    session_count: usize,
    tasks_per_session: usize,
    duration: Duration,
    succeeded: u64,
    failed: u64,
}

impl BenchmarkResult {
    fn total_tasks(&self) -> usize {
        self.session_count * self.tasks_per_session
    }

    fn throughput(&self) -> f64 {
        self.succeeded as f64 / self.duration.as_secs_f64()
    }
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

async fn run_benchmark(
    conn: &flame::client::Connection,
    session_count: usize,
    tasks_per_session: usize,
) -> Result<BenchmarkResult, FlameError> {
    let metrics = Arc::new(BenchmarkMetrics::new());
    let start = Instant::now();

    let mut session_handles = Vec::with_capacity(session_count);
    for session_index in 0..session_count {
        let conn = conn.clone();
        let metrics = metrics.clone();
        let session_id =
            format!("benchmark-{session_count}x{tasks_per_session}-ssn-{session_index}");
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
        session_count,
        tasks_per_session,
        duration: start.elapsed(),
        succeeded: metrics.succeeded.load(Ordering::Relaxed),
        failed: metrics.failed.load(Ordering::Relaxed),
    };
    wait_for_executors_to_settle(conn).await?;
    assert_eq!(result.failed, 0, "benchmark had failed tasks");
    assert_eq!(
        result.succeeded as usize,
        result.total_tasks(),
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

#[tokio::test]
async fn benchmark_task_matrix() -> Result<(), FlameError> {
    tracing_subscriber::fmt::try_init().ok();

    let runtime = benchmark_runtime();

    println!("\n============================================================");
    println!("{runtime} BENCHMARK");
    println!("============================================================\n");

    let tls_config = FlameClientTls {
        ca_file: Some(get_ca_cert_path()),
    };
    let conn = flame::client::connect_with_tls(&benchmark_endpoint(), Some(&tls_config)).await?;

    let mut results = Vec::with_capacity(BENCHMARK_MATRIX.len());
    for &(session_count, tasks_per_session) in BENCHMARK_MATRIX {
        results.push(run_benchmark(&conn, session_count, tasks_per_session).await?);
    }

    println!("\n============================================================");
    println!("{} BENCHMARK RESULTS", runtime.to_uppercase());
    println!("============================================================");
    println!("Sessions  Tasks/session  Total tasks  Duration(s)  Tasks/sec");
    for result in results {
        println!(
            "{:>8}  {:>13}  {:>11}  {:>11.2}  {:>9.2}",
            result.session_count,
            result.tasks_per_session,
            result.total_tasks(),
            result.duration.as_secs_f64(),
            result.throughput(),
        );
    }
    println!("============================================================\n");

    Ok(())
}

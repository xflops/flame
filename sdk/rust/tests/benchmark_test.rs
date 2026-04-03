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

//! Benchmark test for Flame multi-session throughput.
//!
//! This test creates 10 concurrent sessions, each running 1000 tasks,
//! for a total of 10,000 tasks. The benchmark must complete within 10 minutes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::try_join_all;
use stdng::new_ptr;

use flame::{
    apis::{FlameClientTls, FlameError, SessionState, TaskState},
    client::{SessionAttributes, Task, TaskInformer},
};
use flame_rs as flame;

const FLAME_ADDR: &str = "https://127.0.0.1:8080";
const FLAME_APP: &str = "flmping";

fn get_ca_cert_path() -> String {
    let root = std::env::var("FLAME_ROOT").unwrap_or_else(|_| {
        // Fallback: use CARGO_MANIFEST_DIR and navigate up
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        format!("{}/../..", manifest_dir)
    });
    format!("{}/ci/certs/ca.crt", root)
}

const NUM_SESSIONS: usize = 10;
const TASKS_PER_SESSION: usize = 1000;
const TOTAL_TASKS: usize = NUM_SESSIONS * TASKS_PER_SESSION;
const TIMEOUT_SECS: u64 = 600; // 10 minutes

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

/// Run tasks for a single session
async fn run_session(
    conn: &flame::client::Connection,
    session_id: usize,
    metrics: Arc<BenchmarkMetrics>,
) -> Result<(), FlameError> {
    let ssn_attr = SessionAttributes {
        id: format!("benchmark-ssn-{}", session_id),
        application: FLAME_APP.to_string(),
        slots: 1,
        common_data: None,
        min_instances: 0,
        max_instances: None,
    };

    let ssn = conn.create_session(&ssn_attr).await?;
    assert_eq!(ssn.state, SessionState::Open);

    // Submit all tasks for this session
    let mut task_handles = Vec::with_capacity(TASKS_PER_SESSION);
    for _ in 0..TASKS_PER_SESSION {
        let informer = new_ptr(BenchmarkTaskInformer::new(metrics.clone()));
        let handle = ssn.run_task(None, informer);
        task_handles.push(handle);
    }

    // Wait for all tasks to complete
    try_join_all(task_handles).await?;

    ssn.close().await?;
    Ok(())
}

#[tokio::test]
async fn benchmark_multi_session_throughput() -> Result<(), FlameError> {
    tracing_subscriber::fmt::try_init().ok();

    println!("\n============================================================");
    println!(
        "BENCHMARK: {} sessions × {} tasks = {} total",
        NUM_SESSIONS, TASKS_PER_SESSION, TOTAL_TASKS
    );
    println!("============================================================\n");

    let metrics = Arc::new(BenchmarkMetrics::new());
    let tls_config = FlameClientTls {
        ca_file: Some(get_ca_cert_path()),
        cert_file: None,
        key_file: None,
    };
    let conn = flame::client::connect_with_tls(FLAME_ADDR, Some(&tls_config)).await?;

    let start = Instant::now();

    // Create all sessions concurrently
    let mut session_handles = Vec::with_capacity(NUM_SESSIONS);
    for session_id in 0..NUM_SESSIONS {
        let conn = conn.clone();
        let metrics = metrics.clone();
        let handle = tokio::spawn(async move { run_session(&conn, session_id, metrics).await });
        session_handles.push(handle);
    }

    // Wait for all sessions
    for handle in session_handles {
        handle
            .await
            .map_err(|e| FlameError::Internal(e.to_string()))??;
    }

    let duration = start.elapsed();

    // Report results
    let succeeded = metrics.succeeded.load(Ordering::Relaxed);
    let failed = metrics.failed.load(Ordering::Relaxed);
    let throughput = succeeded as f64 / duration.as_secs_f64();

    println!("\n============================================================");
    println!("BENCHMARK RESULTS");
    println!("============================================================");
    println!("Duration:        {:.2}s", duration.as_secs_f64());
    println!("Succeeded:       {}/{}", succeeded, TOTAL_TASKS);
    println!("Failed:          {}", failed);
    println!("Throughput:      {:.2} tasks/sec", throughput);
    println!("============================================================\n");

    // Assertions for CI pass/fail
    assert_eq!(failed, 0, "Benchmark had {} failed tasks", failed);
    assert_eq!(
        succeeded as usize, TOTAL_TASKS,
        "Not all tasks succeeded: {}/{}",
        succeeded, TOTAL_TASKS
    );
    assert!(
        duration < Duration::from_secs(TIMEOUT_SECS),
        "Benchmark exceeded 10 minute timeout: {:.2}s",
        duration.as_secs_f64()
    );

    Ok(())
}

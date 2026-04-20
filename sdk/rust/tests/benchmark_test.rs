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
//!
//! Also includes gang scheduling tests to verify tasks don't start partially
//! when batch_size > 1 (all executors in a batch must be ready before tasks start).

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
        batch_size: 1,
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

const BATCH_SIZE: u32 = 2;
const BATCH_TIMEOUT_SECS: u64 = 120;

#[tokio::test]
async fn benchmark_gang_scheduling_no_partial_start() -> Result<(), FlameError> {
    tracing_subscriber::fmt::try_init().ok();

    println!("\n============================================================");
    println!("GANG SCHEDULING TEST: batch_size={}", BATCH_SIZE);
    println!("Verifying single task stays Pending until batch is complete...");
    println!("============================================================\n");

    let tls_config = FlameClientTls {
        ca_file: Some(get_ca_cert_path()),
    };
    let conn = flame::client::connect_with_tls(FLAME_ADDR, Some(&tls_config)).await?;

    let ssn_attr = SessionAttributes {
        id: format!("gang-test-{}", stdng::rand::short_name()),
        application: FLAME_APP.to_string(),
        slots: 1,
        common_data: None,
        min_instances: 0,
        max_instances: None,
        batch_size: BATCH_SIZE,
    };

    let ssn = conn.create_session(&ssn_attr).await?;
    assert_eq!(ssn.state, SessionState::Open);

    let start = Instant::now();

    println!("Step 1: Creating first task (should remain Pending)...");
    let task1 = ssn.create_task(None).await?;
    let task1_id = task1.id.clone();
    println!(
        "  Task 1 created: id={}, initial state={:?}",
        task1_id, task1.state
    );

    println!("Step 2: Waiting 3 seconds to verify task stays Pending...");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let task1_status = ssn.get_task(&task1_id).await?;
    println!("  Task 1 state after 3s: {:?}", task1_status.state);

    assert_eq!(
        task1_status.state,
        TaskState::Pending,
        "Task should remain Pending with batch_size=2 and only 1 task submitted. Got: {:?}",
        task1_status.state
    );

    println!("Step 3: Creating second task to complete the batch...");
    let task2 = ssn.create_task(None).await?;
    let task2_id = task2.id.clone();
    println!(
        "  Task 2 created: id={}, initial state={:?}",
        task2_id, task2.state
    );

    println!("Step 4: Waiting for both tasks to complete...");
    let timeout = Duration::from_secs(BATCH_TIMEOUT_SECS);
    let poll_interval = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;

    loop {
        let t1 = ssn.get_task(&task1_id).await?;
        let t2 = ssn.get_task(&task2_id).await?;

        let t1_done = matches!(t1.state, TaskState::Succeed | TaskState::Failed);
        let t2_done = matches!(t2.state, TaskState::Succeed | TaskState::Failed);

        if t1_done && t2_done {
            println!("  Task 1 final state: {:?}", t1.state);
            println!("  Task 2 final state: {:?}", t2.state);

            assert_eq!(t1.state, TaskState::Succeed, "Task 1 should succeed");
            assert_eq!(t2.state, TaskState::Succeed, "Task 2 should succeed");
            break;
        }

        if Instant::now() > deadline {
            panic!(
                "Timeout waiting for tasks to complete. Task1: {:?}, Task2: {:?}",
                t1.state, t2.state
            );
        }

        tokio::time::sleep(poll_interval).await;
    }

    ssn.close().await?;

    let duration = start.elapsed();

    println!("\n============================================================");
    println!("GANG SCHEDULING RESULTS");
    println!("============================================================");
    println!("Duration:              {:.2}s", duration.as_secs_f64());
    println!("Single task correctly stayed Pending until batch filled");
    println!("Both tasks succeeded after batch was complete");
    println!("============================================================\n");

    assert!(
        duration < Duration::from_secs(BATCH_TIMEOUT_SECS),
        "Gang scheduling test exceeded {} second timeout: {:.2}s",
        BATCH_TIMEOUT_SECS,
        duration.as_secs_f64()
    );

    Ok(())
}

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

use std::error::Error;
use std::time::{Duration, Instant};

use byte_unit::Byte;
use flame_rs::apis::FlameError;
use flame_rs::client::{Connection, Session, SessionOptions, Task, TaskInformer};
use flame_rs::IntoTaskInput;
use futures::future::{join_all, try_join_all};
use serde_derive::{Deserialize, Serialize};
use stdng::{lock_ptr, new_ptr, MutexPtr};
use tokio::task::JoinHandle;

use crate::cli::{Cli, APPLICATION};
use crate::stats::{latency, Latency};

/// The `flmping-service` request; it must match the worker's wire layout.
#[derive(Debug, Clone, Serialize, Deserialize, Default, flame_rs::FlameMessage)]
pub struct PingRequest {
    pub duration: Option<u64>,
    pub memory: Option<u64>,
}

/// How one task ended.
#[derive(Debug)]
pub enum Outcome {
    /// The watch callback saw the task reach a terminal state at `completed`.
    Done {
        started: Instant,
        completed: Instant,
        success: bool,
    },
    /// The task could not be created or watched.
    Error(String),
}

/// Stamps the time inside the watch callback, so the end-to-end time of a task is
/// measured when the update arrives and not when the caller gets around to it.
struct TaskCompletion {
    done: Option<(Instant, bool)>,
    error: Option<String>,
}

impl TaskCompletion {
    fn new() -> Self {
        Self {
            done: None,
            error: None,
        }
    }
}

impl TaskInformer for TaskCompletion {
    fn on_update(&mut self, task: Task) {
        if self.done.is_none() && task.state.is_terminal() {
            self.done = Some((
                Instant::now(),
                task.state == flame_rs::apis::TaskState::Succeed,
            ));
        }
    }

    fn on_error(&mut self, error: FlameError) {
        self.error.get_or_insert_with(|| error.to_string());
    }
}

/// Measurements of one session set running its tasks.
pub struct Burst {
    pub create: Duration,
    /// From the first submission until the last `create_task` returned.
    pub submit: Duration,
    /// From the last submission until the last task finished.
    pub drain: Duration,
    /// From the first submission until the last task finished.
    pub wall: Duration,
    pub submitted: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub submit_latency: Option<Latency>,
    pub latency: Option<Latency>,
    pub first_error: Option<String>,
}

pub fn request(cli: &Cli) -> Result<PingRequest, Box<dyn Error>> {
    let memory = match cli.memory.as_deref() {
        Some(size) => Some(Byte::parse_str(size, true)?.as_u64()),
        None => None,
    };
    Ok(PingRequest {
        duration: cli.duration,
        memory,
    })
}

fn session_options(cli: &Cli) -> SessionOptions {
    let mut options = SessionOptions::new(APPLICATION).priority(cli.priority);
    if let Some(resreq) = cli.resreq.as_deref() {
        options = options.resreq(resreq);
    }
    options
}

/// A created task whose watch is already running in the background. `started` and
/// `created` are when its `create_task` call started and returned.
struct TaskWatching {
    started: Instant,
    created: Instant,
    completion: MutexPtr<TaskCompletion>,
    watch: JoinHandle<()>,
}

impl TaskWatching {
    /// What the watch has seen so far; `None` while the task has neither finished
    /// nor failed to be watched.
    fn outcome(&self) -> Option<Outcome> {
        let completion = lock_ptr!(self.completion).unwrap();
        match (completion.done, &completion.error) {
            (Some((completed, success)), _) => Some(Outcome::Done {
                started: self.started,
                completed,
                success,
            }),
            (None, Some(error)) => Some(Outcome::Error(error.clone())),
            (None, None) => None,
        }
    }
}

/// Create one task and start watching it right away, so its completion is stamped
/// when it happens however long the other tasks take to submit. Nothing waits for
/// the task here.
async fn submit(session: &Session, input: &PingRequest) -> Result<TaskWatching, String> {
    let started = Instant::now();
    let task = IntoTaskInput::into_task_input(input).map_err(|error| error.to_string())?;
    let task = session
        .create_task(task)
        .await
        .map_err(|error| error.to_string())?;
    let created = Instant::now();

    let completion = new_ptr(TaskCompletion::new());
    let watch = tokio::spawn({
        let session = session.clone();
        let completion = completion.clone();
        // A watch failure reaches the informer through `on_error`.
        async move {
            let _ = session
                .watch_task(session.id.clone(), task.id, completion)
                .await;
        }
    });
    Ok(TaskWatching {
        started,
        created,
        completion,
        watch,
    })
}

/// Create the sessions, then in three steps: create every task of every session at
/// once, wait for all of them (at most `cli.timeout` after the last is created), and
/// finally read each task's result. Creating and closing the sessions is not part
/// of the measured time.
pub async fn run_burst(
    connection: &Connection,
    cli: &Cli,
    input: &PingRequest,
) -> Result<Burst, Box<dyn Error>> {
    let created = Instant::now();
    let sessions = try_join_all(
        (0..cli.sessions).map(|_| connection.create_session_with(session_options(cli))),
    )
    .await?;
    let create = created.elapsed();

    let total = cli.sessions * cli.tasks;
    let bench_start = Instant::now();

    // 1. Create the tasks.
    let created = join_all(
        sessions
            .iter()
            .flat_map(|session| (0..cli.tasks).map(move |_| submit(session, input))),
    )
    .await;
    let mut watching = Vec::with_capacity(total);
    let mut outcomes = Vec::with_capacity(total);
    for result in created {
        match result {
            Ok(task) => watching.push(task),
            Err(error) => outcomes.push(Outcome::Error(error)),
        }
    }

    // 2. Wait for every watch to finish, or for the deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cli.timeout);
    let aborts: Vec<_> = watching
        .iter()
        .map(|watching| watching.watch.abort_handle())
        .collect();
    let _ = tokio::time::timeout_at(
        deadline,
        join_all(watching.iter_mut().map(|watching| &mut watching.watch)),
    )
    .await;
    for abort in aborts {
        // Stops the watches that did not finish; it does nothing to finished ones.
        abort.abort();
    }

    // 3. Check what each task did.
    outcomes.extend(watching.iter().filter_map(TaskWatching::outcome));
    let burst = summarize(
        bench_start,
        create,
        &watching
            .iter()
            .map(|task| (task.started, task.created))
            .collect::<Vec<_>>(),
        &outcomes,
        total,
        (outcomes.len() < total).then(|| Duration::from_secs(cli.timeout)),
    );

    for session in &sessions {
        // A session that still has unfinished tasks refuses to close.
        if let Err(error) = session.close().await {
            eprintln!("warning: session <{}> was not closed: {error}", session.id);
        }
    }
    Ok(burst)
}

/// Turn the raw timings into a round's measurements. `submits` holds when each
/// task's `create_task` started and returned; `timeout` is set when tasks were still
/// outstanding at the deadline.
pub fn summarize(
    bench_start: Instant,
    create: Duration,
    submits: &[(Instant, Instant)],
    outcomes: &[Outcome],
    total: usize,
    timeout: Option<Duration>,
) -> Burst {
    let submit_end = submits.iter().map(|(_, created)| *created).max();
    let submit_latency = latency(
        submits
            .iter()
            .map(|(started, created)| *created - *started)
            .collect(),
    );

    let mut succeeded = 0;
    let mut first_error = None;
    let mut completed_last = None;
    let mut latencies = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        match outcome {
            Outcome::Done {
                started,
                completed,
                success,
            } => {
                completed_last = completed_last.max(Some(*completed));
                if *success {
                    succeeded += 1;
                    latencies.push(*completed - *started);
                }
            }
            Outcome::Error(error) => {
                first_error.get_or_insert_with(|| error.clone());
            }
        }
    }
    let outstanding = total - outcomes.len();
    if let (Some(timeout), true) = (timeout, outstanding > 0) {
        first_error.get_or_insert_with(|| {
            format!(
                "timed out after {}s with {outstanding} task(s) unfinished",
                timeout.as_secs()
            )
        });
    }

    let finished = completed_last.max(submit_end);
    let wall = match (timeout, finished) {
        (Some(timeout), _) => timeout,
        (None, Some(finished)) => finished.saturating_duration_since(bench_start),
        (None, None) => Duration::ZERO,
    };
    let submit = submit_end
        .map(|end| end.saturating_duration_since(bench_start))
        .unwrap_or(Duration::ZERO);

    Burst {
        create,
        submit,
        drain: wall.saturating_sub(submit),
        wall,
        submitted: total,
        succeeded,
        failed: total - succeeded,
        submit_latency,
        latency: latency(latencies),
        first_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn done(base: Instant, started: u64, completed: u64, success: bool) -> Outcome {
        Outcome::Done {
            started: base + ms(started),
            completed: base + ms(completed),
            success,
        }
    }

    fn submit(base: Instant, started: u64, created: u64) -> (Instant, Instant) {
        (base + ms(started), base + ms(created))
    }

    #[test]
    fn separates_submit_time_from_drain_time() {
        let base = Instant::now();
        let burst = summarize(
            base,
            ms(5),
            &[submit(base, 0, 10), submit(base, 0, 40)],
            &[done(base, 0, 100, true), done(base, 0, 300, true)],
            2,
            None,
        );
        assert_eq!(burst.submit, ms(40));
        assert_eq!(burst.wall, ms(300));
        assert_eq!(burst.drain, ms(260));
        assert_eq!(burst.succeeded, 2);
        let latency = burst.latency.unwrap();
        assert_eq!((latency.min_ms, latency.max_ms), (100.0, 300.0));
        let submit_latency = burst.submit_latency.unwrap();
        assert_eq!((submit_latency.min_ms, submit_latency.max_ms), (10.0, 40.0));
    }

    #[test]
    fn counts_failed_and_errored_tasks_and_keeps_latency_to_successes() {
        let base = Instant::now();
        let burst = summarize(
            base,
            ms(0),
            &[submit(base, 0, 5), submit(base, 0, 5), submit(base, 0, 5)],
            &[
                done(base, 0, 50, true),
                done(base, 0, 500, false),
                Outcome::Error("boom".to_string()),
            ],
            3,
            None,
        );
        assert_eq!((burst.succeeded, burst.failed), (1, 2));
        assert_eq!(burst.first_error.as_deref(), Some("boom"));
        assert_eq!(burst.latency.unwrap().max_ms, 50.0);
        assert_eq!(burst.wall, ms(500));
    }

    #[test]
    fn a_timeout_reports_the_unfinished_tasks() {
        let base = Instant::now();
        let burst = summarize(
            base,
            ms(0),
            &[submit(base, 0, 5), submit(base, 0, 6)],
            &[done(base, 0, 50, true)],
            2,
            Some(Duration::from_secs(30)),
        );
        assert_eq!((burst.succeeded, burst.failed), (1, 1));
        assert_eq!(burst.wall, Duration::from_secs(30));
        assert!(burst
            .first_error
            .unwrap()
            .starts_with("timed out after 30s"));
    }

    #[test]
    fn nothing_submitted_has_no_timings() {
        let burst = summarize(Instant::now(), ms(0), &[], &[], 0, None);
        assert_eq!(
            (burst.submit, burst.wall, burst.drain),
            (ms(0), ms(0), ms(0))
        );
        assert!(burst.latency.is_none() && burst.submit_latency.is_none());
    }

    fn task(state: flame_rs::apis::TaskState) -> Task {
        Task {
            id: "1".to_string(),
            ssn_id: "s".to_string(),
            state,
            input: None,
            output: None,
            affinity: Default::default(),
            events: Vec::new(),
        }
    }

    #[test]
    fn the_callback_stamps_only_the_first_terminal_update() {
        use flame_rs::apis::TaskState;
        let mut completion = TaskCompletion::new();
        completion.on_update(task(TaskState::Running));
        assert!(completion.done.is_none());
        completion.on_update(task(TaskState::Succeed));
        let first = completion.done.unwrap();
        assert!(first.1);
        completion.on_update(task(TaskState::Failed));
        assert_eq!(
            completion.done.unwrap().0,
            first.0,
            "later updates are ignored"
        );
    }

    #[test]
    fn a_failed_task_is_not_a_success() {
        use flame_rs::apis::TaskState;
        let mut completion = TaskCompletion::new();
        completion.on_update(task(TaskState::Failed));
        assert_eq!(completion.done.map(|done| done.1), Some(false));
    }

    #[test]
    fn the_first_watch_error_is_kept() {
        let mut completion = TaskCompletion::new();
        completion.on_error(FlameError::Network("first".to_string()));
        completion.on_error(FlameError::Network("second".to_string()));
        assert_eq!(completion.error.as_deref(), Some("first"));
    }

    async fn watching(completion: TaskCompletion) -> TaskWatching {
        let now = Instant::now();
        TaskWatching {
            started: now,
            created: now,
            completion: new_ptr(completion),
            watch: tokio::spawn(async {}),
        }
    }

    #[tokio::test]
    async fn a_watch_with_no_news_has_no_outcome_yet() {
        assert!(watching(TaskCompletion::new()).await.outcome().is_none());
    }

    #[tokio::test]
    async fn the_outcome_is_read_from_what_the_callback_recorded() {
        let mut done = TaskCompletion::new();
        done.done = Some((Instant::now(), true));
        assert!(matches!(
            watching(done).await.outcome(),
            Some(Outcome::Done { success: true, .. })
        ));

        let mut failed = TaskCompletion::new();
        failed.error = Some("stream broke".to_string());
        assert!(matches!(
            watching(failed).await.outcome(),
            Some(Outcome::Error(error)) if error == "stream broke"
        ));
    }

    #[test]
    fn request_parses_memory_sizes() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["flmbench", "-d", "7", "-m", "2KiB"]).unwrap();
        let input = request(&cli).unwrap();
        assert_eq!(input.duration, Some(7));
        assert_eq!(input.memory, Some(2048));
    }
}

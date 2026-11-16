/*
Copyright 2023 The Flame Authors.
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

//! Controller-owned history collection, isolated from task-serving runtimes.
use common::{apis::SessionFilter, ctx::FlameClusterContext, FlameError};
use std::time::Duration;

use super::*;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

pub(super) struct State {
    retention: Duration,
    history: Option<PathBuf>,
}

impl State {
    pub(super) fn new(context: &FlameClusterContext) -> Self {
        let flame_home = std::env::var_os(common::FLAME_HOME)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/local/flame"));
        Self {
            retention: context.cluster.history.retention,
            history: history_directory(&context.cluster.storage, &flame_home),
        }
    }
}

impl Controller {
    /// Run only on the history worker, including final directory removal.
    pub async fn collect_history(&self) -> Result<(), FlameError> {
        self.collect_history_at(Utc::now()).await
    }

    pub(super) async fn collect_history_at(&self, now: DateTime<Utc>) -> Result<(), FlameError> {
        let retention = self.gc.retention;
        let age = chrono::Duration::from_std(retention)
            .map_err(|_| FlameError::InvalidConfig("history retention overflow".into()))?;
        let cutoff = now
            .checked_sub_signed(age)
            .ok_or_else(|| FlameError::InvalidConfig("history cutoff overflow".into()))?;
        let started = std::time::Instant::now();
        let mut number = 1;
        let mut examined = 0;
        let mut removed = 0;
        let mut deferred = 0;
        let mut failed = 0;
        loop {
            let filter = SessionFilter::by_state(SessionState::Closed).with_paging(
                common::apis::Paging::new()
                    .with_limit(PAGE_SIZE)?
                    .with_number(number)?,
            );
            let candidates = self.storage.list_sessions(Some(&filter)).await?;
            examined += candidates.len();
            let more = (candidates.paging.offset().saturating_add(candidates.len()) as u64)
                < candidates.paging.total().unwrap_or(0);
            let previous_removed = removed;
            for observed in candidates {
                let expected = match self.storage.get_session_ptr(observed.id.clone()) {
                    Ok(expected) => expected,
                    Err(FlameError::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                };
                let eligible = expected.try_lock().is_some_and(|session| {
                    session.version == observed.version
                        && session.creation_time == observed.creation_time
                        && session.application == observed.application
                        && expired(&session, now, cutoff)
                });
                if !eligible {
                    deferred += 1;
                    continue;
                }
                match self.stale_session(&expected).await {
                    Ok(true) => removed += 1,
                    Ok(false) => deferred += 1,
                    Err(error) => {
                        failed += 1;
                        tracing::warn!(session_id = %observed.id, %error, "history removal deferred");
                    }
                }
                tokio::task::yield_now().await;
            }
            // Removed entries shift the next candidates into the current page.
            if removed == previous_removed {
                if !more {
                    break;
                }
                number += 1;
            }
        }
        if let Some(history) = &self.gc.history {
            clear_history(history)?;
        }
        tracing::info!(
            examined,
            removed,
            deferred,
            failed,
            elapsed_ms = started.elapsed().as_millis(),
            "session history collection completed"
        );
        Ok(())
    }
}

/// Resolve the same local storage URL aliases and relative base as the filesystem engine.
pub(super) fn history_directory(storage: &str, flame_home: &Path) -> Option<PathBuf> {
    let path = ["filesystem://", "file://", "fs://"]
        .into_iter()
        .find_map(|prefix| storage.strip_prefix(prefix))?;
    let base = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        flame_home.join(path)
    };
    Some(base.join(".history"))
}

/// Remove all detached session directories, keeping the history root.
pub(super) fn clear_history(history: &Path) -> Result<(), FlameError> {
    let metadata = match std::fs::symlink_metadata(history) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir(),
        FlameError::Storage("History path is not a directory".into())
    )?;
    for entry in std::fs::read_dir(history)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(super) fn expired(session: &Session, now: DateTime<Utc>, cutoff: DateTime<Utc>) -> bool {
    let completed = session.completion_time.unwrap_or(session.creation_time);
    if completed < session.creation_time || completed > now {
        tracing::warn!(session_id = %session.id, creation_time = %session.creation_time, completion_time = %completed,
            "invalid history timestamp; retaining session");
        return false;
    }
    session.is_closed() && completed < cutoff
}

const PAGE_SIZE: i32 = 100;
const INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

pub(super) fn collection_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

impl super::Controller {
    pub async fn run_gc(self: std::sync::Arc<Self>) -> Result<(), FlameError> {
        let controller = self;
        let retention = controller.gc.retention;
        tracing::info!(
            retention_seconds = retention.as_secs(),
            interval_seconds = INTERVAL.as_secs(),
            "session history collection started"
        );
        let mut interval = collection_interval();
        loop {
            interval.tick().await;
            let controller = controller.clone();
            let runtime = tokio::runtime::Handle::current();
            // Await each blocking worker: there is one operation in flight and no backlog.
            let result =
                tokio::task::spawn_blocking(move || runtime.block_on(controller.collect_history()))
                    .await
                    .map_err(|error| {
                        FlameError::Internal(format!("history worker failed: {error}"))
                    })?;
            if let Err(error) = result {
                tracing::warn!(%error, "session history collection deferred");
            }
        }
    }
}

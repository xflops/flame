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

use super::super::gc::{clear_history, collection_interval, expired, history_directory};
use crate::controller::{Controller, ControllerPtr};
use chrono::{Duration, Utc};
use common::apis::{
    ApplicationAttributes, ExecutorState, Node, Session, SessionAttributes, SessionState,
    SessionStatus,
};
use common::ctx::FlameClusterContext;
use futures::FutureExt;
use std::{fs, path::Path, time::Duration as StdDuration};

async fn fixture() -> ControllerPtr {
    let mut context = FlameClusterContext::default();
    context.cluster.storage = "none".into();
    let storage = common::storage::new_ptr(&context).await.unwrap();
    storage
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    crate::controller::new_ptr(storage)
}

async fn create(controller: &Controller, id: &str) {
    controller
        .create_session(SessionAttributes {
            id: id.into(),
            application: "app".into(),
            resreq: Some(Default::default()),
            ..Default::default()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn closed_history_expires_only_after_cutoff() {
    let controller = fixture().await;
    let storage = controller.storage();
    create(&controller, "closed").await;
    controller.close_session("closed".into()).await.unwrap();
    let completed = storage
        .get_session("closed".into())
        .unwrap()
        .completion_time
        .unwrap();
    controller
        .collect_history_at(completed + Duration::days(30))
        .await
        .unwrap();
    assert!(storage.get_session("closed".into()).is_ok());
    controller
        .collect_history_at(completed + Duration::days(30) + Duration::seconds(1))
        .await
        .unwrap();
    assert!(storage.list_sessions(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn closed_history_retires_without_changing_assigned_executor() {
    let controller = fixture().await;
    let storage = controller.storage();
    create(&controller, "open").await;
    assert!(storage.get_session("open".into()).unwrap().is_open());
    create(&controller, "closed").await;
    storage
        .register_node(&Node {
            name: "node".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut executor = storage
        .create_executor("node".into(), "closed".into())
        .await
        .unwrap();
    executor.ssn_id = Some("closed".into());
    executor.state = ExecutorState::Unbinding;
    storage.update_executor(&executor).await.unwrap();
    controller.close_session("closed".into()).await.unwrap();
    let expected = storage.get_session_ptr("closed".into()).unwrap();
    assert!(controller.stale_session(&expected).await.unwrap());
    assert!(storage.get_session("closed".into()).is_err());
    let retained = storage.get_executor_ptr(executor.id).unwrap();
    let retained = stdng::lock_ptr!(retained).unwrap();
    assert_eq!(retained.ssn_id.as_deref(), Some("closed"));
    assert_eq!(retained.state, ExecutorState::Unbinding);
    assert!(storage.get_session("open".into()).unwrap().is_open());
}

#[tokio::test]
async fn stale_candidate_cannot_delete_recreated_id() {
    let controller = fixture().await;
    let storage = controller.storage();
    create(&controller, "same").await;
    controller.close_session("same".into()).await.unwrap();
    let observed = storage.get_session("same".into()).unwrap();
    let expected = storage.get_session_ptr("same".into()).unwrap();
    controller.delete_session("same".into()).await.unwrap();
    create(&controller, "same").await;
    controller.close_session("same".into()).await.unwrap();
    // Match the old metadata to exercise the pointer identity check itself.
    {
        let replacement = storage.get_session_ptr("same".into()).unwrap();
        let mut session = stdng::lock_ptr!(replacement).unwrap();
        session.version = observed.version;
        session.creation_time = observed.creation_time;
        session.completion_time = observed.completion_time;
    }
    assert!(!controller.stale_session(&expected).await.unwrap());
    assert!(storage.get_session("same".into()).is_ok());
}

#[tokio::test]
async fn open_session_cannot_be_staled() {
    let controller = fixture().await;
    create(&controller, "open").await;
    let expected = controller.storage().get_session_ptr("open".into()).unwrap();
    assert!(!controller.stale_session(&expected).await.unwrap());
    assert!(controller.delete_session("open".into()).await.is_err());
    assert!(controller
        .storage()
        .get_session("open".into())
        .unwrap()
        .is_open());
}

/// Closed is authoritative even when the cached final counters contain residual work.
#[tokio::test]
async fn closed_retirement_does_not_revalidate_cached_task_counters() {
    let controller = fixture().await;
    let storage = controller.storage();
    for id in ["stale", "manual"] {
        create(&controller, id).await;
        controller.close_session(id.into()).await.unwrap();
        let shared = storage.get_session_ptr(id.into()).unwrap();
        let mut session = stdng::lock_ptr!(shared).unwrap();
        session.task_counts.pending = 1;
        session.task_counts.running = 1;
    }
    let expected = storage.get_session_ptr("stale".into()).unwrap();
    assert!(controller.stale_session(&expected).await.unwrap());
    controller.delete_session("manual".into()).await.unwrap();
    assert!(storage.list_sessions(None).await.unwrap().is_empty());
}

/// A busy Session mutex defers direct revalidation without blocking unrelated work.
#[tokio::test]
async fn busy_session_mutex_does_not_block_other_ids() {
    let controller = fixture().await;
    let storage = controller.storage();
    create(&controller, "first").await;
    create(&controller, "second").await;
    controller.close_session("first".into()).await.unwrap();
    let expected = storage.get_session_ptr("first".into()).unwrap();
    {
        let _busy = stdng::lock_ptr!(expected).unwrap();
        assert!(!controller
            .stale_session(&expected)
            .now_or_never()
            .expect("a busy mutex must defer immediately")
            .unwrap());
    }
    storage
        .create_task("second".into(), None, None)
        .await
        .unwrap();
    assert!(storage.get_session("first".into()).unwrap().is_closed());
}

#[test]
fn legacy_and_invalid_timestamps_are_safe() {
    let now = Utc::now();
    let cutoff = now - Duration::days(30);
    let mut session = Session {
        creation_time: now - Duration::days(31),
        status: SessionStatus {
            state: SessionState::Closed,
        },
        ..Default::default()
    };
    assert!(expired(&session, now, cutoff));
    session.completion_time = Some(now + Duration::seconds(1));
    assert!(!expired(&session, now, cutoff));
    session.completion_time = Some(session.creation_time - Duration::seconds(1));
    assert!(!expired(&session, now, cutoff));
}

/// Collection starts after recovery and then keeps the fixed four-hour cadence.
#[tokio::test(start_paused = true)]
async fn collection_timer_waits_four_hours_and_skips_missed_passes() {
    let mut interval = collection_interval();
    let first = interval.tick().await;
    assert_eq!(
        interval.tick().await - first,
        StdDuration::from_secs(4 * 60 * 60)
    );

    // A long pass must not queue a burst of catch-up passes.
    tokio::time::advance(StdDuration::from_secs(13 * 60 * 60)).await;
    interval.tick().await;
    let next = interval.tick().await;
    assert!(next > tokio::time::Instant::now() - StdDuration::from_secs(1));
}

/// Closed history is eligible regardless of residual assignment; live sessions and executors remain.
#[tokio::test]
async fn collection_removes_closed_history_and_preserves_open_session_and_executor() {
    let controller = fixture().await;
    let storage = controller.storage();
    create(&controller, "open").await;
    create(&controller, "unbinding").await;
    create(&controller, "expired").await;
    storage
        .register_node(&Node {
            name: "node".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut executor = storage
        .create_executor("node".into(), "unbinding".into())
        .await
        .unwrap();
    executor.ssn_id = Some("unbinding".into());
    executor.state = ExecutorState::Unbinding;
    storage.update_executor(&executor).await.unwrap();
    storage.close_session("unbinding".into()).await.unwrap();
    storage.close_session("expired".into()).await.unwrap();

    let now = chrono::Utc::now() + chrono::Duration::days(40);
    controller.collect_history_at(now).await.unwrap();
    assert!(storage.get_session("expired".into()).is_err());
    assert_eq!(
        storage.get_session("open".into()).unwrap().status.state,
        SessionState::Open
    );
    assert!(storage.get_session("unbinding".into()).is_err());
    let retained = storage.get_executor_ptr(executor.id).unwrap();
    let retained = stdng::lock_ptr!(retained).unwrap();
    assert_eq!(retained.ssn_id.as_deref(), Some("unbinding"));
    assert_eq!(retained.state, ExecutorState::Unbinding);
    assert!(storage.get_session("open".into()).unwrap().is_open());
}

/// An invalid timestamp defers one candidate without blocking collection or live tasks.
#[tokio::test]
async fn invalid_timestamp_does_not_block_another_session() {
    let controller = fixture().await;
    let storage = controller.storage();
    create(&controller, "invalid").await;
    create(&controller, "expired").await;
    create(&controller, "live").await;
    storage.close_session("invalid".into()).await.unwrap();
    storage.close_session("expired".into()).await.unwrap();
    let now = chrono::Utc::now() + chrono::Duration::days(40);
    let invalid = storage.get_session_ptr("invalid".into()).unwrap();
    {
        let mut session = stdng::lock_ptr!(invalid).unwrap();
        session.completion_time = Some(session.creation_time - chrono::Duration::seconds(1));
    }
    controller.collect_history_at(now).await.unwrap();
    assert!(storage.get_session("invalid".into()).is_ok());
    assert!(storage.get_session("expired".into()).is_err());
    storage
        .create_task("live".into(), None, None)
        .await
        .unwrap();
    {
        let mut session = stdng::lock_ptr!(invalid).unwrap();
        session.completion_time = Some(session.creation_time);
    }
    controller.collect_history_at(now).await.unwrap();
    assert!(storage.get_session("invalid".into()).is_err());
    assert!(storage.get_session("live".into()).unwrap().is_open());
}

/// Storage queries include sessions restored before the Controller was initialized.
#[tokio::test]
async fn controller_collects_history_present_before_initialization() {
    let mut context = FlameClusterContext::default();
    context.cluster.storage = "none".into();
    let storage = common::storage::new_ptr(&context).await.unwrap();
    storage
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    storage
        .create_session(SessionAttributes {
            id: "restored".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage.close_session("restored".into()).await.unwrap();
    let controller = crate::controller::new_ptr(storage.clone());
    controller
        .collect_history_at(chrono::Utc::now() + chrono::Duration::days(40))
        .await
        .unwrap();
    assert!(storage.get_session("restored".into()).is_err());
}

/// GC applies its completion cutoff after listing Closed session metadata.
#[tokio::test]
async fn collection_filters_closed_history_by_completion_cutoff() {
    let controller = fixture().await;
    let storage = controller.storage();
    let now = chrono::Utc::now() + chrono::Duration::days(40);
    let cutoff = now - chrono::Duration::days(30);
    for id in ["expired", "boundary", "recent", "future", "open"] {
        create(&controller, id).await;
        if id != "open" {
            controller.close_session(id.into()).await.unwrap();
        }
    }
    for (id, completed) in [
        ("boundary", cutoff),
        ("recent", now - chrono::Duration::days(1)),
        ("future", now + chrono::Duration::seconds(1)),
    ] {
        let shared = storage.get_session_ptr(id.into()).unwrap();
        stdng::lock_ptr!(shared).unwrap().completion_time = Some(completed);
    }
    controller.collect_history_at(now).await.unwrap();
    assert!(storage.get_session("expired".into()).is_err());
    for id in ["boundary", "recent", "future", "open"] {
        assert!(storage.get_session(id.into()).is_ok(), "retained {id}");
    }
}

/// A full first page of recent sessions must not starve expired history on later pages.
#[tokio::test]
async fn collection_reaches_later_pages_when_first_page_is_deferred() {
    let controller = fixture().await;
    let storage = controller.storage();
    for index in 0..101 {
        let id = format!("history-{index}");
        create(&controller, &id).await;
        controller.close_session(id).await.unwrap();
    }
    let now = chrono::Utc::now() + chrono::Duration::days(40);
    let filter = common::apis::SessionFilter::by_state(SessionState::Closed)
        .with_paging(common::apis::Paging::new());
    let page = storage.list_sessions(Some(&filter)).await.unwrap();
    assert_eq!(page.len(), 100);
    for session in page {
        let shared = storage.get_session_ptr(session.id).unwrap();
        let mut session = stdng::lock_ptr!(shared).unwrap();
        session.completion_time = Some(now - chrono::Duration::days(1));
    }
    controller.collect_history_at(now).await.unwrap();
    assert_eq!(storage.list_sessions(None).await.unwrap().len(), 100);
}

#[tokio::test]
async fn collection_removes_multiple_pages_without_skipping_shifted_sessions() {
    let controller = fixture().await;
    for index in 0..205 {
        let id = format!("expired-{index}");
        create(&controller, &id).await;
        controller.close_session(id).await.unwrap();
    }
    controller
        .collect_history_at(Utc::now() + Duration::days(40))
        .await
        .unwrap();
    assert!(controller
        .storage()
        .list_sessions(None)
        .await
        .unwrap()
        .is_empty());
}

#[test]
fn storage_aliases_resolve_absolute_and_relative_history_paths() {
    for prefix in ["filesystem://", "file://", "fs://"] {
        assert_eq!(
            history_directory(&format!("{prefix}/data/flame"), Path::new("/home/flame")),
            Some("/data/flame/.history".into())
        );
        assert_eq!(
            history_directory(&format!("{prefix}data"), Path::new("/home/flame")),
            Some("/home/flame/data/.history".into())
        );
    }
    assert!(history_directory("none", Path::new("/home/flame")).is_none());
}

#[tokio::test]
async fn restart_cleanup_finds_staged_history_and_preserves_reused_live_session() {
    let directory = tempfile::tempdir().unwrap();
    let mut context = common::ctx::FlameClusterContext::default();
    context.cluster.storage = format!("fs://{}", directory.path().display());
    let storage = common::storage::new_ptr(&context).await.unwrap();
    let attributes = common::apis::SessionAttributes {
        id: "same".into(),
        application: "app".into(),
        ..Default::default()
    };
    storage.create_session(attributes.clone()).await.unwrap();
    storage.close_session("same".into()).await.unwrap();
    storage
        .stale_session(&storage.get_session_ptr("same".into()).unwrap())
        .await
        .unwrap();
    let history = directory.path().join(".history");
    assert!(fs::read_dir(&history).unwrap().next().is_some());
    storage.create_session(attributes).await.unwrap();
    let recovered = common::storage::new_ptr(&context).await.unwrap();
    recovered.load_data().await.unwrap();
    // Constructor initialization discovers detached history without first running run_gc.
    let controller = crate::controller::new_ptr(recovered.clone());
    controller.collect_history().await.unwrap();
    assert!(fs::read_dir(&history).unwrap().next().is_none());
    assert!(recovered.get_session("same".into()).unwrap().is_open());
    assert!(directory.path().join("sessions/same/metadata").exists());
}

#[test]
fn cleanup_removes_all_subdirectories_and_never_follows_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let history = directory.path().join(".history");
    fs::create_dir(&history).unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("keep"), b"keep").unwrap();
    for id in 0..120 {
        fs::create_dir(history.join(format!("ignored-{id}"))).unwrap();
    }
    std::os::unix::fs::symlink(
        external.path(),
        history.join(uuid::Uuid::new_v4().to_string()),
    )
    .unwrap();
    let staged = history.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&staged).unwrap();
    std::os::unix::fs::symlink(external.path(), staged.join("link")).unwrap();
    fs::write(history.join("keep-file"), b"keep").unwrap();
    clear_history(&history).unwrap();
    assert!(history.is_dir());
    assert!(!staged.exists());
    assert!(external.path().join("keep").exists());
    assert!(!history.join("ignored-0").exists());
    assert!(history.join("keep-file").exists());
    clear_history(&history).unwrap();
    clear_history(&directory.path().join("missing")).unwrap();
    let redirected = directory.path().join("redirected");
    std::os::unix::fs::symlink(external.path(), &redirected).unwrap();
    assert!(clear_history(&redirected).is_err());
}

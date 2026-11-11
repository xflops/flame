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

use super::*;
use crate::controller::gc::{
    classify_stale, validate_applications, validate_sessions, ApplicationState,
    GarbageCollectableStorage, GarbageCollector, LivenessProvider, LivenessSnapshot, StaleReason,
};
use async_trait::async_trait;
use rpc::flame::v1 as flame_rpc;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Build one frontend application record with explicit lifecycle fields.
fn application(name: &str, state: i32, time: i64) -> flame_rpc::Application {
    flame_rpc::Application {
        metadata: Some(flame_rpc::Metadata {
            id: name.into(),
            name: name.into(),
        }),
        status: Some(flame_rpc::ApplicationStatus {
            state,
            creation_time: time,
        }),
        ..Default::default()
    }
}
/// Build a session whose application owner is carried by its spec rather than display name.
fn session(app: &str, id: &str, state: i32) -> flame_rpc::Session {
    flame_rpc::Session {
        metadata: Some(flame_rpc::Metadata {
            id: id.into(),
            name: "display name".into(),
        }),
        spec: Some(flame_rpc::SessionSpec {
            application: app.into(),
            ..Default::default()
        }),
        status: Some(flame_rpc::SessionStatus {
            state,
            creation_time: 1,
            ..Default::default()
        }),
    }
}
/// Validate fixture RPC lists through the same boundary used by the real frontend provider.
fn snapshot(
    applications: Vec<flame_rpc::Application>,
    sessions: Vec<flame_rpc::Session>,
) -> LivenessSnapshot {
    LivenessSnapshot {
        applications: validate_applications(applications).unwrap(),
        sessions: validate_sessions(sessions).unwrap(),
    }
}
/// Build one filesystem object descriptor with a deterministic creation timestamp.
fn object(key: &str, time: i64) -> ObjectMetadata {
    ObjectMetadata::new()
        .with_key(key)
        .with_version(1)
        .with_size(4)
        .with_creation_time(time)
}
/// Parse the complete ownership key used by classifier assertions.
fn key(value: &str) -> ObjectKey {
    ObjectKey::try_from(value).unwrap()
}

struct MockProvider {
    responses: VecDeque<Result<LivenessSnapshot, FlameError>>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl LivenessProvider for MockProvider {
    /// Return one complete scripted snapshot and count freshness checks.
    async fn snapshot(&mut self) -> Result<LivenessSnapshot, FlameError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses
            .pop_front()
            .expect("unexpected liveness lookup")
    }
}
struct MockStorage {
    objects: Vec<ObjectMetadata>,
    deleted: Mutex<Vec<String>>,
    failures: HashSet<String>,
}
#[async_trait]
impl GarbageCollectableStorage for MockStorage {
    /// Return fixture metadata without simulating public object deletion or recreation.
    async fn list(&self, filter: &ObjectFilter) -> Result<Vec<ObjectMetadata>, FlameError> {
        Ok(self
            .objects
            .iter()
            .filter(|object| filter.matches(&object.key))
            .cloned()
            .collect())
    }
    /// Record exact internal deletion, or return its injected storage error.
    async fn delete(&self, key: &ObjectKey) -> Result<(), FlameError> {
        let key = key.to_key().unwrap();
        if self.failures.contains(&key) {
            return Err(FlameError::Storage("injected deletion failure".into()));
        }
        self.deleted.lock().unwrap().push(key);
        Ok(())
    }
}
/// Build observable storage without deletion failures.
fn storage(objects: Vec<ObjectMetadata>) -> Arc<MockStorage> {
    Arc::new(MockStorage {
        objects,
        deleted: Mutex::new(Vec::new()),
        failures: HashSet::new(),
    })
}
/// Inject full application/session snapshots and expose the number of refreshes.
fn collector(
    storage: Arc<dyn GarbageCollectableStorage>,
    responses: Vec<Result<LivenessSnapshot, FlameError>>,
) -> (GarbageCollector, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        GarbageCollector::with_dependencies(
            storage,
            Box::new(MockProvider {
                responses: responses.into(),
                calls: Arc::clone(&calls),
            }),
        ),
        calls,
    )
}

/// Retain every existing session state, including closed sessions and ownership from spec.application.
#[test]
fn existing_open_closed_and_suspended_sessions_retain_data() {
    let live = snapshot(
        Vec::new(),
        vec![
            session("app", "open", 0),
            session("app", "closed", 1),
            session("app", "suspending", 2),
            session("app", "suspended", 3),
        ],
    );
    for id in ["open", "closed", "suspending", "suspended"] {
        assert_eq!(
            classify_stale(&live, &key(&format!("app/{id}/object")), 1, 100_000),
            None
        );
    }
    assert_eq!(
        classify_stale(&live, &key("other/closed/object"), 1, 100_000),
        Some(StaleReason::MissingSession)
    );
}

/// Missing ownership expires strictly after ten seconds, while future or overflowed timestamps stay retained.
#[test]
fn missing_ownership_uses_strict_creation_grace() {
    let live = LivenessSnapshot::default();
    for (path, reason) in [
        ("app/session/object", StaleReason::MissingSession),
        ("app/pkg/archive", StaleReason::MissingApplication),
    ] {
        let key = key(path);
        assert_eq!(classify_stale(&live, &key, 9_999, 20_000), Some(reason));
        assert_eq!(classify_stale(&live, &key, 10_000, 20_000), None);
        assert_eq!(classify_stale(&live, &key, 20_001, 20_000), None);
        assert_eq!(classify_stale(&live, &key, i64::MAX, i64::MAX), None);
    }
}

/// Package data follows application incarnations while disabled and current applications retain their packages.
#[test]
fn package_namespace_is_owned_by_application() {
    let live = snapshot(
        vec![
            application("app", 0, 20_000),
            application("disabled", 1, 20_000),
        ],
        vec![session("missing", "pkg", 1)],
    );
    assert_eq!(
        classify_stale(&live, &key("app/pkg/old"), 1, 40_000),
        Some(StaleReason::PreviousApplication)
    );
    assert_eq!(
        classify_stale(&live, &key("app/pkg/current"), 20_000, 40_000),
        None
    );
    assert_eq!(
        classify_stale(&live, &key("disabled/pkg/old"), 1, 40_000),
        None
    );
    assert_eq!(
        classify_stale(&live, &key("missing/pkg/old"), 1, 40_000),
        Some(StaleReason::MissingApplication)
    );
}

/// Reject absent, ambiguous, or invalid session identity and status fields before they authorize deletion.
#[test]
fn session_validation_fails_closed_for_malformed_lists() {
    let valid = session("app", "id", 0);
    let mut cases = Vec::new();
    let mut item = valid.clone();
    item.metadata = None;
    cases.push(vec![item]);
    let mut item = valid.clone();
    item.spec = None;
    cases.push(vec![item]);
    let mut item = valid.clone();
    item.status = None;
    cases.push(vec![item]);
    for (app, id) in [
        ("", "id"),
        ("app", ""),
        ("app/nested", "id"),
        ("app", "id/nested"),
        ("app", "*"),
    ] {
        cases.push(vec![session(app, id, 0)]);
    }
    cases.push(vec![session("app", "id", 99)]);
    let mut item = valid.clone();
    item.status.as_mut().unwrap().creation_time = 0;
    cases.push(vec![item]);
    cases.push(vec![valid.clone(), valid.clone()]);
    cases.push(vec![valid, session("other", "id", 1)]);
    for records in cases {
        assert!(validate_sessions(records).is_err());
    }
}

/// Reject malformed application records and preserve valid raw state and incarnation times.
#[test]
fn application_validation_fails_closed_for_malformed_lists() {
    let valid = application("app", 0, 11);
    let mut missing_metadata = valid.clone();
    missing_metadata.metadata = None;
    let mut missing_status = valid.clone();
    missing_status.status = None;
    for records in [
        vec![missing_metadata],
        vec![missing_status],
        vec![application("", 0, 1)],
        vec![application("app/nested", 0, 1)],
        vec![application("app", 99, 1)],
        vec![application("app", 0, 0)],
        vec![valid.clone(), valid],
    ] {
        assert!(validate_applications(records).is_err());
    }
    let applications = validate_applications(vec![
        application("enabled", 0, 11),
        application("disabled", 1, 22),
    ])
    .unwrap();
    assert_eq!(applications["enabled"].creation_time, 11);
    assert_eq!(applications["disabled"].state, ApplicationState::Disabled);
}

/// Live sessions require no destructive freshness query, regardless of whether they are open or closed.
#[tokio::test]
async fn live_session_objects_are_not_candidates() {
    let storage = storage(vec![
        object("app/open/object", 1),
        object("app/closed/object", 1),
    ]);
    let live = snapshot(
        Vec::new(),
        vec![session("app", "open", 0), session("app", "closed", 1)],
    );
    let (mut collector, calls) = collector(storage.clone(), vec![Ok(live)]);
    let stats = collector.reconcile(100_000).await.unwrap();
    assert_eq!((stats.candidates, stats.deleted), (0, 0));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(storage.deleted.lock().unwrap().is_empty());
}

/// Confirm absence twice and delete only objects older than the creation grace period.
#[tokio::test]
async fn absent_sessions_delete_old_exact_objects_after_grace() {
    let storage = storage(vec![
        object("app/gone/old", 1),
        object("app/gone/young", 10_000),
    ]);
    let (mut collector, calls) = collector(
        storage.clone(),
        vec![
            Ok(LivenessSnapshot::default()),
            Ok(LivenessSnapshot::default()),
        ],
    );
    let stats = collector.reconcile(20_000).await.unwrap();
    assert_eq!((stats.candidates, stats.deleted, stats.failures), (1, 1, 0));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(&*storage.deleted.lock().unwrap(), &["app/gone/old"]);
}

/// A session or application recovered in the fresh snapshot cancels its initially stale candidate.
#[tokio::test]
async fn fresh_snapshot_recovery_prevents_deletion() {
    let storage = storage(vec![
        object("app/session/object", 1),
        object("app/pkg/archive", 1),
        object("app/shared/object", 1),
    ]);
    let recovered = snapshot(
        vec![application("app", 0, 100)],
        vec![session("app", "session", 1)],
    );
    let (mut collector, calls) = collector(
        storage.clone(),
        vec![Ok(LivenessSnapshot::default()), Ok(recovered)],
    );
    let stats = collector.reconcile(20_000).await.unwrap();
    assert_eq!((stats.candidates, stats.deleted), (3, 0));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(storage.deleted.lock().unwrap().is_empty());
}

/// Failed initial or fresh liveness queries authorize no deletion, including malformed session responses.
#[tokio::test]
async fn unavailable_or_malformed_liveness_deletes_nothing() {
    for responses in [
        vec![Err(FlameError::Network("initial query failed".into()))],
        vec![
            Ok(LivenessSnapshot::default()),
            Err(FlameError::Network("fresh query failed".into())),
        ],
        vec![
            Ok(LivenessSnapshot::default()),
            validate_sessions(vec![flame_rpc::Session::default()]).map(|sessions| {
                LivenessSnapshot {
                    sessions,
                    ..Default::default()
                }
            }),
        ],
    ] {
        let storage = storage(vec![object("app/session/object", 1)]);
        let (mut collector, _) = collector(storage.clone(), responses);
        assert!(collector.reconcile(20_000).await.is_err());
        assert!(storage.deleted.lock().unwrap().is_empty());
    }
}

/// Application-owned namespaces retain current and disabled data, collect old incarnations, and respect creation grace.
#[tokio::test]
async fn application_owned_namespaces_keep_current_data() {
    let storage = storage(vec![
        object("app/pkg/old", 1),
        object("app/pkg/current", 20_000),
        object("disabled/pkg/archive", 1),
        object("missing/pkg/archive", 1),
        object("app/shared/old", 1),
        object("app/shared/current", 20_000),
        object("disabled/shared/object", 1),
        object("missing/shared/old", 1),
        object("missing/shared/young", 35_000),
        object("app/session/object", 1),
    ]);
    let live = snapshot(
        vec![
            application("app", 0, 20_000),
            application("disabled", 1, 20_000),
        ],
        vec![session("app", "session", 1)],
    );
    let (mut collector, _) = collector(storage.clone(), vec![Ok(live.clone()), Ok(live)]);
    assert_eq!(collector.reconcile(40_000).await.unwrap().deleted, 4);
    assert_eq!(
        &*storage.deleted.lock().unwrap(),
        &[
            "app/pkg/old",
            "missing/pkg/archive",
            "app/shared/old",
            "missing/shared/old"
        ]
    );
}

/// One filesystem deletion error does not prevent other confirmed stale sessions from being collected.
#[tokio::test]
async fn one_deletion_error_does_not_stop_other_candidates() {
    let storage = Arc::new(MockStorage {
        objects: vec![object("app/gone/fail", 1), object("app/gone/delete", 1)],
        deleted: Mutex::new(Vec::new()),
        failures: HashSet::from(["app/gone/fail".into()]),
    });
    let (mut collector, _) = collector(
        storage.clone(),
        vec![
            Ok(LivenessSnapshot::default()),
            Ok(LivenessSnapshot::default()),
        ],
    );
    let stats = collector.reconcile(20_000).await.unwrap();
    assert_eq!((stats.deleted, stats.failures), (1, 1));
    assert_eq!(&*storage.deleted.lock().unwrap(), &["app/gone/delete"]);
}

/// Session-owned files are unlinked directly while already-open readers finish using their captured file handles.
#[tokio::test]
async fn gone_session_cleanup_does_not_wait_for_existing_reader() {
    use crate::controller::CacheEndpoint;
    use crate::model::ObjectFilter;
    use crate::storage::FsStorage;
    use bytes::Bytes;
    let directory = tempfile::tempdir().unwrap();
    let controller = Arc::new(Controller::new(
        CacheEndpoint {
            scheme: "grpc".into(),
            host: "127.0.0.1".into(),
            port: 9090,
        },
        Box::new(FsStorage::new(directory.path().into()).unwrap()),
    ));
    let storage = Arc::clone(&controller.storage);
    let key = key("app/gone/object");
    let metadata = storage
        .write_object(
            &key,
            "raw",
            4,
            Box::pin(futures::stream::iter([Ok(Bytes::from_static(b"data"))])),
        )
        .await
        .unwrap();
    let mut reader = storage.read_object(&mut metadata.clone()).await.unwrap();
    let (mut collector, _) = collector(
        controller,
        vec![
            Ok(LivenessSnapshot::default()),
            Ok(LivenessSnapshot::default()),
        ],
    );
    let stats = tokio::time::timeout(
        Duration::from_secs(5),
        collector.reconcile(metadata.creation_time + 20_000),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stats.deleted, 1);
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        futures::StreamExt::next(&mut reader)
            .await
            .unwrap()
            .unwrap()
            .data,
        b"data".as_slice()
    );
    assert!(futures::StreamExt::next(&mut reader).await.is_none());
}

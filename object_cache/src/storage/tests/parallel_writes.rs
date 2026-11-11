/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use super::*;
use crate::model::Part;
use bytes::Bytes;
use std::time::Duration;
use tempfile::tempdir;

/// Return a complete object key shared by isolated parallel-write scenarios.
fn key() -> ObjectKey {
    ObjectKey::from_path("parallel/session/object").unwrap()
}
/// Commit the initial exact-size payload using the public filesystem factory.
async fn create(storage: &FsStorage, key: &ObjectKey, payload: &'static [u8]) -> ObjectMetadata {
    let mut writer = storage
        .object_writer(key, "raw", payload.len() as u64)
        .await
        .unwrap();
    writer
        .write_chunk(Bytes::from_static(payload))
        .await
        .unwrap();
    storage.commit_test_writer(writer).await.unwrap()
}
/// Return published metadata without exposing Writing or Stale physical slots.
async fn latest(storage: &FsStorage) -> ObjectMetadata {
    storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .remove(0)
}
/// Consume public logical parts and verify their dense stable IDs through normal readers.
async fn contents(storage: &FsStorage, metadata: &ObjectMetadata) -> Vec<(u64, Vec<u8>)> {
    let mut reader = storage.read_object(&mut metadata.clone()).await.unwrap();
    let mut parts: Vec<(u64, Vec<u8>)> = Vec::new();
    while let Some(chunk) = reader.next().await {
        let chunk = chunk.unwrap();
        if parts.last().is_none_or(|(part, _)| *part != chunk.part) {
            parts.push((chunk.part, Vec::new()));
        }
        parts.last_mut().unwrap().1.extend_from_slice(&chunk.data);
    }
    parts
}

/// A later upload commits immediately and keeps its logical ID when an earlier upload eventually completes.
#[tokio::test]
async fn parallel_commits_publish_in_completion_order_and_preserve_historical_prefixes() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    let initial = create(&storage, &key, b"base").await;
    let mut earlier = storage.patch_writer(&key, "raw", 3).await.unwrap();
    let mut later =
        tokio::time::timeout(Duration::from_secs(1), storage.patch_writer(&key, "raw", 3))
            .await
            .unwrap()
            .unwrap();
    later.write_chunk(Bytes::from_static(b"two")).await.unwrap();
    let prefix = tokio::time::timeout(Duration::from_secs(2), storage.commit_test_writer(later))
        .await
        .expect("commit must not wait for an earlier Writing slot")
        .unwrap();
    assert_eq!((prefix.version, prefix.size), (2, 7));
    assert_eq!(latest(&storage).await.version, 2);
    assert_eq!(
        contents(&storage, &prefix).await,
        vec![(0, b"base".to_vec()), (1, b"two".to_vec())]
    );
    assert!(matches!(
        storage
            .read_object(&mut prefix.clone().with_version(3))
            .await,
        Err(FlameError::InvalidConfig(_))
    ));
    assert!(matches!(
        storage
            .read_patch(&mut prefix.clone().with_version(3))
            .await,
        Err(FlameError::InvalidConfig(_))
    ));
    assert_eq!(
        contents(&storage, &initial).await,
        vec![(0, b"base".to_vec())]
    );
    earlier
        .write_chunk(Bytes::from_static(b"one"))
        .await
        .unwrap();
    let metadata = storage.commit_test_writer(earlier).await.unwrap();
    assert_eq!((metadata.version, metadata.size), (3, 10));
    assert_eq!(
        contents(&storage, &metadata).await,
        vec![
            (0, b"base".to_vec()),
            (1, b"two".to_vec()),
            (2, b"one".to_vec())
        ]
    );
    assert_eq!(
        contents(&storage, &prefix).await,
        vec![(0, b"base".to_vec()), (1, b"two".to_vec())]
    );
    let reopened = FsStorage::new(directory.path().into()).unwrap();
    assert_eq!(latest(&reopened).await.version, 3);
    assert_eq!(
        contents(&reopened, &metadata).await,
        contents(&storage, &metadata).await
    );
}

/// Stale physical attempts consume no logical IDs and cannot invalidate other successful uploads.
#[tokio::test]
async fn stale_attempts_leave_dense_committed_parts_and_retry_uses_a_new_extent() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    create(&storage, &key, b"base").await;
    let earlier = storage.patch_writer(&key, "raw", 3).await.unwrap();
    let mut later = storage.patch_writer(&key, "raw", 3).await.unwrap();
    later.write_chunk(Bytes::from_static(b"two")).await.unwrap();
    let prefix = storage.commit_test_writer(later).await.unwrap();
    storage.abort_writer(earlier).unwrap();
    let mut retry = storage.patch_writer(&key, "raw", 3).await.unwrap();
    retry.write_chunk(Bytes::from_static(b"new")).await.unwrap();
    let metadata = storage.commit_test_writer(retry).await.unwrap();
    assert_eq!(metadata.version, 3);
    assert_eq!(
        contents(&storage, &metadata).await,
        vec![
            (0, b"base".to_vec()),
            (1, b"two".to_vec()),
            (2, b"new".to_vec())
        ]
    );
    assert_eq!(
        contents(&storage, &prefix).await,
        vec![(0, b"base".to_vec()), (1, b"two".to_vec())]
    );
    let object = ObjectFiles::open(&storage.object_dir(&key))
        .unwrap()
        .unwrap()
        .object;
    assert_eq!(object.parts[1].state, dao::PartState::Stale);
    assert!(object.parts[3].offset >= 10);
    let reopened = FsStorage::new(directory.path().into()).unwrap();
    assert_eq!(
        contents(&reopened, &metadata).await,
        contents(&storage, &metadata).await
    );
}

/// Empty uploads receive unique logical IDs in commit order even when their physical extents share an offset.
#[tokio::test]
async fn empty_parts_skip_writing_and_stale_attempts_without_duplicate_public_ids() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    let initial = storage.object_writer(&key, "raw", 0).await.unwrap();
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    let second = storage.patch_writer(&key, "raw", 0).await.unwrap();
    assert_eq!(storage.commit_test_writer(second).await.unwrap().version, 1);
    let metadata = storage.commit_test_writer(initial).await.unwrap();
    assert_eq!(metadata.version, 2);
    assert_eq!(
        contents(&storage, &metadata).await,
        vec![(0, Vec::new()), (1, Vec::new())]
    );
    let canceled = storage.patch_writer(&key, "raw", 0).await.unwrap();
    let later = storage.patch_writer(&key, "raw", 0).await.unwrap();
    storage.abort_writer(canceled).unwrap();
    assert_eq!(storage.commit_test_writer(later).await.unwrap().version, 3);
    let next = storage.patch_writer(&key, "raw", 0).await.unwrap();
    let metadata = storage.commit_test_writer(next).await.unwrap();
    assert_eq!(
        contents(&storage, &metadata).await,
        vec![
            (0, Vec::new()),
            (1, Vec::new()),
            (2, Vec::new()),
            (3, Vec::new())
        ]
    );
}

/// Declared sizes reject incomplete streams and overruns without acknowledging a truncated object.
#[tokio::test]
async fn declared_size_underflow_and_overflow_abort_the_initial_allocation() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    for payload in [b"ab".as_slice(), b"abcd".as_slice()] {
        let mut writer = storage.object_writer(&key, "raw", 3).await.unwrap();
        assert_eq!(
            writer
                .write_chunk(Bytes::copy_from_slice(payload))
                .await
                .is_err(),
            payload.len() > 3
        );
        assert!(storage.commit_test_writer(writer).await.is_err());
        assert!(storage
            .list(&ObjectFilter::AllObject)
            .await
            .unwrap()
            .is_empty());
        assert!(!storage.object_dir(&key).exists());
    }
    assert_eq!(create(&storage, &key, b"ok").await.size, 2);
}

/// Recovery turns Writing into Stale and retains later committed records using their persisted logical IDs.
#[tokio::test]
async fn recovery_marks_writing_stale_and_preserves_later_commit_order() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    create(&storage, &key, b"base").await;
    let dir = storage.object_dir(&key);
    let index = fs::OpenOptions::new()
        .write(true)
        .open(dir.join(INDEX_FILE))
        .unwrap();
    let mut writing = PartDao::from_part(Part::new(0, Bytes::new()), 4);
    writing.state = dao::PartState::Writing;
    writing.part = u64::MAX;
    index
        .write_all_at(&writing.encode(), RECORD_LEN as u64)
        .unwrap();
    let committed = PartDao::from_part(Part::new(1, Bytes::new()), 4);
    index
        .write_all_at(&committed.encode(), 2 * RECORD_LEN as u64)
        .unwrap();
    index.sync_all().unwrap();
    let before = fs::read(dir.join(INDEX_FILE)).unwrap();
    let opened = ObjectFiles::open(&dir).unwrap().unwrap();
    assert_eq!(opened.object.parts[1].state, dao::PartState::Writing);
    assert_eq!(fs::read(dir.join(INDEX_FILE)).unwrap(), before);
    let reopened = FsStorage::new(directory.path().into()).unwrap();
    let metadata = latest(&reopened).await;
    assert_eq!(metadata.version, 2);
    assert_eq!(
        contents(&reopened, &metadata).await,
        vec![(0, b"base".to_vec()), (1, Vec::new())]
    );
    let parts = ObjectFiles::open(&dir).unwrap().unwrap().object.parts;
    assert_eq!(
        (parts[1].state, parts[2].state),
        (dao::PartState::Stale, dao::PartState::Committed)
    );
    assert_eq!(
        u64::from_le_bytes(
            fs::read(dir.join(INDEX_FILE)).unwrap()[RECORD_LEN + 56..RECORD_LEN + 64]
                .try_into()
                .unwrap()
        ),
        2
    );
    for (name, success, index_present) in [
        ("abandoned", false, true),
        ("survivor", true, true),
        ("partial", false, false),
    ] {
        let key = ObjectKey::from_path(&format!("parallel/session/{name}")).unwrap();
        let dir = reopened.object_dir(&key);
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join(METADATA_FILE),
            serde_json::to_vec(&ObjectMetadataDao {
                uuid: uuid::Uuid::new_v4().to_string(),
                data_type: "raw".into(),
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(dir.join(DATA_FILE), []).unwrap();
        if index_present {
            let mut writing = writing;
            writing.offset = 0;
            let mut bytes = writing.encode().to_vec();
            if success {
                let mut committed = committed;
                committed.offset = 0;
                committed.part = 0;
                bytes.extend_from_slice(&committed.encode());
            }
            fs::write(dir.join(INDEX_FILE), bytes).unwrap();
        }
    }
    let recovered = FsStorage::new(directory.path().into()).unwrap();
    let objects = recovered.list(&ObjectFilter::AllObject).await.unwrap();
    assert_eq!(objects.len(), 2);
    assert!(!directory.path().join("parallel/session/abandoned").exists());
    assert!(!directory.path().join("parallel/session/partial").exists());
    let survivor = objects
        .iter()
        .find(|object| object.key.ends_with("/survivor"))
        .unwrap();
    assert_eq!(contents(&recovered, survivor).await, vec![(0, Vec::new())]);
}

/// A crash may persist a later unacknowledged descriptor first; recovery keeps the earlier acknowledged prefix usable.
#[tokio::test]
async fn recovery_retires_a_gapped_unacknowledged_tail_before_validating_its_extent() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    let acknowledged = create(&storage, &key, b"base").await;
    let dir = storage.object_dir(&key);
    let index = fs::OpenOptions::new()
        .write(true)
        .open(dir.join(INDEX_FILE))
        .unwrap();
    let mut interrupted = PartDao::from_part(Part::new(1, Bytes::new()), 4);
    interrupted.state = dao::PartState::Writing;
    interrupted.part = u64::MAX;
    index
        .write_all_at(&interrupted.encode(), RECORD_LEN as u64)
        .unwrap();
    let invalid_tail = PartDao::from_part(Part::new(2, Bytes::from_static(b"lost")), 1000);
    index
        .write_all_at(&invalid_tail.encode(), 2 * RECORD_LEN as u64)
        .unwrap();
    index.sync_all().unwrap();
    let reopened = FsStorage::new(directory.path().into()).unwrap();
    assert_eq!(latest(&reopened).await.version, 1);
    assert_eq!(
        contents(&reopened, &acknowledged).await,
        vec![(0, b"base".to_vec())]
    );
    let parts = ObjectFiles::open(&dir).unwrap().unwrap().object.parts;
    assert!(parts[1..]
        .iter()
        .all(|part| part.state == dao::PartState::Stale));
    let mut retry = reopened.patch_writer(&key, "raw", 3).await.unwrap();
    retry.write_chunk(Bytes::from_static(b"new")).await.unwrap();
    let metadata = reopened.commit_test_writer(retry).await.unwrap();
    assert_eq!(metadata.version, 2);
    assert_eq!(
        contents(&reopened, &metadata).await,
        vec![(0, b"base".to_vec()), (1, b"new".to_vec())]
    );
}

/// Duplicate persisted logical IDs are ambiguous and fail closed instead of exposing a reordered historical prefix.
#[tokio::test]
async fn recovery_rejects_duplicate_committed_logical_ids() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    create(&storage, &key, b"base").await;
    let duplicate = PartDao::from_part(Part::new(0, Bytes::new()), 4);
    let index = fs::OpenOptions::new()
        .write(true)
        .open(storage.object_dir(&key).join(INDEX_FILE))
        .unwrap();
    index
        .write_all_at(&duplicate.encode(), RECORD_LEN as u64)
        .unwrap();
    index.sync_all().unwrap();
    assert!(matches!(
        FsStorage::new(directory.path().into()),
        Err(FlameError::DataLoss(_))
    ));
}

/// An fsync fence advertises its own stable prefix while excluding later prepared payloads that it may not have synced.
#[test]
fn sync_fence_publishes_only_covered_logical_prefixes() {
    let mut prepared: Vec<_> = (0..3)
        .map(|part| {
            let mut record = PartDao::from_part(
                Part::new(part, Bytes::from(vec![part as u8; part as usize + 1])),
                0,
            );
            record.state = dao::PartState::Writing;
            record
        })
        .collect();
    let mut stale = PartDao::from_part(Part::new(u64::MAX, Bytes::new()), 0);
    stale.state = dao::PartState::Stale;
    prepared.push(stale);
    let mut object = ObjectDao {
        metadata: ObjectMetadataDao {
            uuid: "object".into(),
            data_type: "raw".into(),
        },
        parts: prepared,
        cached_version: 0,
    };
    object.publish(1);
    assert_eq!(
        (
            object.metadata("app/session/object", "").version,
            object.size()
        ),
        (2, 3)
    );
    assert_eq!(
        object
            .visible_parts()
            .map(|part| part.part)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    object.publish(0);
    let own = object.metadata_prefix("app/session/object", "", 1);
    assert_eq!((own.version, own.size), (1, 1));
    assert_eq!(object.version(), 2);
    object.publish(2);
    assert_eq!((object.version(), object.size()), (3, 6));
    assert_eq!(
        object
            .visible_parts()
            .map(|part| part.part)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

/// GC unlinks immediately while unfinished writers fail their own later commit and captured readers retain their inode.
#[tokio::test]
async fn gc_does_not_wait_for_writing_parts_or_resurrect_their_metadata() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    create(&storage, &key, b"base").await;
    let mut earlier = storage.patch_writer(&key, "raw", 1).await.unwrap();
    earlier.write_chunk(Bytes::from_static(b"x")).await.unwrap();
    let mut later = storage.patch_writer(&key, "raw", 1).await.unwrap();
    later.write_chunk(Bytes::from_static(b"y")).await.unwrap();
    let metadata = storage.commit_test_writer(later).await.unwrap();
    let mut reader = storage.read_object(&mut metadata.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), storage.delete_objects(&key))
        .await
        .unwrap()
        .unwrap();
    assert!(storage.commit_test_writer(earlier).await.is_err());
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    let mut data = Vec::new();
    while let Some(chunk) = reader.next().await {
        let chunk = chunk.unwrap();
        data.extend_from_slice(&chunk.data);
    }
    assert_eq!(data, b"basey");
}

/// Idle writers leave even a one-thread blocking pool available to independent commits, reads, and allocations.
#[test]
fn idle_uploads_do_not_occupy_the_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory = tempdir().unwrap();
        let storage = FsStorage::new(directory.path().into()).unwrap();
        let key = key();
        create(&storage, &key, b"base").await;
        let first = storage.patch_writer(&key, "raw", 1).await.unwrap();
        let mut later =
            tokio::time::timeout(Duration::from_secs(2), storage.patch_writer(&key, "raw", 1))
                .await
                .unwrap()
                .unwrap();
        later.write_chunk(Bytes::from_static(b"x")).await.unwrap();
        let metadata =
            tokio::time::timeout(Duration::from_secs(2), storage.commit_test_writer(later))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), contents(&storage, &metadata))
                .await
                .unwrap(),
            vec![(0, b"base".to_vec()), (1, b"x".to_vec())]
        );
        let other = ObjectKey::from_path("parallel/session/other").unwrap();
        let unrelated = tokio::time::timeout(
            Duration::from_secs(2),
            storage.object_writer(&other, "raw", 0),
        )
        .await
        .unwrap()
        .unwrap();
        storage.abort_writer(unrelated).unwrap();
        storage.abort_writer(first).unwrap();
    });
}

/// Canceling a borrowed write poisons its handle before a queued pwrite can overwrite any retried payload.
#[test]
fn canceled_chunk_cannot_be_retried_or_committed_while_its_pwrite_is_pending() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let directory = tempdir().unwrap();
        let storage = FsStorage::new(directory.path().into()).unwrap();
        let key = key();
        create(&storage, &key, b"base").await;
        let mut writer = storage.patch_writer(&key, "raw", 3).await.unwrap();
        let selected = writer.physical_part();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.await.unwrap();
        {
            let mut write = Box::pin(writer.write_raw_chunk(crate::model::Chunk {
                part: selected.part,
                offset: 0,
                data: Bytes::from_static(b"old"),
            }));
            assert!(futures::poll!(&mut write).is_pending());
        }
        assert!(writer
            .write_raw_chunk(crate::model::Chunk {
                part: selected.part,
                offset: 0,
                data: Bytes::from_static(b"new")
            })
            .await
            .is_err());
        assert!(storage
            .commit_part(writer, Part::new(selected.part, Bytes::from_static(b"new")))
            .await
            .is_err());
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        let mut retry = storage.patch_writer(&key, "raw", 3).await.unwrap();
        retry.write_chunk(Bytes::from_static(b"new")).await.unwrap();
        let metadata = storage.commit_test_writer(retry).await.unwrap();
        assert_eq!(
            contents(&storage, &metadata).await,
            vec![(0, b"base".to_vec()), (1, b"new".to_vec())]
        );
    });
}

/// Both public write names create missing objects or add independent parts without replacing existing data.
#[tokio::test]
async fn create_if_missing_is_shared_by_object_and_patch_writes() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    let first = storage
        .write_patch(
            &key,
            "raw",
            4,
            Box::pin(futures::stream::iter([Ok(Bytes::from_static(b"base"))])),
        )
        .await
        .unwrap();
    let second = storage
        .write_object(
            &key,
            "raw",
            4,
            Box::pin(futures::stream::iter([Ok(Bytes::from_static(b"next"))])),
        )
        .await
        .unwrap();
    assert_eq!(second.uuid, first.uuid);
    assert_eq!((second.version, second.size), (2, 8));
    assert_eq!(
        contents(&storage, &second).await,
        vec![(0, b"base".to_vec()), (1, b"next".to_vec())]
    );
    let generated = ObjectKey::from_path("parallel/session").unwrap();
    let object = storage
        .write_patch(&generated, "raw", 0, Box::pin(futures::stream::empty()))
        .await
        .unwrap();
    assert!(object.key.starts_with("parallel/session/"));
    assert_eq!(object.version, 1);
}

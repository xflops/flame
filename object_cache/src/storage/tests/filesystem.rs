/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use super::*;
use crate::model::{Chunk, Part};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::io::Write;
use tempfile::tempdir;

/// Create the complete object key used by filesystem regressions.
fn key() -> ObjectKey {
    ObjectKey::try_from("checksums/session/object").unwrap()
}
/// Create metadata with a fresh object UUID for a test upload.
fn metadata(key: &ObjectKey) -> ObjectMetadata {
    ObjectMetadata::new()
        .with_uuid(uuid::Uuid::new_v4().to_string())
        .with_key(key.to_key().unwrap())
}
/// Commit one checksummed test part through bounded, sequential chunks.
async fn write(
    storage: &FsStorage,
    key: &ObjectKey,
    metadata: ObjectMetadata,
    part: u64,
    bytes: Bytes,
) -> ObjectMetadata {
    let mut payload =
        Part::new(part, bytes.clone()).with_creation_time(1_700_000_000_000 + part as i64);
    let mut writer = storage
        .writer(key, &metadata.data_type, bytes.len() as u64)
        .await
        .unwrap();
    let selected = writer.physical_part();
    payload.part = selected.part;
    for (index, data) in bytes.chunks(CHUNK_SIZE).enumerate() {
        writer
            .write_raw_chunk(Chunk {
                part: selected.part,
                offset: (index * CHUNK_SIZE) as u64,
                data: Bytes::copy_from_slice(data),
            })
            .await
            .unwrap();
    }
    storage.commit_part(writer, payload).await.unwrap()
}
/// Collect one part while checking chunk ownership, offsets, size bounds, and stable EOF.
async fn read(storage: &FsStorage, key: &ObjectKey, part: u64) -> Result<Vec<u8>, FlameError> {
    let mut reader = storage.reader(key, part).await?;
    let descriptor = reader.part();
    assert_eq!(descriptor.part, part);
    let mut bytes = Vec::new();
    while let Some(chunk) = reader.next_chunk().await? {
        assert_eq!(chunk.part, part);
        assert_eq!(chunk.offset, bytes.len() as u64);
        assert!(chunk.data.len() <= CHUNK_SIZE);
        bytes.extend_from_slice(&chunk.data);
    }
    assert!(reader.next_chunk().await?.is_none());
    assert_eq!(bytes.len() as u64, descriptor.size);
    Ok(bytes)
}
/// Inspect private committed records for filesystem recovery assertions.
fn inspect(storage: &FsStorage, key: &ObjectKey) -> Option<ObjectDao> {
    ObjectFiles::open(&storage.object_dir(key))
        .unwrap()
        .map(|files| files.object)
}
/// Corrupt one payload byte in place without changing the file length.
fn flip(path: &Path, offset: u64) {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0];
    file.read_exact_at(&mut byte, offset).unwrap();
    byte[0] ^= 0x80;
    file.write_all_at(&byte, offset).unwrap();
    file.sync_all().unwrap();
}

#[tokio::test]
/// Captured file handles retain original parts across GC unlink and recreation of the same key.
async fn captured_readers_survive_gc_unlink_and_key_recreation() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let old_first = Bytes::from(vec![19; CHUNK_SIZE + 37]);
    let old_meta = write(&storage, &key, metadata(&key), 0, old_first.clone()).await;
    write(
        &storage,
        &key,
        old_meta.clone(),
        1,
        Bytes::from_static(b"old second"),
    )
    .await;
    let mut readers = storage.readers(&key, &[1, 0]).await.unwrap();
    let first = readers[1].next_chunk().await.unwrap().unwrap();
    assert_eq!(first.data, old_first.slice(..CHUNK_SIZE));
    write(
        &storage,
        &key,
        old_meta.clone(),
        2,
        Bytes::from_static(b"later append"),
    )
    .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        storage.delete_objects(&key),
    )
    .await
    .expect("GC must not wait for captured readers")
    .unwrap();
    let recreated = write(
        &storage,
        &key,
        metadata(&key),
        0,
        Bytes::from_static(b"new first"),
    )
    .await;
    assert_ne!(recreated.uuid, old_meta.uuid);
    for (mut reader, expected) in readers.into_iter().zip([
        Bytes::from_static(b"old second"),
        old_first.slice(CHUNK_SIZE..),
    ]) {
        let mut actual = Vec::new();
        while let Some(chunk) = reader.next_chunk().await.unwrap() {
            actual.extend_from_slice(&chunk.data);
        }
        assert_eq!(actual, expected);
    }
    assert_eq!(read(&storage, &key, 0).await.unwrap(), b"new first");
}

#[tokio::test]
/// Verify an empty selection performs no object-file lookup.
async fn empty_reader_selection_does_not_require_an_object() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    assert!(storage.readers(&key(), &[]).await.unwrap().is_empty());
}

#[tokio::test]
/// Verify multipart checksums, offsets, metadata, and UUID survive startup recovery.
async fn multipart_readers_preserve_parts_offsets_and_uuid_after_restart() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    let payload = Bytes::from(vec![37; 3 * CHUNK_SIZE + 17]);
    let meta = write(&storage, &key, meta.clone(), 0, payload.clone()).await;
    write(
        &storage,
        &key,
        meta.clone(),
        1,
        Bytes::from_static(b"second"),
    )
    .await;
    let stat = inspect(&storage, &key).unwrap();
    assert_eq!(stat.metadata.uuid, meta.uuid);
    assert_eq!((stat.version(), stat.size()), (2, payload.len() as u64 + 6));
    assert_eq!(
        stat.parts[0].checksum,
        <[u8; 32]>::from(Sha256::digest(&payload))
    );
    assert_eq!(read(&storage, &key, 0).await.unwrap(), payload);
    assert_eq!(read(&storage, &key, 1).await.unwrap(), b"second");
    let reopened = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let recovered = reopened.list(&ObjectFilter::AllObject).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].uuid, meta.uuid);
    assert_eq!(
        reopened.list(&ObjectFilter::AllObject).await.unwrap()[0].version,
        2
    );
    assert_eq!(read(&reopened, &key, 1).await.unwrap(), b"second");
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(storage.object_dir(&key).join(METADATA_FILE)).unwrap())
            .unwrap();
    assert_eq!(
        persisted,
        serde_json::json!({"uuid":meta.uuid,"data_type":"raw"})
    );
}

#[tokio::test]
/// Verify empty parts each emit one independently verified chunk.
async fn empty_put_and_append_emit_distinct_verified_empty_chunks() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    for part in 0..2 {
        write(&storage, &key, meta.clone(), part, Bytes::new()).await;
        let mut reader = storage.reader(&key, part).await.unwrap();
        let chunk = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!((chunk.part, chunk.offset, chunk.data.len()), (part, 0, 0));
        assert!(reader.next_chunk().await.unwrap().is_none());
    }
    let stat = inspect(&storage, &key).unwrap();
    assert_eq!((stat.version(), stat.size()), (2, 0));
    assert!(stat
        .parts
        .iter()
        .all(|r| r.checksum == <[u8; 32]>::from(Sha256::digest([]))));
}

#[tokio::test]
/// Verify same-length corruption fails both captured and reopened readers.
async fn corruption_fails_after_partial_delivery_and_on_reopened_reads() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let payload = Bytes::from(vec![42; 3 * CHUNK_SIZE + 17]);
    write(&storage, &key, metadata(&key), 0, payload.clone()).await;
    let mut warm = storage.reader(&key, 0).await.unwrap();
    flip(
        &storage.object_dir(&key).join(DATA_FILE),
        payload.len() as u64 - 1,
    );
    let mut delivered = 0;
    loop {
        match warm.next_chunk().await {
            Ok(Some(chunk)) => delivered += chunk.data.len(),
            Err(FlameError::DataLoss(_)) => break,
            other => panic!("corrupt payload must fail: {other:?}"),
        }
    }
    assert!(delivered > 0 && delivered < payload.len());
    assert!(warm.next_chunk().await.unwrap().is_none());
    let reopened = FsStorage::new(dir.path().to_path_buf()).unwrap();
    assert!(matches!(
        read(&reopened, &key, 0).await,
        Err(FlameError::DataLoss(_))
    ));
}

#[tokio::test]
/// Verify a corrupt appended part leaves earlier committed parts readable.
async fn corrupted_appended_part_does_not_invalidate_clean_prefix() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    write(&storage, &key, meta.clone(), 0, Bytes::from_static(b"base")).await;
    write(&storage, &key, meta, 1, Bytes::from_static(b"patch")).await;
    flip(&storage.object_dir(&key).join(DATA_FILE), 8);
    assert_eq!(read(&storage, &key, 0).await.unwrap(), b"base");
    assert!(matches!(
        read(&storage, &key, 1).await,
        Err(FlameError::DataLoss(_))
    ));
}

#[tokio::test]
/// Verify abort retires an unfinished initial allocation and permits recreating the same key.
async fn abort_does_not_publish_and_the_same_key_can_be_created() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    let mut writer = storage.writer(&key, &meta.data_type, 6).await.unwrap();
    writer
        .write_raw_chunk(Chunk {
            part: 0,
            offset: 0,
            data: Bytes::from_static(b"prefix"),
        })
        .await
        .unwrap();
    storage.abort_writer(writer).unwrap();
    assert!(inspect(&storage, &key).is_none());
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    write(&storage, &key, meta, 0, Bytes::from_static(b"complete")).await;
    assert_eq!(read(&storage, &key, 0).await.unwrap(), b"complete");
}

#[tokio::test]
/// Verify invalid chunk ownership, offsets, and final sizes cannot commit.
async fn writers_reject_wrong_part_offsets_and_lengths_without_committing() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    let mut writer = storage.writer(&key, &meta.data_type, 3).await.unwrap();
    assert!(writer
        .write_raw_chunk(Chunk {
            part: 1,
            offset: 0,
            data: Bytes::from_static(b"bad"),
        })
        .await
        .is_err());
    storage.abort_writer(writer).unwrap();
    assert!(inspect(&storage, &key).is_none());
    write(&storage, &key, meta.clone(), 0, Bytes::from_static(b"base")).await;
    let mut writer = storage.writer(&key, &meta.data_type, 3).await.unwrap();
    assert!(writer
        .write_raw_chunk(Chunk {
            part: 1,
            offset: 7,
            data: Bytes::from_static(b"bad")
        })
        .await
        .is_err());
    storage.abort_writer(writer).unwrap();
    assert_eq!(inspect(&storage, &key).unwrap().version(), 1);
}

#[tokio::test]
/// Verify startup trims torn indexes while preserving committed parts and safe append offsets.
async fn startup_recovers_torn_index_and_ignores_orphan_payload() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    write(&storage, &key, meta.clone(), 0, Bytes::from_static(b"base")).await;
    let object = storage.object_dir(&key);
    fs::OpenOptions::new()
        .append(true)
        .open(object.join(DATA_FILE))
        .unwrap()
        .write_all(b"orphan")
        .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(object.join(INDEX_FILE))
        .unwrap()
        .write_all(&[7; 31])
        .unwrap();
    // Live stat and LIST must not run recovery or trim an active index tail.
    assert_eq!(inspect(&storage, &key).unwrap().version(), 1);
    storage.list(&ObjectFilter::AllObject).await.unwrap();
    assert_eq!(
        fs::metadata(object.join(INDEX_FILE)).unwrap().len(),
        (RECORD_LEN + 31) as u64
    );
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    assert_eq!(
        storage.list(&ObjectFilter::AllObject).await.unwrap().len(),
        1
    );
    assert_eq!(
        fs::metadata(object.join(INDEX_FILE)).unwrap().len(),
        RECORD_LEN as u64
    );
    write(&storage, &key, meta, 1, Bytes::from_static(b"next")).await;
    assert_eq!(inspect(&storage, &key).unwrap().parts[1].offset, 10);
    assert_eq!(read(&storage, &key, 0).await.unwrap(), b"base");
    assert_eq!(read(&storage, &key, 1).await.unwrap(), b"next");
}

#[tokio::test]
/// Verify recovery preserves hidden object names and removes only reserved upload staging.
async fn startup_preserves_hidden_objects_and_only_cleans_reserved_staging() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = ObjectKey::try_from("app/session/.hidden").unwrap();
    write(
        &storage,
        &key,
        metadata(&key),
        0,
        Bytes::from_static(b"valid"),
    )
    .await;
    let abandoned = storage
        .session_dir(&key)
        .join(STAGING_DIR)
        .join("interrupted");
    fs::create_dir_all(&abandoned).unwrap();
    storage.list(&ObjectFilter::AllObject).await.unwrap();
    assert!(abandoned.exists());
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    assert_eq!(
        storage.list(&ObjectFilter::AllObject).await.unwrap().len(),
        1
    );
    assert!(!abandoned.exists());
    assert_eq!(read(&storage, &key, 0).await.unwrap(), b"valid");
    assert!(ObjectKey::try_from("app/session/.uploads").is_err());
}

#[tokio::test]
/// Verify deletion retires catalog metadata and recreation uses a fresh UUID.
async fn deletion_updates_live_catalog_and_recreation_gets_new_uuid() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let first = metadata(&key);
    let first = write(&storage, &key, first.clone(), 0, Bytes::from_static(b"old")).await;
    storage
        .delete_objects(&ObjectKey::from_path("checksums/session").unwrap())
        .await
        .unwrap();
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    assert!(inspect(&storage, &key).is_none());
    let second = metadata(&key);
    let second = write(
        &storage,
        &key,
        second.clone(),
        0,
        Bytes::from_static(b"new"),
    )
    .await;
    assert_ne!(first.uuid, inspect(&storage, &key).unwrap().metadata.uuid);
    assert_eq!(
        storage.list(&ObjectFilter::AllObject).await.unwrap()[0].uuid,
        second.uuid
    );
}

#[tokio::test]
/// Verify filtered listings preserve identity and timestamps while projecting part prefixes.
async fn filtered_list_projects_prefix_metadata_and_rejects_ahead_versions() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let meta = metadata(&key);
    write(
        &storage,
        &key,
        meta.clone(),
        0,
        Bytes::from_static(b"first"),
    )
    .await;
    write(&storage, &key, meta, 1, Bytes::from_static(b"second")).await;
    let mut filter = ObjectFilter::ByKey {
        key: key.clone(),
        version: None,
    };
    let current = storage.list(&filter).await.unwrap().remove(0);
    assert_eq!((current.version, current.size), (2, 11));
    filter = ObjectFilter::ByKey {
        key: key.clone(),
        version: Some(1),
    };
    let prefix = storage.list(&filter).await.unwrap().remove(0);
    assert_eq!((prefix.version, prefix.size), (1, 5));
    assert_eq!(prefix.uuid, current.uuid);
    assert_eq!(prefix.creation_time, current.creation_time);
    filter = ObjectFilter::ByKey {
        key: key.clone(),
        version: Some(0),
    };
    let empty = storage.list(&filter).await.unwrap().remove(0);
    assert_eq!((empty.version, empty.size), (0, 0));
    assert_eq!(empty.creation_time, current.creation_time);
    assert_eq!(empty.uuid, current.uuid);
    filter = ObjectFilter::ByKey {
        key: key.clone(),
        version: Some(3),
    };
    assert!(matches!(
        storage.list(&filter).await,
        Err(FlameError::InvalidConfig(_))
    ));
    filter = ObjectFilter::ByKey {
        key: ObjectKey::try_from("checksums/session/missing").unwrap(),
        version: Some(3),
    };
    assert!(storage.list(&filter).await.unwrap().is_empty());
    filter = ObjectFilter::ByKey {
        key: ObjectKey::from_path("checksums/session").unwrap(),
        version: None,
    };
    assert_eq!(storage.list(&filter).await.unwrap().len(), 1);
}

#[tokio::test]
/// Preserve captured descriptors through bounded reads, verify the checksum before EOF, and terminate after corruption.
async fn reader_preserves_descriptor_and_verifies_checksum_through_stable_eof() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
    let key = key();
    let payload = Bytes::from(vec![23; CHUNK_SIZE + 7]);
    write(&storage, &key, metadata(&key), 0, payload.clone()).await;
    let mut reader = storage.reader(&key, 0).await.unwrap();
    let captured = reader.part();
    assert_eq!(captured.part, 0);
    assert_eq!(captured.creation_time, 1_700_000_000_000);
    assert_eq!(captured.size, payload.len() as u64);
    assert_eq!(
        captured.checksum,
        <[u8; 32]>::from(Sha256::digest(&payload))
    );
    let mut actual = Vec::new();
    while let Some(chunk) = reader.next_chunk().await.unwrap() {
        assert_eq!(chunk.offset, actual.len() as u64);
        assert!(chunk.data.len() <= CHUNK_SIZE);
        assert_eq!(reader.part(), captured);
        actual.extend_from_slice(&chunk.data);
    }
    assert_eq!(actual, payload);
    assert!(reader.next_chunk().await.unwrap().is_none());
    assert!(reader.next_chunk().await.unwrap().is_none());
    assert_eq!(reader.part(), captured);

    flip(
        &storage.object_dir(&key).join(DATA_FILE),
        payload.len() as u64 - 1,
    );
    let mut corrupted = storage.reader(&key, 0).await.unwrap();
    assert!(corrupted.next_chunk().await.unwrap().is_some());
    assert!(matches!(
        corrupted.next_chunk().await,
        Err(FlameError::DataLoss(_))
    ));
    assert!(corrupted.next_chunk().await.unwrap().is_none());
    assert_eq!(corrupted.part(), captured);
}

/// Verify completed aborts leave no attempted part in the Writing state.
async fn assert_pending_cleared(storage: &FsStorage, key: &ObjectKey) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if inspect(storage, key).is_none_or(|object| {
                object
                    .parts
                    .iter()
                    .all(|part| part.state != dao::PartState::Writing)
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

/// Count transport owners to verify positional writes release each supplied chunk after I/O.
struct TrackedChunk {
    bytes: Vec<u8>,
    live: Arc<std::sync::atomic::AtomicUsize>,
}
impl AsRef<[u8]> for TrackedChunk {
    /// Expose fixture bytes without copying so ownership lifetime remains observable.
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for TrackedChunk {
    /// Retire the transport owner after the positional write releases its supplied bytes.
    fn drop(&mut self) {
        self.live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
/// Stream a multipart-sized append with bounded owners while its preallocated extent remains invisible.
async fn append_streams_to_reserved_extent_with_bounded_chunk_ownership() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    let meta = metadata(&key);
    write(&storage, &key, meta.clone(), 0, Bytes::from_static(b"base")).await;
    let mut writer = storage
        .writer(&key, &meta.data_type, 24 * 64 * 1024)
        .await
        .unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let chunk_size = 64 * 1024;
    let count = 24;
    let mut hash = Sha256::new();
    for index in 0..count {
        let bytes = vec![index as u8; chunk_size];
        hash.update(&bytes);
        live.fetch_add(1, Ordering::SeqCst);
        let data = Bytes::from_owner(TrackedChunk {
            bytes,
            live: Arc::clone(&live),
        });
        writer
            .write_raw_chunk(Chunk {
                part: 1,
                offset: (index * chunk_size) as u64,
                data,
            })
            .await
            .unwrap();
    }
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(inspect(&storage, &key).unwrap().version(), 1);
    assert_eq!(
        fs::metadata(storage.object_dir(&key).join(DATA_FILE))
            .unwrap()
            .len(),
        4 + (count * chunk_size) as u64
    );
    let descriptor = Part {
        part: 1,
        size: (count * chunk_size) as u64,
        creation_time: 42,
        checksum: hash.finalize().into(),
    };
    storage.commit_part(writer, descriptor).await.unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(
        read(&storage, &key, 1).await.unwrap().len(),
        count * chunk_size
    );
    assert_eq!(inspect(&storage, &key).unwrap().version(), 2);
    assert_pending_cleared(&storage, &key).await;
}

/// Storage finalizes source failures and truncated streams without exposing their payloads or blocking later appends.
#[tokio::test]
async fn failed_sources_and_truncated_appends_preserve_committed_parts() {
    for source_failure in [true, false] {
        let directory = tempdir().unwrap();
        let storage = FsStorage::new(directory.path().into()).unwrap();
        let key = key();
        let meta = metadata(&key);
        write(&storage, &key, meta.clone(), 0, Bytes::from_static(b"base")).await;
        let size = 2 * CHUNK_SIZE + 3;
        let mut chunks = vec![Ok(Bytes::from(vec![17; size]))];
        if source_failure {
            chunks.push(Err(FlameError::InvalidState("source disconnected".into())));
        }
        assert!(storage
            .write_patch(
                &key,
                "raw",
                (size + 1) as u64,
                Box::pin(futures::stream::iter(chunks))
            )
            .await
            .is_err());
        assert_pending_cleared(&storage, &key).await;
        assert_eq!(
            storage.list(&ObjectFilter::AllObject).await.unwrap()[0].version,
            1
        );
        write(
            &storage,
            &key,
            meta,
            1,
            Bytes::from_static(b"replacement append"),
        )
        .await;
        assert_eq!(read(&storage, &key, 0).await.unwrap(), b"base");
        assert_eq!(
            read(&storage, &key, 1).await.unwrap(),
            b"replacement append"
        );
    }
}

/// Failed size and UUID validation mark only the attempted physical part stale.
#[tokio::test]
async fn append_errors_mark_failed_parts_stale_without_advancing_version() {
    let directory = tempdir().unwrap();
    let storage = FsStorage::new(directory.path().into()).unwrap();
    let key = key();
    let meta = metadata(&key);
    write(&storage, &key, meta.clone(), 0, Bytes::from_static(b"base")).await;
    let mut invalid = storage.writer(&key, &meta.data_type, 7).await.unwrap();
    invalid
        .write_raw_chunk(Chunk {
            part: 1,
            offset: 0,
            data: Bytes::from_static(b"pending"),
        })
        .await
        .unwrap();
    assert!(storage
        .commit_part(invalid, Part::new(1, Bytes::from_static(b"wrong length")))
        .await
        .is_err());
    assert_pending_cleared(&storage, &key).await;
    assert_eq!(inspect(&storage, &key).unwrap().version(), 1);
    let mut stale = storage.writer(&key, &meta.data_type, 7).await.unwrap();
    let selected = stale.physical_part();
    stale
        .write_raw_chunk(Chunk {
            part: selected.part,
            offset: 0,
            data: Bytes::from_static(b"pending"),
        })
        .await
        .unwrap();
    let metadata_path = storage.object_dir(&key).join(METADATA_FILE);
    let original = fs::read(&metadata_path).unwrap();
    fs::write(
        &metadata_path,
        serde_json::to_vec(&ObjectMetadataDao {
            uuid: uuid::Uuid::new_v4().to_string(),
            data_type: "raw".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        storage
            .commit_part(
                stale,
                Part::new(selected.part, Bytes::from_static(b"pending"))
            )
            .await,
        Err(FlameError::InvalidVersion(_))
    ));
    fs::write(&metadata_path, original).unwrap();
    assert_pending_cleared(&storage, &key).await;
    assert_eq!(read(&storage, &key, 0).await.unwrap(), b"base");
    assert_eq!(inspect(&storage, &key).unwrap().version(), 1);
    write(
        &storage,
        &key,
        metadata(&key),
        1,
        Bytes::from_static(b"valid append"),
    )
    .await;
    assert_eq!(read(&storage, &key, 1).await.unwrap(), b"valid append");
}

/// Return an already recovered catalog and readable persisted parts directly from asynchronous connection.
#[tokio::test]
async fn connect_recovers_existing_objects_before_returning() {
    let dir = tempdir().unwrap();
    let key = key();
    let committed = {
        let storage = FsStorage::new(dir.path().to_path_buf()).unwrap();
        write(
            &storage,
            &key,
            metadata(&key),
            0,
            Bytes::from_static(b"persisted"),
        )
        .await
    };
    let connected = connect(dir.path().to_str().unwrap()).await.unwrap();
    let listed = connected.list(&ObjectFilter::AllObject).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].uuid.as_str(), listed[0].version, listed[0].size),
        (committed.uuid.as_str(), 1, 9)
    );
    let mut reader = connected.read_object(&mut listed[0].clone()).await.unwrap();
    let mut data = Vec::new();
    while let Some(chunk) = reader.next().await {
        data.extend_from_slice(&chunk.unwrap().data);
    }
    assert_eq!(data, b"persisted");
}

/// Direct GC unlink completes while an append is uncommitted, and that append cannot republish collected metadata.
#[tokio::test]
async fn gc_unlinks_without_waiting_for_append_and_append_cannot_resurrect_metadata() {
    let dir = tempdir().unwrap();
    let storage = FsStorage::new(dir.path().into()).unwrap();
    let key = key();
    write(
        &storage,
        &key,
        metadata(&key),
        0,
        Bytes::from_static(b"base"),
    )
    .await;
    let mut pending = storage.writer(&key, "raw", 4).await.unwrap();
    pending
        .write_raw_chunk(Chunk {
            part: 1,
            offset: 0,
            data: Bytes::from_static(b"tail"),
        })
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        storage.delete_objects(&key),
    )
    .await
    .expect("GC must not wait for the writer")
    .unwrap();
    assert!(storage
        .commit_part(pending, Part::new(1, Bytes::from_static(b"tail")))
        .await
        .is_err());
    assert!(storage
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    assert!(!storage.object_dir(&key).exists());
}

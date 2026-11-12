/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use super::*;

/// Use one complete key across independent cache-incarnation scenarios.
fn key() -> ObjectKey {
    ObjectKey::from_path("app/session/object").unwrap()
}

/// Commit a payload through the production streaming storage boundary.
async fn write(storage: &FsStorage, bytes: Bytes) -> ObjectMetadata {
    storage
        .write_object(
            &key(),
            "raw",
            bytes.len() as u64,
            Box::pin(futures::stream::iter([Ok(bytes)])),
        )
        .await
        .unwrap()
}

/// Consume the selected snapshot, retaining chunk identities for framing assertions.
async fn read(storage: &FsStorage, metadata: &ObjectMetadata) -> Result<Vec<Chunk>, FlameError> {
    let mut stream = storage.read_object(&mut metadata.clone()).await?;
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }
    Ok(chunks)
}

/// Prove a cache hit uses verified memory bytes and a cold read still detects disk corruption.
#[tokio::test]
async fn verified_parts_are_shared_by_storage_clones_and_bypass_payload_reads() {
    let directory = tempfile::tempdir().unwrap();
    let storage = FsStorage::with_cache(directory.path().into(), pool(16)).unwrap();
    let metadata = write(&storage, Bytes::from_static(b"data")).await;
    assert_eq!(storage.part_cache.used(), 0, "writes do not warm the cache");
    assert_eq!(
        read(&storage, &metadata).await.unwrap()[0].data,
        b"data"[..]
    );
    assert_eq!(storage.part_cache.used(), 4);
    fs::OpenOptions::new()
        .write(true)
        .open(storage.object_dir(&key()).join(DATA_FILE))
        .unwrap()
        .write_all_at(b"bad!", 0)
        .unwrap();
    let clone = storage.clone();
    assert_eq!(read(&clone, &metadata).await.unwrap()[0].data, b"data"[..]);
    storage.part_cache.remove_object(&metadata.uuid);
    assert!(matches!(
        read(&storage, &metadata).await,
        Err(FlameError::DataLoss(_))
    ));
    assert_eq!(storage.part_cache.used(), 0);
}

/// Release incomplete fills on cancellation and never publish a checksum-failing part.
#[tokio::test]
async fn canceled_and_corrupt_reads_do_not_admit_partial_parts() {
    let directory = tempfile::tempdir().unwrap();
    let size = CHUNK_SIZE + 1;
    let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64)).unwrap();
    let metadata = write(&storage, Bytes::from(vec![7; size])).await;
    {
        let mut stream = storage.read_object(&mut metadata.clone()).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.data.len(), CHUNK_SIZE);
        assert_eq!(storage.part_cache.used(), 0);
        assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_none());
    }
    assert_eq!(storage.part_cache.used(), 0);
    fs::OpenOptions::new()
        .write(true)
        .open(storage.object_dir(&key()).join(DATA_FILE))
        .unwrap()
        .write_all_at(&[8], CHUNK_SIZE as u64)
        .unwrap();
    assert!(matches!(
        read(&storage, &metadata).await,
        Err(FlameError::DataLoss(_))
    ));
    assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_none());
    assert_eq!(storage.part_cache.used(), 0);
}

/// An incomplete miss preserves current entries; only its verified final chunk triggers LRU eviction.
#[tokio::test]
async fn cold_reads_evict_only_when_a_complete_part_is_ready() {
    let directory = tempfile::tempdir().unwrap();
    let size = CHUNK_SIZE + 1;
    let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64)).unwrap();
    let base = write(&storage, Bytes::from_static(b"base")).await;
    read(&storage, &base).await.unwrap();
    write(&storage, Bytes::from(vec![4; size])).await;
    {
        let mut stream = storage.read_patch(&mut base.clone()).await.unwrap();
        stream.next().await.unwrap().unwrap();
        assert_eq!(storage.part_cache.used(), 4);
        assert!(storage.part_cache.get_patch(&base.uuid, 0).is_some());
        assert!(storage.part_cache.get_patch(&base.uuid, 1).is_none());
    }
    assert_eq!(storage.part_cache.used(), 4);
    let mut stream = storage.read_patch(&mut base.clone()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    assert_eq!(storage.part_cache.used(), 4);
    stream.next().await.unwrap().unwrap();
    assert_eq!(storage.part_cache.used(), size as u64);
    assert!(storage.part_cache.get_patch(&base.uuid, 0).is_none());
    assert!(storage.part_cache.get_patch(&base.uuid, 1).is_some());
    assert!(stream.next().await.is_none());
}

/// Keep an already-active cached stream readable while GC invalidates its incarnation.
#[tokio::test]
async fn gc_removes_entries_without_interrupting_pinned_cached_readers() {
    let directory = tempfile::tempdir().unwrap();
    let size = CHUNK_SIZE + 3;
    let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64)).unwrap();
    let metadata = write(&storage, Bytes::from(vec![9; size])).await;
    read(&storage, &metadata).await.unwrap();
    {
        let mut stream = storage.read_object(&mut metadata.clone()).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().data.len(), CHUNK_SIZE);
        storage.delete_objects(&key()).await.unwrap();
        assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_none());
        assert_eq!(storage.part_cache.used(), 0);
        let last = stream.next().await.unwrap().unwrap();
        assert_eq!(
            (last.part, last.offset, last.data.len()),
            (0, CHUNK_SIZE as u64, 3)
        );
        assert_eq!(last.data, [9; 3][..]);
        assert!(stream.next().await.is_none());
    }
    assert_eq!(storage.part_cache.used(), 0);
    assert!(matches!(
        storage.read_object(&mut metadata.clone()).await,
        Err(FlameError::NotFound(_))
    ));
}

/// An old captured filesystem read cannot refill after GC or alias a recreated object's parts.
#[tokio::test]
async fn gc_and_recreation_reject_late_fills_from_old_readers() {
    let directory = tempfile::tempdir().unwrap();
    let size = CHUNK_SIZE + 3;
    let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64)).unwrap();
    let metadata = write(&storage, Bytes::from(vec![1; size])).await;
    {
        let mut stream = storage.read_object(&mut metadata.clone()).await.unwrap();
        stream.next().await.unwrap().unwrap();
        storage.delete_objects(&key()).await.unwrap();
        let replacement = write(&storage, Bytes::from_static(b"new")).await;
        assert_ne!(metadata.uuid, replacement.uuid);
        assert_eq!(stream.next().await.unwrap().unwrap().data, [1; 3][..]);
        assert!(stream.next().await.is_none());
        assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_none());
        assert_eq!(storage.part_cache.used(), 0);
        assert_eq!(
            read(&storage, &replacement).await.unwrap()[0].data,
            b"new"[..]
        );
        assert!(matches!(
            storage.read_object(&mut metadata.clone()).await,
            Err(FlameError::InvalidVersion(_))
        ));
    }
}

/// Stream oversized, disabled, and empty parts without admitting payload entries.
#[tokio::test]
async fn uncacheable_parts_keep_filesystem_streaming_behavior() {
    for (capacity, payload) in [(0, b"data".as_slice()), (3, b"data"), (4, b"")] {
        let directory = tempfile::tempdir().unwrap();
        let storage = FsStorage::with_cache(directory.path().into(), pool(capacity)).unwrap();
        let metadata = write(&storage, Bytes::copy_from_slice(payload)).await;
        let chunks = read(&storage, &metadata).await.unwrap();
        assert_eq!(
            chunks
                .iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect::<Vec<_>>(),
            payload
        );
        assert_eq!(storage.part_cache.used(), 0);
        assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_none());
    }
}

/// Parallel writers cache by logical commit order while historical and patch selections stay stable.
#[tokio::test]
async fn cache_preserves_parallel_commit_order_historical_prefixes_and_patches() {
    let directory = tempfile::tempdir().unwrap();
    let storage = FsStorage::with_cache(directory.path().into(), pool(32)).unwrap();
    let base = write(&storage, Bytes::from_static(b"base")).await;
    let mut earlier = storage.patch_writer(&key(), "raw", 3).await.unwrap();
    let mut later = storage.patch_writer(&key(), "raw", 3).await.unwrap();
    later.write_chunk(Bytes::from_static(b"two")).await.unwrap();
    let prefix = storage.commit_test_writer(later).await.unwrap();
    assert_eq!(read(&storage, &prefix).await.unwrap()[1].data, b"two"[..]);
    earlier
        .write_chunk(Bytes::from_static(b"one"))
        .await
        .unwrap();
    let complete = storage.commit_test_writer(earlier).await.unwrap();
    let chunks = read(&storage, &complete).await.unwrap();
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| (chunk.part, chunk.data.as_ref()))
            .collect::<Vec<_>>(),
        vec![(0, b"base".as_slice()), (1, b"two"), (2, b"one")]
    );
    assert_eq!(read(&storage, &base).await.unwrap().len(), 1);
    assert_eq!(read(&storage, &prefix).await.unwrap().len(), 2);
    assert_eq!(
        storage.part_cache.get_patch(&complete.uuid, 1).unwrap()[0],
        b"two"[..]
    );
    assert_eq!(
        storage.part_cache.get_patch(&complete.uuid, 2).unwrap()[0],
        b"one"[..]
    );
    let mut cursor = prefix.clone();
    let mut patches = storage.read_patch(&mut cursor).await.unwrap();
    assert_eq!(cursor.version, complete.version);
    let patch = patches.next().await.unwrap().unwrap();
    assert_eq!((patch.part, patch.data), (2, Bytes::from_static(b"one")));
    assert!(patches.next().await.is_none());
}

/// Concurrent cold reads stream independently and publish only one complete verified cache entry.
#[tokio::test]
async fn concurrent_misses_publish_only_ready_parts_without_double_counting() {
    let directory = tempfile::tempdir().unwrap();
    let size = CHUNK_SIZE + 1;
    let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64)).unwrap();
    let metadata = write(&storage, Bytes::from(vec![5; size])).await;
    let mut first = storage.read_object(&mut metadata.clone()).await.unwrap();
    let mut second = storage.read_object(&mut metadata.clone()).await.unwrap();
    first.next().await.unwrap().unwrap();
    second.next().await.unwrap().unwrap();
    assert_eq!(storage.part_cache.used(), 0);
    first.next().await.unwrap().unwrap();
    second.next().await.unwrap().unwrap();
    assert!(first.next().await.is_none());
    assert!(second.next().await.is_none());
    assert_eq!(storage.part_cache.used(), size as u64);
    assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_some());
}

/// Retained output chunks remain readable after eviction and do not count toward the resident pool.
#[tokio::test]
async fn downstream_chunks_remain_valid_outside_the_resident_pool() {
    for warm in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let size = CHUNK_SIZE + 1;
        let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64)).unwrap();
        let metadata = write(&storage, Bytes::from(vec![6; size])).await;
        if warm {
            read(&storage, &metadata).await.unwrap();
        }
        {
            let held = {
                let mut stream = storage.read_object(&mut metadata.clone()).await.unwrap();
                stream.next().await.unwrap().unwrap().data
            };
            storage.part_cache.remove_object(&metadata.uuid);
            assert_eq!(storage.part_cache.used(), 0);
            storage.part_cache.insert_patch(
                "another",
                Part::new(0, Bytes::from_static(b"new")),
                vec![Bytes::from_static(b"new")],
            );
            assert_eq!(storage.part_cache.used(), 3);
            assert_eq!(held.len(), CHUNK_SIZE);
            assert_eq!(held[0], 6);
        }
        assert_eq!(storage.part_cache.used(), 3);
    }
}

/// Fully cached snapshots construct memory readers without opening the object's payload file.
#[tokio::test]
async fn fully_cached_reads_succeed_without_a_payload_file() {
    let directory = tempfile::tempdir().unwrap();
    let size = CHUNK_SIZE + 1;
    let storage = FsStorage::with_cache(directory.path().into(), pool(size as u64 + 4)).unwrap();
    write(&storage, Bytes::from(vec![3; size])).await;
    let metadata = write(&storage, Bytes::from_static(b"tail")).await;
    let expected = read(&storage, &metadata).await.unwrap();
    assert_eq!(storage.part_cache.used(), size as u64 + 4);

    let object_dir = storage.object_dir(&key());
    fs::rename(object_dir.join(DATA_FILE), object_dir.join("data.hidden")).unwrap();
    assert!(object_dir.join(METADATA_FILE).exists());

    let chunks = read(&storage, &metadata).await.unwrap();
    assert_eq!(chunks.len(), expected.len());
    for (actual, expected) in chunks.iter().zip(&expected) {
        assert_eq!(actual.part, expected.part);
        assert_eq!(actual.offset, expected.offset);
        assert_eq!(actual.data, expected.data);
    }
}

/// Merge cached and filesystem readers in requested order while opening payload readers only for misses.
#[tokio::test]
async fn mixed_cached_and_disk_parts_preserve_object_and_explicit_reader_order() {
    let directory = tempfile::tempdir().unwrap();
    let storage = FsStorage::with_cache(directory.path().into(), pool(12)).unwrap();
    write(&storage, Bytes::from_static(b"zero")).await;
    write(&storage, Bytes::from_static(b"one!")).await;
    let metadata = write(&storage, Bytes::from_static(b"two!")).await;
    read(&storage, &metadata.clone().with_version(1))
        .await
        .unwrap();
    let mut suffix = storage
        .read_patch(&mut metadata.clone().with_version(2))
        .await
        .unwrap();
    assert_eq!(suffix.next().await.unwrap().unwrap().data, b"two!"[..]);
    assert!(suffix.next().await.is_none());
    assert!(storage.part_cache.get_patch(&metadata.uuid, 0).is_some());
    assert!(storage.part_cache.get_patch(&metadata.uuid, 1).is_none());
    assert!(storage.part_cache.get_patch(&metadata.uuid, 2).is_some());

    let object_dir = storage.object_dir(&key());
    let file = fs::OpenOptions::new()
        .write(true)
        .open(object_dir.join(DATA_FILE))
        .unwrap();
    file.write_all_at(b"bad0", 0).unwrap();
    file.write_all_at(b"bad2", 8).unwrap();
    let chunks = read(&storage, &metadata).await.unwrap();
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| (chunk.part, chunk.offset, chunk.data.as_ref()))
            .collect::<Vec<_>>(),
        vec![
            (0, 0, b"zero".as_slice()),
            (1, 0, b"one!".as_slice()),
            (2, 0, b"two!".as_slice()),
        ]
    );
    assert!(storage.part_cache.get_patch(&metadata.uuid, 1).is_some());

    fs::rename(object_dir.join(DATA_FILE), object_dir.join("data.hidden")).unwrap();
    let readers = storage.readers(&key(), &[1, 0, 1]).await.unwrap();
    for (mut reader, (part, expected)) in
        readers
            .into_iter()
            .zip([(1, b"one!"), (0, b"zero"), (1, b"one!")])
    {
        assert_eq!(reader.part().part, part);
        let chunk = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!(chunk.part, part);
        assert_eq!(chunk.data, expected[..]);
        assert!(reader.next_chunk().await.unwrap().is_none());
    }
}

/// Repeated cold selections clone independent offsets and checksum state from a disk-reader map.
#[tokio::test]
async fn repeated_disk_selections_have_independent_chunk_positions() {
    let directory = tempfile::tempdir().unwrap();
    let storage = FsStorage::with_cache(directory.path().into(), pool(0)).unwrap();
    let size = CHUNK_SIZE + 1;
    write(&storage, Bytes::from(vec![9; size])).await;
    let readers = storage.readers(&key(), &[0, 0]).await.unwrap();
    assert_eq!(readers.len(), 2);
    for mut reader in readers {
        let first = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!(first.part, 0);
        assert_eq!(first.offset, 0);
        assert_eq!(first.data.len(), CHUNK_SIZE);
        let last = reader.next_chunk().await.unwrap().unwrap();
        assert_eq!(last.offset, CHUNK_SIZE as u64);
        assert_eq!(last.data, [9][..]);
        assert!(reader.next_chunk().await.unwrap().is_none());
    }
}

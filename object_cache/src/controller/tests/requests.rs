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
use crate::controller::{CacheEndpoint, Controller, GrpcCacheServer, WriteRequest};

use crate::storage::{ByteStream, ChunkStream, StorageEngine, CHUNK_SIZE};
use async_trait::async_trait;
use futures::{stream, StreamExt, TryStreamExt};
use rpc::flame::v1::object_cache_service_server::ObjectCacheService;
use rpc::flame::v1::{
    cache_get_response, CacheGetMode, CacheGetRequest, CacheGetResponse, CacheObjectMetadata,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tonic::{Code, Request};

/// Build a complete fixture key under the audit session.
fn key(name: &str) -> ObjectKey {
    ObjectKey::try_from(format!("audit/session/{name}").as_str()).unwrap()
}

/// Return the local endpoint used in controller fixtures.
fn endpoint() -> CacheEndpoint {
    CacheEndpoint {
        scheme: "grpc".into(),
        host: "127.0.0.1".into(),
        port: 9090,
    }
}

/// Build a controller using the selected filesystem storage path.
async fn cache(storage: &str) -> Arc<Controller> {
    Arc::new(Controller::new(
        endpoint(),
        crate::storage::connect(storage, &pool(16 * 1024 * 1024))
            .await
            .unwrap(),
    ))
}

/// Upload raw fixture bytes in transport-sized chunks.
async fn put(cache: &Arc<Controller>, key: &ObjectKey, bytes: Bytes) -> ObjectMetadata {
    cache
        .put_stream(WriteRequest {
            key: key.clone(),
            data_type: "raw".into(),
            size: bytes.len() as u64,
            chunks: Box::pin(stream::iter(
                bytes
                    .chunks(CHUNK_SIZE)
                    .map(Bytes::copy_from_slice)
                    .map(Ok)
                    .collect::<Vec<_>>(),
            )),
        })
        .await
        .unwrap()
}

/// Build a GET request with an explicit mode, version, and UUID cursor.
fn request(
    key: &ObjectKey,
    mode: CacheGetMode,
    version: u64,
    uuid: &str,
) -> Request<CacheGetRequest> {
    Request::new(CacheGetRequest {
        key: key.to_key().unwrap(),
        mode: mode as i32,
        version,
        uuid: uuid.into(),
    })
}

/// Capture and fully consume a GET response for assertions.
async fn get(
    cache: &Arc<Controller>,
    key: &ObjectKey,
    mode: CacheGetMode,
    version: u64,
    uuid: &str,
) -> Vec<CacheGetResponse> {
    GrpcCacheServer::new(Arc::clone(cache))
        .get(request(key, mode, version, uuid))
        .await
        .unwrap()
        .into_inner()
        .try_collect()
        .await
        .unwrap()
}

/// Extract the required metadata header from a collected GET response.
fn metadata(frames: &[CacheGetResponse]) -> &CacheObjectMetadata {
    match frames[0].payload.as_ref().unwrap() {
        cache_get_response::Payload::Header(header) => header.metadata.as_ref().unwrap(),
        _ => panic!("GET must begin with metadata"),
    }
}

/// Group response chunks by part while checking the transport chunk-size bound.
fn parts(frames: &[CacheGetResponse]) -> Vec<(u64, Vec<u8>)> {
    let mut parts: Vec<(u64, Vec<u8>)> = Vec::new();
    for frame in &frames[1..] {
        let Some(cache_get_response::Payload::Chunk(chunk)) = &frame.payload else {
            panic!("expected part chunk after metadata")
        };
        assert!(chunk.data.len() <= CHUNK_SIZE);
        match parts.last_mut() {
            Some((part, data)) if *part == chunk.part => data.extend_from_slice(&chunk.data),
            _ => parts.push((chunk.part, chunk.data.to_vec())),
        }
    }
    parts
}
/// Keep zero-based part IDs, prefix sizes, and empty parts consistent across all GET modes.

#[tokio::test]
async fn get_modes_preserve_part_identity_prefix_sizes_and_empty_parts() {
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path().to_str().unwrap()).await;
    let key = key("modes");
    let created = put(&cache, &key, Bytes::from_static(b"base")).await;
    let large = Bytes::from(vec![7; CHUNK_SIZE + 13]);
    cache.patch(&key, "raw", large.clone()).await.unwrap();
    cache.patch(&key, "raw", Bytes::new()).await.unwrap();

    let latest = get(&cache, &key, CacheGetMode::Object, 0, "").await;
    assert_eq!(metadata(&latest).version, 3);
    assert_eq!(metadata(&latest).size, 4 + large.len() as u64);
    assert_eq!(
        parts(&latest),
        [(0, b"base".to_vec()), (1, large.to_vec()), (2, Vec::new())]
    );
    assert_eq!(
        latest.len(),
        5,
        "header, base, two large-part chunks, empty part"
    );

    let prefix = get(&cache, &key, CacheGetMode::Object, 1, &created.uuid).await;
    assert_eq!((metadata(&prefix).version, metadata(&prefix).size), (1, 4));
    assert_eq!(parts(&prefix), [(0, b"base".to_vec())]);

    let suffix = get(&cache, &key, CacheGetMode::Patches, 1, &created.uuid).await;
    assert_eq!(metadata(&suffix).version, 3);
    assert_eq!(parts(&suffix), [(1, large.to_vec()), (2, Vec::new())]);
    assert_eq!(
        get(&cache, &key, CacheGetMode::Patches, 3, &created.uuid)
            .await
            .len(),
        1
    );
    let descriptor = get(&cache, &key, CacheGetMode::Metadata, u64::MAX, "ignored").await;
    assert_eq!(descriptor.len(), 1);
    assert_eq!(
        (metadata(&descriptor).version, metadata(&descriptor).size),
        (3, 4 + large.len() as u64)
    );
}
/// Reject unsupported GET modes and invalid cursor combinations.

#[tokio::test]
async fn get_rejects_unknown_mode_missing_uuid_and_ahead_cursors() {
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path().to_str().unwrap()).await;
    let key = key("invalid");
    let created = put(&cache, &key, Bytes::from_static(b"base")).await;
    let server = GrpcCacheServer::new(cache);
    for input in [
        request(&key, CacheGetMode::Object, 2, &created.uuid),
        request(&key, CacheGetMode::Patches, 1, ""),
        Request::new(CacheGetRequest {
            mode: 99,
            ..request(&key, CacheGetMode::Object, 0, "").into_inner()
        }),
    ] {
        let error = server
            .get(input)
            .await
            .err()
            .expect("invalid request must fail");
        assert_eq!(error.code(), Code::InvalidArgument);
    }
}
/// Reject stale UUID cursors after key recreation regardless of the replacement part count.

#[tokio::test]
async fn recreated_keys_cannot_reuse_an_old_uuid_even_at_equal_or_greater_counts() {
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path().to_str().unwrap()).await;
    let key = key("recreated");
    let old = put(&cache, &key, Bytes::from_static(b"old base")).await;
    cache
        .patch(&key, "raw", Bytes::from_static(b"old tail"))
        .await
        .unwrap();
    cache.delete(&key).await.unwrap();
    let new = put(&cache, &key, Bytes::from_static(b"new base")).await;
    assert_ne!(old.uuid, new.uuid);
    cache
        .patch(&key, "raw", Bytes::from_static(b"new tail"))
        .await
        .unwrap();
    for append in [false, true] {
        if append {
            cache
                .patch(&key, "raw", Bytes::from_static(b"new suffix"))
                .await
                .unwrap();
        }
        let error = GrpcCacheServer::new(Arc::clone(&cache))
            .get(request(&key, CacheGetMode::Patches, 2, &old.uuid))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code(), Code::FailedPrecondition);
        let all = get(&cache, &key, CacheGetMode::Object, 0, "").await;
        assert_eq!(metadata(&all).uuid, new.uuid);
        let actual = parts(&all);
        assert_eq!(actual[0].1, b"new base");
        assert_eq!(actual[1].1, b"new tail");
        if append {
            assert_eq!(actual[2].1, b"new suffix");
        }
    }
}

struct TrackedStorage {
    inner: crate::storage::StorageEnginePtr,
    reads: Arc<AtomicUsize>,
    barrier: Option<Arc<CommitBarrier>>,
    delete_barrier: Option<Arc<CommitBarrier>>,
    prefix_list_barrier: Option<Arc<CommitBarrier>>,
}

#[derive(Default)]
struct CommitBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// Hold request EOF after its payload is written so tests can inspect or collect an unfinished upload.
fn held_chunks(payload: Bytes, barrier: Arc<CommitBarrier>, fail: bool) -> ByteStream {
    Box::pin(stream::iter([Ok(payload)]).chain(stream::once(async move {
        barrier.entered.notify_one();
        barrier.release.notified().await;
        if fail {
            Err(FlameError::Internal("fixture upload failed".into()))
        } else {
            Ok(Bytes::new())
        }
    })))
}

impl TrackedStorage {
    /// Observe payload polling without exposing or changing storage's private filesystem readers.
    fn track_chunks(&self, chunks: ChunkStream) -> ChunkStream {
        let reads = Arc::clone(&self.reads);
        Box::pin(stream::unfold(
            (chunks, reads),
            |(mut chunks, reads)| async move {
                reads.fetch_add(1, Ordering::SeqCst);
                chunks.next().await.map(|chunk| (chunk, (chunks, reads)))
            },
        ))
    }
    /// Delay the response after storage has durably committed and published its catalog update.
    async fn complete_write(&self, metadata: ObjectMetadata) -> Result<ObjectMetadata, FlameError> {
        if let Some(barrier) = &self.barrier {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
        Ok(metadata)
    }
}

#[async_trait]
impl StorageEngine for TrackedStorage {
    /// Capture the selected snapshot and count payload stream polls only during consumption.
    async fn read_object(&self, metadata: &mut ObjectMetadata) -> Result<ChunkStream, FlameError> {
        Ok(self.track_chunks(self.inner.read_object(metadata).await?))
    }
    /// Capture a bounded suffix with the same lazy chunk forwarding.
    async fn read_patch(&self, metadata: &mut ObjectMetadata) -> Result<ChunkStream, FlameError> {
        Ok(self.track_chunks(self.inner.read_patch(metadata).await?))
    }
    /// Forward object bytes and pause only after storage publishes the committed result.
    async fn write_object(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError> {
        self.complete_write(
            self.inner
                .write_object(key, data_type, size, chunks)
                .await?,
        )
        .await
    }
    /// Forward patch bytes with the same storage-owned commit and cleanup behavior.
    async fn write_patch(
        &self,
        key: &ObjectKey,
        data_type: &str,
        size: u64,
        chunks: ByteStream,
    ) -> Result<ObjectMetadata, FlameError> {
        self.complete_write(self.inner.write_patch(key, data_type, size, chunks).await?)
            .await
    }
    /// Pause only after the backend has returned an atomic prefix projection and released its activity.
    async fn list(&self, filter: &ObjectFilter) -> Result<Vec<ObjectMetadata>, FlameError> {
        let metadata = self.inner.list(filter).await?;
        if matches!(
            filter,
            ObjectFilter::ByKey {
                version: Some(_),
                ..
            }
        ) {
            if let Some(barrier) = &self.prefix_list_barrier {
                barrier.entered.notify_one();
                barrier.release.notified().await;
            }
        }
        Ok(metadata)
    }
    /// Pause after backend deletion returns, when its storage-owned operation has already ended.
    async fn delete_objects(&self, key: &ObjectKey) -> Result<(), FlameError> {
        self.inner.delete_objects(key).await?;
        if let Some(barrier) = &self.delete_barrier {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
        Ok(())
    }
}

/// Allow deletion of a durable upload even when a wrapper delays its response after backend completion.

#[tokio::test]
async fn namespace_delete_proceeds_after_backend_commit_before_delayed_response() {
    for prefix in ["audit/session/committing", "audit/session", "audit/*"] {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(CommitBarrier::default());
        let storage = TrackedStorage {
            inner: crate::storage::connect(
                directory.path().to_str().unwrap(),
                &pool(16 * 1024 * 1024),
            )
            .await
            .unwrap(),
            reads: Arc::new(AtomicUsize::new(0)),
            barrier: Some(Arc::clone(&barrier)),
            delete_barrier: None,
            prefix_list_barrier: None,
        };
        let cache = Arc::new(Controller::new(endpoint(), Box::new(storage)));
        let key = key("committing");
        let writer_cache = Arc::clone(&cache);
        let writer_key = key.clone();
        let writer = tokio::spawn(async move {
            put(&writer_cache, &writer_key, Bytes::from_static(b"committed")).await
        });
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .unwrap();
        assert!(!cache
            .storage
            .list(&ObjectFilter::ByKey {
                key: key.clone(),
                version: None
            })
            .await
            .unwrap()
            .is_empty());

        tokio::time::timeout(
            Duration::from_secs(2),
            cache.delete(&ObjectKey::from_path(prefix).unwrap()),
        )
        .await
        .expect(
            "completed backend writes must release exclusion before returning through a wrapper",
        )
        .unwrap();
        assert!(!writer.is_finished());
        barrier.release.notify_one();
        writer.await.unwrap();
        assert!(cache
            .storage
            .list(&ObjectFilter::ByKey {
                key: key.clone(),
                version: None
            })
            .await
            .unwrap()
            .is_empty());

        assert!(cache
            .list(&ObjectFilter::AllObject)
            .await
            .unwrap()
            .is_empty());
    }
}
/// Preserve a committed upload after its RPC waiter is canceled without keeping backend exclusion alive.

#[tokio::test]
async fn cancelled_rpc_does_not_retain_exclusion_after_backend_commit() {
    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(CommitBarrier::default());
    let storage = TrackedStorage {
        inner: crate::storage::connect(directory.path().to_str().unwrap(), &pool(16 * 1024 * 1024))
            .await
            .unwrap(),
        reads: Arc::new(AtomicUsize::new(0)),
        barrier: Some(Arc::clone(&barrier)),
        delete_barrier: None,
        prefix_list_barrier: None,
    };
    let cache = Arc::new(Controller::new(endpoint(), Box::new(storage)));
    let key = key("cancelled-put");
    let owned_cache = Arc::clone(&cache);
    let owned_key = key.clone();
    let waiting_rpc = tokio::spawn(async move {
        owned_cache
            .put_stream(WriteRequest {
                key: owned_key,
                data_type: "raw".into(),
                size: 7,
                chunks: Box::pin(stream::iter([Ok(Bytes::from_static(b"payload"))])),
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    assert!(!cache
        .storage
        .list(&ObjectFilter::ByKey {
            key: key.clone(),
            version: None
        })
        .await
        .unwrap()
        .is_empty());

    waiting_rpc.abort();
    assert!(waiting_rpc.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), cache.delete(&key))
        .await
        .expect("a response wrapper must not retain the completed backend mutation")
        .unwrap();
    barrier.release.notify_one();
    assert!(cache
        .storage
        .list(&ObjectFilter::ByKey {
            key: key.clone(),
            version: None
        })
        .await
        .unwrap()
        .is_empty());
}

/// Reject corrupt stored bytes during the filesystem response stream.

#[tokio::test]
async fn corrupt_storage_read_is_never_returned_as_a_successful_response() {
    use std::os::unix::fs::FileExt;
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path().to_str().unwrap()).await;
    let key = key("corrupt-get");
    let size = CHUNK_SIZE + 13;
    put(&cache, &key, Bytes::from(vec![9; size])).await;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(
            directory
                .path()
                .join(key.to_key().unwrap())
                .join("data.bin"),
        )
        .unwrap();
    file.write_all_at(&[10], size as u64 - 1).unwrap();
    file.sync_data().unwrap();
    let response = GrpcCacheServer::new(Arc::clone(&cache))
        .get(request(&key, CacheGetMode::Object, 0, ""))
        .await
        .unwrap()
        .into_inner();
    let error = response.try_collect::<Vec<_>>().await.unwrap_err();
    assert_eq!(error.code(), Code::DataLoss);
}

/// Allow recreation after backend deletion commits even when a canceled response wrapper remains paused.
#[tokio::test]
async fn cancelled_delete_response_does_not_block_recreation_after_backend_completion() {
    {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(CommitBarrier::default());
        let storage = TrackedStorage {
            inner: Box::new(crate::storage::FsStorage::new(directory.path().into()).unwrap()),
            reads: Arc::new(AtomicUsize::new(0)),
            barrier: None,
            delete_barrier: Some(Arc::clone(&barrier)),
            prefix_list_barrier: None,
        };
        let cache = Arc::new(Controller::new(endpoint(), Box::new(storage)));
        let key = key("cancelled-delete");
        let original = put(&cache, &key, Bytes::from_static(b"old payload")).await;
        let deleting = {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            tokio::spawn(async move { cache.delete(&key).await })
        };
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .unwrap();
        deleting.abort();
        assert!(deleting.await.unwrap_err().is_cancelled());
        assert!(cache
            .storage
            .list(&ObjectFilter::AllObject)
            .await
            .unwrap()
            .is_empty());

        let recreating = {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            tokio::spawn(async move {
                cache
                    .storage
                    .write_object(
                        &key,
                        "raw",
                        11,
                        Box::pin(stream::iter([Ok(Bytes::from_static(b"new payload"))])),
                    )
                    .await
            })
        };
        let recreated = tokio::time::timeout(Duration::from_secs(2), recreating)
            .await
            .expect(
                "backend deletion completion must release its operation before wrapper completion",
            )
            .unwrap()
            .unwrap();
        barrier.release.notify_one();
        assert_ne!(recreated.uuid, original.uuid);
        assert_eq!(
            parts(&get(&cache, &key, CacheGetMode::Object, 0, "").await),
            [(0, b"new payload".to_vec())]
        );
        let persisted = cache.storage.list(&ObjectFilter::AllObject).await.unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].uuid, recreated.uuid);
    }
}

/// Return the captured prefix metadata while later deletion and replacement proceed after backend lookup ends.
#[tokio::test]
async fn completed_prefix_projection_does_not_block_later_deletion_or_recreation() {
    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(CommitBarrier::default());
    let storage = TrackedStorage {
        inner: Box::new(crate::storage::FsStorage::new(directory.path().into()).unwrap()),
        reads: Arc::new(AtomicUsize::new(0)),
        barrier: None,
        delete_barrier: None,
        prefix_list_barrier: Some(Arc::clone(&barrier)),
    };
    let cache = Arc::new(Controller::new(endpoint(), Box::new(storage)));
    let key = key("prefix-incarnation");
    let original = put(&cache, &key, Bytes::from_static(b"base")).await;
    cache
        .patch(&key, "raw", Bytes::from_static(b"tail"))
        .await
        .unwrap();
    let listing = {
        let cache = Arc::clone(&cache);
        let key = key.clone();
        tokio::spawn(async move {
            cache
                .list(&ObjectFilter::ByKey {
                    key,
                    version: Some(1),
                })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    cache.delete(&key).await.unwrap();
    let replacement = put(&cache, &key, Bytes::from_static(b"replacement payload")).await;
    barrier.release.notify_one();
    let projected = tokio::time::timeout(Duration::from_secs(5), listing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].uuid, original.uuid);
    assert_eq!((projected[0].version, projected[0].size), (1, 4));
    assert_ne!(replacement.uuid, original.uuid);
    let current = cache.list(&ObjectFilter::AllObject).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].uuid, replacement.uuid);
    assert_eq!((current[0].version, current[0].size), (1, 19));
}

/// A durable indexed part is readable before the writer RPC finishes returning its result.
#[tokio::test]
async fn committed_parts_are_readable_before_writer_completion() {
    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(CommitBarrier::default());
    let storage = TrackedStorage {
        inner: Box::new(crate::storage::FsStorage::new(directory.path().into()).unwrap()),
        reads: Arc::new(AtomicUsize::new(0)),
        barrier: Some(Arc::clone(&barrier)),
        delete_barrier: None,
        prefix_list_barrier: None,
    };
    let controller = Arc::new(Controller::new(endpoint(), Box::new(storage)));
    let key = key("append-completion");
    barrier.release.notify_one();
    let created = put(&controller, &key, Bytes::from_static(b"base")).await;
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    let patching = {
        let controller = Arc::clone(&controller);
        let key = key.clone();
        tokio::spawn(async move {
            controller
                .patch(&key, "raw", Bytes::from_static(b"tail"))
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    let selected = controller
        .list(&ObjectFilter::ByKey {
            key: key.clone(),
            version: None,
        })
        .await
        .unwrap()
        .remove(0);
    assert_eq!(selected.uuid, created.uuid);
    assert_eq!(selected.version, 2);

    let mut reader = tokio::time::timeout(
        Duration::from_secs(5),
        controller.storage.read_object(&mut selected.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        !patching.is_finished(),
        "writer response is still paused after durable commit"
    );
    let mut payloads: Vec<(u64, Vec<u8>)> = Vec::new();
    while let Some(chunk) = reader.next().await.transpose().unwrap() {
        match payloads.last_mut() {
            Some((part, bytes)) if *part == chunk.part => bytes.extend_from_slice(&chunk.data),
            _ => payloads.push((chunk.part, chunk.data.to_vec())),
        }
    }
    assert_eq!(payloads, [(0, b"base".to_vec()), (1, b"tail".to_vec())]);
    barrier.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), patching)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Internal GC deletion completes with an uncommitted patch regardless of whether the request later aborts or commits.
#[tokio::test]
async fn gc_unlinks_while_a_patch_is_uncommitted_without_resurrection() {
    for abort in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(directory.path().to_str().unwrap()).await;
        let key = key("gc-writing");
        put(&cache, &key, Bytes::from_static(b"base")).await;
        let barrier = Arc::new(CommitBarrier::default());
        let patch = {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            let chunks = held_chunks(Bytes::from_static(b"tail"), Arc::clone(&barrier), abort);
            tokio::spawn(async move { cache.storage.write_patch(&key, "raw", 4, chunks).await })
        };
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), cache.delete(&key))
            .await
            .expect("GC must not wait for the writer")
            .unwrap();
        assert!(cache
            .list(&ObjectFilter::AllObject)
            .await
            .unwrap()
            .is_empty());
        barrier.release.notify_one();
        assert!(tokio::time::timeout(Duration::from_secs(5), patch)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(cache
            .list(&ObjectFilter::AllObject)
            .await
            .unwrap()
            .is_empty());
    }
}

/// Internal GC unlink proceeds immediately while an already opened reader completes from its retained file.
#[tokio::test]
async fn gc_unlinks_before_active_reader_eof() {
    let directory = tempfile::tempdir().unwrap();
    let cache = cache(directory.path().to_str().unwrap()).await;
    let key = key("gc-captured");
    let candidate = put(&cache, &key, Bytes::from_static(b"captured")).await;
    let mut reader = cache
        .storage
        .read_object(&mut candidate.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), cache.delete(&key))
        .await
        .expect("GC must not wait for readers")
        .unwrap();
    assert!(cache
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    let mut bytes = Vec::new();
    while let Some(chunk) = reader.next().await.transpose().unwrap() {
        bytes.extend_from_slice(&chunk.data);
    }
    assert_eq!(bytes, b"captured");
}

/// Capture a chunk stream without polling payloads and request only one chunk for the first transfer.
#[tokio::test]
async fn opening_payload_reader_does_not_preload_the_object() {
    let directory = tempfile::tempdir().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let storage = TrackedStorage {
        inner: crate::storage::connect(directory.path().to_str().unwrap(), &pool(16 * 1024 * 1024))
            .await
            .unwrap(),
        reads: Arc::clone(&reads),
        barrier: None,
        delete_barrier: None,
        prefix_list_barrier: None,
    };
    let controller = Arc::new(Controller::new(endpoint(), Box::new(storage)));
    let object = key("bounded-io");
    let size = 3 * CHUNK_SIZE + 17;
    let created = put(&controller, &object, Bytes::from(vec![23; size])).await;
    let meta = get(&controller, &object, CacheGetMode::Metadata, 0, "").await;
    assert_eq!(metadata(&meta).size, size as u64);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    let mut reader = controller
        .storage
        .read_object(&mut created.clone())
        .await
        .unwrap();
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "opening descriptors must not read payloads"
    );
    let first = reader.next().await.transpose().unwrap().unwrap();
    assert_eq!(
        (first.part, first.offset, first.data.len()),
        (0, 0, CHUNK_SIZE)
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the first response must not preload later chunks"
    );
    assert_eq!(first.data, Bytes::from(vec![23; CHUNK_SIZE]));
}

/// Abort partial source failure before committing an object and allow the same key to be retried.
#[tokio::test]
async fn failed_source_aborts_the_filesystem_upload_before_retry() {
    let directory = tempfile::tempdir().unwrap();
    let controller = cache(directory.path().to_str().unwrap()).await;
    let object = key("source-failure");
    let error = controller
        .put_stream(WriteRequest {
            key: object.clone(),
            data_type: "raw".into(),
            size: 7,
            chunks: Box::pin(stream::iter([
                Ok(Bytes::from_static(b"partial")),
                Err(FlameError::Internal("injected source read failure".into())),
            ])),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, FlameError::Internal(_)));
    assert!(controller
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    put(&controller, &object, Bytes::from_static(b"complete")).await;
    assert_eq!(
        parts(&get(&controller, &object, CacheGetMode::Object, 0, "").await),
        [(0, b"complete".to_vec())]
    );
}

/// GET serves the latest committed prefix immediately while the next patch is still being written.
#[tokio::test]
async fn latest_get_does_not_wait_for_an_uncommitted_patch() {
    let directory = tempfile::tempdir().unwrap();
    let controller = cache(directory.path().to_str().unwrap()).await;
    let key = key("read-during-patch");
    let committed = put(&controller, &key, Bytes::from_static(b"base")).await;
    let barrier = Arc::new(CommitBarrier::default());
    let writing = {
        let controller = Arc::clone(&controller);
        let key = key.clone();
        let chunks = held_chunks(Bytes::from_static(b"pending"), Arc::clone(&barrier), false);
        tokio::spawn(async move { controller.storage.write_patch(&key, "raw", 7, chunks).await })
    };
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    let frames = tokio::time::timeout(
        Duration::from_secs(5),
        get(&controller, &key, CacheGetMode::Object, 0, ""),
    )
    .await
    .expect("a read must not wait for the pending patch");
    assert_eq!(
        (metadata(&frames).version, metadata(&frames).uuid.as_str()),
        (1, committed.uuid.as_str())
    );
    assert_eq!(parts(&frames), [(0, b"base".to_vec())]);
    barrier.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), writing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frames = get(&controller, &key, CacheGetMode::Object, 0, "").await;
    assert_eq!(metadata(&frames).version, 2);
    assert_eq!(
        parts(&frames),
        [(0, b"base".to_vec()), (1, b"pending".to_vec())]
    );
}

/// A new object with no committed initial part is absent rather than a read waiting for its writer.
#[tokio::test]
async fn latest_get_returns_not_found_during_an_uncommitted_create() {
    let directory = tempfile::tempdir().unwrap();
    let controller = cache(directory.path().to_str().unwrap()).await;
    let key = key("read-during-create");
    let barrier = Arc::new(CommitBarrier::default());
    let writing = {
        let controller = Arc::clone(&controller);
        let key = key.clone();
        let chunks = held_chunks(Bytes::from_static(b"initial"), Arc::clone(&barrier), false);
        tokio::spawn(async move {
            controller
                .storage
                .write_object(&key, "raw", 7, chunks)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        GrpcCacheServer::new(Arc::clone(&controller)).get(request(
            &key,
            CacheGetMode::Object,
            0,
            "",
        )),
    )
    .await
    .expect("a read must not wait for an unavailable object");
    assert!(matches!(response, Err(status) if status.code() == Code::NotFound));
    barrier.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), writing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frames = get(&controller, &key, CacheGetMode::Object, 0, "").await;
    assert_eq!(metadata(&frames).version, 1);
    assert_eq!(parts(&frames), [(0, b"initial".to_vec())]);
}

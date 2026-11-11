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
use crate::controller::{grpc, CacheEndpoint, Controller, GrpcCacheServer, WriteRequest};
use crate::storage::{ChunkStream, CHUNK_SIZE};
use futures::{stream, StreamExt, TryStreamExt};
use rpc::flame::v1::object_cache_service_server::ObjectCacheService;
use rpc::flame::v1::{cache_get_response, CacheGetMode, CacheGetRequest, CacheGetResponse};
use std::time::Duration;
use tonic::Request;

/// Build a controller fixture for the selected storage backend.
async fn controller(storage: &str) -> Arc<Controller> {
    Arc::new(Controller::new(
        CacheEndpoint {
            scheme: "grpc".into(),
            host: "127.0.0.1".into(),
            port: 9090,
        },
        crate::storage::connect(storage).await.unwrap(),
    ))
}
/// Return the shared complete key used by snapshot regressions.
fn key() -> ObjectKey {
    ObjectKey::try_from("snapshots/session/object").unwrap()
}
/// Create a raw single-part snapshot fixture.
async fn put(controller: &Arc<Controller>, payload: &'static [u8]) -> ObjectMetadata {
    controller
        .put_stream(WriteRequest {
            key: key(),
            data_type: "raw".into(),
            size: payload.len() as u64,
            chunks: Box::pin(stream::iter([Ok(Bytes::from_static(payload))])),
        })
        .await
        .unwrap()
}
/// Capture a full-object GET response without consuming its stream.
async fn capture(controller: &Arc<Controller>) -> grpc::ResponseStream<CacheGetResponse> {
    GrpcCacheServer::new(Arc::clone(controller))
        .get(Request::new(CacheGetRequest {
            key: key().to_key().unwrap(),
            mode: CacheGetMode::Object as i32,
            version: 0,
            uuid: String::new(),
        }))
        .await
        .unwrap()
        .into_inner()
}
/// Collect a response snapshot into its identity, version, and grouped part payloads.
async fn consume(
    stream: grpc::ResponseStream<CacheGetResponse>,
) -> (String, u64, Vec<(u64, Vec<u8>)>) {
    let frames: Vec<_> = stream.try_collect().await.unwrap();
    let Some(cache_get_response::Payload::Header(header)) = frames[0].payload.as_ref() else {
        panic!("missing metadata header");
    };
    let metadata = header.metadata.as_ref().unwrap();
    let mut parts: Vec<(u64, Vec<u8>)> = Vec::new();
    for frame in &frames[1..] {
        let Some(cache_get_response::Payload::Chunk(chunk)) = frame.payload.as_ref() else {
            panic!("expected chunk");
        };
        match parts.last_mut() {
            Some((part, bytes)) if *part == chunk.part => bytes.extend_from_slice(&chunk.data),
            _ => parts.push((chunk.part, chunk.data.to_vec())),
        }
    }
    (metadata.uuid.clone(), metadata.version, parts)
}
/// Allow PATCH to complete while an earlier captured filesystem GET retains its original parts.

#[tokio::test]
async fn captured_get_snapshot_allows_patch_before_stream_is_consumed() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(directory.path().to_str().unwrap()).await;
    let created = put(&controller, b"base").await;
    let snapshot = capture(&controller).await;
    tokio::time::timeout(
        Duration::from_secs(2),
        controller.patch(&key(), "raw", Bytes::from_static(b"patch")),
    )
    .await
    .expect("PATCH must not wait for GET consumption")
    .unwrap();
    let (uuid, version, parts) = consume(snapshot).await;
    assert_eq!(uuid, created.uuid);
    assert_eq!(version, 1);
    assert_eq!(parts, [(0, b"base".to_vec())]);
    let (_, version, parts) = consume(capture(&controller).await).await;
    assert_eq!(version, 2);
    assert_eq!(parts, [(0, b"base".to_vec()), (1, b"patch".to_vec())]);
}
/// Unlink while a response owns its captured inode, then preserve its original response before recreation.

#[tokio::test]
async fn captured_get_survives_gc_unlink_before_recreation() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(directory.path().to_str().unwrap()).await;
    let created = put(&controller, b"original").await;
    let snapshot = capture(&controller).await;
    tokio::time::timeout(Duration::from_secs(2), controller.delete(&key()))
        .await
        .expect("GC must not wait for readers")
        .unwrap();
    let (uuid, version, parts) = consume(snapshot).await;
    assert_eq!(uuid, created.uuid);
    assert_eq!(version, 1);
    assert_eq!(parts, [(0, b"original".to_vec())]);
    controller.delete(&key()).await.unwrap();
    let replacement = put(&controller, b"replacement").await;
    let (uuid, _, parts) = consume(capture(&controller).await).await;
    assert_eq!(uuid, replacement.uuid);
    assert_eq!(parts, [(0, b"replacement".to_vec())]);
}
/// Unlink exact/session/application paths while multipart readers retain their captured inodes.

#[tokio::test]
async fn active_multipart_reader_survives_matching_namespace_deletion() {
    for prefix in [
        "snapshots/session/object",
        "snapshots/session",
        "snapshots/*",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let controller = controller(&format!("file://{}", directory.path().display())).await;
        let created = put(&controller, b"original").await;
        let large = Bytes::from(vec![17; CHUNK_SIZE + 13]);
        controller
            .patch(&key(), "raw", large.clone())
            .await
            .unwrap();
        controller
            .patch(&key(), "raw", Bytes::from_static(b"last"))
            .await
            .unwrap();
        let snapshot = capture(&controller).await;

        let namespace = ObjectKey::from_path(prefix).unwrap();
        tokio::time::timeout(Duration::from_secs(2), controller.delete(&namespace))
            .await
            .expect("GC must not wait for readers")
            .unwrap();
        let (uuid, version, parts) = consume(snapshot).await;
        assert_eq!(uuid, created.uuid);
        assert_eq!(version, 3);
        assert_eq!(
            parts,
            [
                (0, b"original".to_vec()),
                (1, large.to_vec()),
                (2, b"last".to_vec())
            ]
        );
        controller.delete(&namespace).await.unwrap();
        let replacement = put(&controller, b"replacement").await;
        let (uuid, version, parts) = consume(capture(&controller).await).await;
        assert_eq!((uuid, version), (replacement.uuid, 1));
        assert_eq!(parts, [(0, b"replacement".to_vec())]);
    }
}
/// Keep a captured filesystem prefix immutable while PATCH commits a newer part.

#[tokio::test]
async fn filesystem_snapshot_keeps_previous_parts_after_patch() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(&format!("file://{}", directory.path().display())).await;
    let created = put(&controller, b"base").await;
    let snapshot = capture(&controller).await;
    tokio::time::timeout(
        Duration::from_secs(2),
        controller.patch(&key(), "raw", Bytes::from_static(b"patch")),
    )
    .await
    .expect("PATCH must not wait for cold GET consumption")
    .unwrap();
    let (uuid, version, parts) = consume(snapshot).await;
    assert_eq!(uuid, created.uuid);
    assert_eq!(version, 1);
    assert_eq!(parts, [(0, b"base".to_vec())]);

    let (_, version, parts) = consume(capture(&controller).await).await;
    assert_eq!(version, 2);
    assert_eq!(parts, [(0, b"base".to_vec()), (1, b"patch".to_vec())]);
}
/// Unlink immediately while an already captured reader completes from its original data file.

#[tokio::test]
async fn active_reader_survives_object_unlink_before_eof() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(&format!("file://{}", directory.path().display())).await;
    let created = put(&controller, b"deleted").await;
    let snapshot = capture(&controller).await;
    tokio::time::timeout(Duration::from_secs(2), controller.delete(&key()))
        .await
        .expect("GC must not wait for readers")
        .unwrap();
    assert!(controller
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
    let (uuid, version, parts) = consume(snapshot).await;
    assert_eq!(uuid, created.uuid);
    assert_eq!(version, 1);
    assert_eq!(parts, [(0, b"deleted".to_vec())]);
    controller.delete(&key()).await.unwrap();
    assert!(controller
        .storage
        .list(&ObjectFilter::ByKey {
            key: key(),
            version: None
        })
        .await
        .unwrap()
        .is_empty());
}

/// Preserve cold filesystem prefix, suffix, and metadata modes through filtered metadata lookup and opaque readers.
#[tokio::test]
async fn filesystem_get_modes_preserve_empty_parts() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(&format!("file://{}", directory.path().display())).await;
    let created = put(&controller, b"").await;
    controller
        .patch(&key(), "raw", Bytes::from_static(b"body"))
        .await
        .unwrap();
    controller.patch(&key(), "raw", Bytes::new()).await.unwrap();

    for (mode, cursor, expected_version, expected_parts) in [
        (
            CacheGetMode::Object,
            2,
            2,
            vec![(0, Vec::new()), (1, b"body".to_vec())],
        ),
        (
            CacheGetMode::Patches,
            1,
            3,
            vec![(1, b"body".to_vec()), (2, Vec::new())],
        ),
        (CacheGetMode::Metadata, 0, 3, Vec::new()),
    ] {
        let response = GrpcCacheServer::new(Arc::clone(&controller))
            .get(Request::new(CacheGetRequest {
                key: key().to_key().unwrap(),
                mode: mode as i32,
                version: cursor,
                uuid: created.uuid.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let frames: Vec<_> = response.try_collect().await.unwrap();
        let Some(cache_get_response::Payload::Header(header)) = frames[0].payload.as_ref() else {
            panic!("missing metadata header");
        };
        let metadata = header.metadata.as_ref().unwrap();
        assert_eq!(metadata.uuid, created.uuid);
        assert_eq!((metadata.version, metadata.size), (expected_version, 4));
        let parts: Vec<_> = frames
            .into_iter()
            .skip(1)
            .map(|frame| {
                let Some(cache_get_response::Payload::Chunk(chunk)) = frame.payload else {
                    panic!("expected part chunk after header");
                };
                (chunk.part, chunk.data.to_vec())
            })
            .collect();
        assert_eq!(parts, expected_parts);
    }
}

/// Collect the domain reader's selected part bytes without using RPC messages.
async fn domain_parts(mut reader: ChunkStream) -> Vec<(u64, Vec<u8>)> {
    let mut parts: Vec<(u64, Vec<u8>)> = Vec::new();
    while let Some(chunk) = reader.next().await.transpose().unwrap() {
        match parts.last_mut() {
            Some((part, bytes)) if *part == chunk.part => bytes.extend_from_slice(&chunk.data),
            _ => parts.push((chunk.part, chunk.data.to_vec())),
        }
    }
    parts
}

/// Query the current UUID through the controller's exact-key metadata listing.
async fn selected_metadata(cache: &Controller) -> ObjectMetadata {
    cache
        .list(&ObjectFilter::ByKey {
            key: key(),
            version: None,
        })
        .await
        .unwrap()
        .pop()
        .unwrap()
}

/// Keep metadata-selected prefixes readable after a later append.
#[tokio::test]
async fn metadata_lookup_before_patch_reads_old_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(&format!("file://{}", directory.path().display())).await;
    put(&controller, b"base").await;
    let selected = selected_metadata(&controller).await;
    controller
        .patch(&key(), "raw", Bytes::from_static(b"patch"))
        .await
        .unwrap();
    let reader = controller
        .storage
        .read_object(&mut selected.clone())
        .await
        .unwrap();
    assert_eq!(domain_parts(reader).await, [(0, b"base".to_vec())]);
}

/// Reject a metadata generation replaced after lookup through both factories before considering invalid versions.
#[tokio::test]
async fn metadata_lookup_before_recreation_rejects_the_old_uuid() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(directory.path().to_str().unwrap()).await;
    put(&controller, b"old").await;
    let mut selected = selected_metadata(&controller).await;
    controller.delete(&key()).await.unwrap();
    put(&controller, b"replacement").await;
    selected.version = u64::MAX;
    assert!(matches!(
        controller.storage.read_object(&mut selected.clone()).await,
        Err(common::FlameError::InvalidVersion(_))
    ));
    assert!(matches!(
        controller.storage.read_patch(&mut selected.clone()).await,
        Err(common::FlameError::InvalidVersion(_))
    ));
    selected = selected_metadata(&controller).await;
    selected.uuid.clear();
    selected.version = u64::MAX;
    assert!(matches!(
        controller.storage.read_object(&mut selected.clone()).await,
        Err(common::FlameError::InvalidVersion(_))
    ));
    assert!(matches!(
        controller.storage.read_patch(&mut selected.clone()).await,
        Err(common::FlameError::InvalidVersion(_))
    ));
}

/// Reject ahead metadata versions through both factories and return no patches at the current cursor.
#[tokio::test]
async fn metadata_readers_reject_ahead_versions_and_current_patch_cursor_is_empty() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(directory.path().to_str().unwrap()).await;
    put(&controller, b"base").await;
    let selected = selected_metadata(&controller).await;
    let mut ahead = selected.clone();
    ahead.version += 1;
    assert!(matches!(
        controller.storage.read_object(&mut ahead.clone()).await,
        Err(common::FlameError::InvalidConfig(_))
    ));
    assert!(matches!(
        controller.storage.read_patch(&mut ahead.clone()).await,
        Err(common::FlameError::InvalidConfig(_))
    ));
    let reader = controller
        .storage
        .read_patch(&mut selected.clone())
        .await
        .unwrap();
    assert!(domain_parts(reader).await.is_empty());
}

/// Use metadata.version as the patch cursor and publish captured metadata before lazy stream consumption.
#[tokio::test]
async fn read_patch_preserves_parts_and_captures_metadata_from_the_version_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(&format!("file://{}", directory.path().display())).await;
    put(&controller, b"").await;
    let cursor = selected_metadata(&controller).await;
    controller
        .patch(&key(), "raw", Bytes::from_static(b"body"))
        .await
        .unwrap();
    controller.patch(&key(), "raw", Bytes::new()).await.unwrap();

    let mut captured = cursor.clone();
    let chunks = controller.storage.read_patch(&mut captured).await.unwrap();
    assert_eq!((captured.version, captured.size), (3, 4));
    assert_eq!(captured.uuid, cursor.uuid);
    controller
        .patch(&key(), "raw", Bytes::from_static(b"tail"))
        .await
        .unwrap();
    assert_eq!(
        domain_parts(chunks).await,
        [(1, b"body".to_vec()), (2, Vec::new())]
    );

    let chunks = controller.storage.read_patch(&mut captured).await.unwrap();
    assert_eq!((captured.version, captured.size), (4, 8));
    assert_eq!(domain_parts(chunks).await, [(3, b"tail".to_vec())]);
    let chunks = controller.storage.read_patch(&mut captured).await.unwrap();
    assert!(domain_parts(chunks).await.is_empty());
    let mut zero = captured.with_version(0);
    let chunks = controller.storage.read_patch(&mut zero).await.unwrap();
    assert_eq!((zero.version, zero.size), (4, 8));
    assert_eq!(
        domain_parts(chunks).await,
        [
            (0, Vec::new()),
            (1, b"body".to_vec()),
            (2, Vec::new()),
            (3, b"tail".to_vec())
        ]
    );
}

/// An unconsumed response retains only its inode and never delays internal GC deletion.
#[tokio::test]
async fn internal_gc_delete_succeeds_with_an_unconsumed_response() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(directory.path().to_str().unwrap()).await;
    put(&controller, b"payload").await;
    {
        let _response = capture(&controller).await;
        controller.delete(&key()).await.unwrap();
        assert!(controller
            .list(&ObjectFilter::AllObject)
            .await
            .unwrap()
            .is_empty());
    }
    controller.delete(&key()).await.unwrap();
    assert!(controller
        .list(&ObjectFilter::AllObject)
        .await
        .unwrap()
        .is_empty());
}

/// Keep deletion available outside an active reader's exact object, session, and application scopes.
#[tokio::test]
async fn active_reader_allows_unrelated_namespace_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let controller = controller(directory.path().to_str().unwrap()).await;
    put(&controller, b"protected").await;
    let snapshot = capture(&controller).await;
    for (object, selection) in [
        ("snapshots/session/unrelated", "snapshots/session/unrelated"),
        ("snapshots/other/object", "snapshots/other"),
        ("other/session/object", "other/*"),
    ] {
        controller
            .storage
            .write_object(
                &ObjectKey::from_path(object).unwrap(),
                "raw",
                9,
                Box::pin(stream::iter([Ok(Bytes::from_static(b"unrelated"))])),
            )
            .await
            .unwrap();
        controller
            .delete(&ObjectKey::from_path(selection).unwrap())
            .await
            .unwrap();
    }
    assert_eq!(consume(snapshot).await.2, [(0, b"protected".to_vec())]);
    controller.delete(&key()).await.unwrap();
}

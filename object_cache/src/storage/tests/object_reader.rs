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

use super::*;
use crate::model::{Chunk, Part};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct ScriptReader {
    part: Part,
    chunks: VecDeque<Result<Chunk, FlameError>>,
    released: Arc<AtomicUsize>,
}
#[async_trait]
impl PartReader for ScriptReader {
    /// Return the immutable domain descriptor captured by this fixture.
    fn part(&self) -> Part {
        self.part
    }

    /// Forward scripted chunks or integrity failures, then expose successful EOF.
    async fn next_chunk(&mut self) -> Result<Option<Chunk>, FlameError> {
        self.chunks.pop_front().transpose()
    }
}
impl Drop for ScriptReader {
    /// Track release of the captured resource on EOF, failure or cancellation.
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

/// Capture a descriptor-only object view and observe release of its backend reader.
fn reader(chunks: Vec<Result<Chunk, FlameError>>) -> (ObjectReader, Arc<AtomicUsize>) {
    let part = Part::new(0, Bytes::from_static(b"abc"));
    let released = Arc::new(AtomicUsize::new(0));
    let reader = ScriptReader {
        part,
        chunks: chunks.into(),
        released: Arc::clone(&released),
    };
    let object = Object {
        metadata: ObjectMetadata::new().with_version(1).with_size(3),
        parts: vec![part],
    };
    (ObjectReader::new(object, vec![Box::new(reader)]), released)
}

/// Release a captured backend reader exactly once when next_chunk exposes EOF.
#[tokio::test]
async fn reader_releases_each_part_after_observing_eof() {
    let (mut reader, released) = reader(vec![Ok(Chunk {
        part: 0,
        offset: 0,
        data: Bytes::from_static(b"abc"),
    })]);
    assert_eq!(reader.metadata().version, 1);
    assert_eq!(reader.metadata().size, 3);
    let chunk = reader.next_chunk().await.unwrap().unwrap();
    assert_eq!(
        (chunk.part, chunk.offset, chunk.data),
        (0, 0, Bytes::from_static(b"abc"))
    );
    assert_eq!(released.load(Ordering::SeqCst), 0);
    assert!(reader.next_chunk().await.unwrap().is_none());
    assert!(reader.next_chunk().await.unwrap().is_none());
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

/// Propagate verification failure after partial delivery and release the failed reader.
#[tokio::test]
async fn reader_propagates_late_chunk_error_and_terminates() {
    let (reader, released) = reader(vec![
        Ok(Chunk {
            part: 0,
            offset: 0,
            data: Bytes::from_static(b"abc"),
        }),
        Err(FlameError::DataLoss("late integrity failure".into())),
    ]);
    let mut chunks = reader.into_stream();
    assert!(chunks.next().await.unwrap().is_ok());
    assert!(matches!(
        chunks.next().await,
        Some(Err(FlameError::DataLoss(_)))
    ));
    assert!(chunks.next().await.is_none());
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

/// Reject a captured descriptor without its storage handle instead of reporting successful EOF.
#[tokio::test]
async fn missing_captured_part_reader_is_data_loss() {
    let mut reader = ObjectReader::new(
        Object {
            metadata: ObjectMetadata::new().with_version(1).with_size(3),
            parts: vec![Part::new(0, Bytes::from_static(b"abc"))],
        },
        Vec::new(),
    );
    assert!(matches!(
        reader.next_chunk().await,
        Err(FlameError::DataLoss(_))
    ));
    assert!(reader.next_chunk().await.unwrap().is_none());
}

/// Keep source errors visible and immediately release the failed backend resource.
#[tokio::test]
async fn reader_error_is_not_reported_as_clean_eof() {
    let (mut reader, released) = reader(vec![Err(FlameError::DataLoss("source failure".into()))]);
    assert!(matches!(
        reader.next_chunk().await,
        Err(FlameError::DataLoss(_))
    ));
    assert!(reader.next_chunk().await.unwrap().is_none());
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

/// Dropping an unpolled public-shaped stream releases its eagerly captured backend readers without reading payloads.
#[test]
fn unpolled_chunk_stream_releases_captured_readers_on_drop() {
    let released = {
        let (reader, released) = reader(vec![Ok(Chunk {
            part: 0,
            offset: 0,
            data: Bytes::from_static(b"abc"),
        })]);
        {
            let _chunks = reader.into_stream();
            assert_eq!(released.load(Ordering::SeqCst), 0);
        }
        released
    };
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

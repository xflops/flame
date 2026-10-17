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

//! The streaming get response, as a state machine in the style of the
//! executor manager. Each stage is a handler that emits at most one message
//! and moves the response to the next stage:
//!
//! ```text
//! Header -> Next -> Reading -+-> Next (part complete) -> ... -> Done
//!             |              +-> Reading (more chunks)
//!             |              +-> Done (read error, after the error message)
//!             +-> Done (no parts left)
//! ```
//!
//! The optional read guard keeps writers out until the stream is dropped.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use rpc::flame::v1::{cache_get_response, CacheGetChunk, CacheGetHeader, CacheGetResponse};
use tokio::sync::OwnedRwLockReadGuard;
use tonic::Status;

use super::grpc::{ResponseStream, CHUNK_SIZE};
use super::{ObjectCache, ObjectKey};

/// A part of a get response, read from storage chunk by chunk while the
/// response is sent: one `ObjectData` of the object.
pub(super) struct ObjectPart {
    /// The position of the data in the object; its version is `index + 1`.
    pub(super) index: usize,
    pub(super) len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseStage {
    Header,
    Next,
    Reading,
    Done,
}

/// The data shared by all stages of one response.
struct ResponseData {
    stage: ResponseStage,
    cache: Arc<ObjectCache>,
    key: ObjectKey,
    header: Option<CacheGetHeader>,
    parts: std::vec::IntoIter<ObjectPart>,
    current: Option<ObjectPart>,
    offset: u64,
    /// Held while chunks are read from storage so writers cannot replace the
    /// object mid-response.
    _guard: Option<OwnedRwLockReadGuard<()>>,
}

impl ResponseData {
    /// Read and emit one chunk of the current part.
    async fn read_chunk(&mut self) -> Option<Result<CacheGetResponse, Status>> {
        let (index, len) = {
            let part = self.current.as_ref()?;
            (part.index, part.len)
        };
        let size = (len - self.offset).min(CHUNK_SIZE as u64) as usize;
        let read = self
            .cache
            .storage
            .read_range(&self.key, index, self.offset, size)
            .await;
        match read {
            Ok(data) => {
                self.offset += data.len() as u64;
                if self.offset == len {
                    self.current = None;
                    self.stage = ResponseStage::Next;
                }
                Some(Ok(CacheGetResponse {
                    payload: Some(cache_get_response::Payload::Chunk(CacheGetChunk {
                        version: index as u64 + 1,
                        data,
                    })),
                }))
            }
            Err(error) => {
                self.current = None;
                self.parts = Vec::new().into_iter();
                self.stage = ResponseStage::Done;
                Some(Err(Status::from(error)))
            }
        }
    }
}

/// Emits the next message of the response, or `None` at the end of the stream.
#[async_trait]
trait State: Send {
    async fn execute(&mut self) -> Option<Result<CacheGetResponse, Status>>;

    fn stage(&self) -> ResponseStage;

    fn into_data(self: Box<Self>) -> ResponseData;
}

fn from(data: ResponseData) -> Box<dyn State> {
    match data.stage {
        ResponseStage::Header => Box::new(HeaderState { data }),
        ResponseStage::Next => Box::new(NextPartState { data }),
        ResponseStage::Reading => Box::new(ReadingState { data }),
        ResponseStage::Done => Box::new(DoneState { data }),
    }
}

macro_rules! state_data {
    () => {
        fn stage(&self) -> ResponseStage {
            self.data.stage
        }

        fn into_data(self: Box<Self>) -> ResponseData {
            self.data
        }
    };
}

struct HeaderState {
    data: ResponseData,
}

#[async_trait]
impl State for HeaderState {
    async fn execute(&mut self) -> Option<Result<CacheGetResponse, Status>> {
        let header = self.data.header.take()?;
        self.data.stage = ResponseStage::Next;
        Some(Ok(CacheGetResponse {
            payload: Some(cache_get_response::Payload::Header(header)),
        }))
    }

    state_data!();
}

struct NextPartState {
    data: ResponseData,
}

#[async_trait]
impl State for NextPartState {
    async fn execute(&mut self) -> Option<Result<CacheGetResponse, Status>> {
        self.data.current = self.data.parts.next();
        self.data.offset = 0;
        if self.data.current.is_none() {
            self.data.stage = ResponseStage::Done;
            return None;
        }
        // A part has no message of its own; emit its first chunk.
        self.data.stage = ResponseStage::Reading;
        self.data.read_chunk().await
    }

    state_data!();
}

struct ReadingState {
    data: ResponseData,
}

#[async_trait]
impl State for ReadingState {
    async fn execute(&mut self) -> Option<Result<CacheGetResponse, Status>> {
        self.data.read_chunk().await
    }

    state_data!();
}

struct DoneState {
    data: ResponseData,
}

#[async_trait]
impl State for DoneState {
    async fn execute(&mut self) -> Option<Result<CacheGetResponse, Status>> {
        None
    }

    state_data!();
}

/// The get response: the header, then every part chunk by chunk.
pub(super) fn response_stream(
    cache: Arc<ObjectCache>,
    key: ObjectKey,
    header: CacheGetHeader,
    parts: Vec<ObjectPart>,
    guard: Option<OwnedRwLockReadGuard<()>>,
) -> ResponseStream<CacheGetResponse> {
    let state = from(ResponseData {
        stage: ResponseStage::Header,
        cache,
        key,
        header: Some(header),
        parts: parts.into_iter(),
        current: None,
        offset: 0,
        _guard: guard,
    });
    // A handler is built only when the stage changes.
    Box::pin(stream::unfold(state, |mut state| async move {
        let stage = state.stage();
        let response = state.execute().await?;
        if state.stage() != stage {
            state = from(state.into_data());
        }
        Some((response, state))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheEndpoint, Object};
    use futures::StreamExt;

    const KEY: &str = "app/ssn/obj";

    async fn cache_with_object() -> Arc<ObjectCache> {
        let cache = Arc::new(
            ObjectCache::new(
                CacheEndpoint {
                    scheme: "grpc".to_string(),
                    host: "127.0.0.1".to_string(),
                    port: 9090,
                },
                crate::storage::connect("none", None).await.unwrap(),
            )
            .unwrap(),
        );
        let key = ObjectKey::try_from(KEY).unwrap();
        cache
            .put(
                key.clone(),
                Object::new()
                    .append_data(b"first".to_vec())
                    .with_data_type("raw"),
            )
            .await
            .unwrap();
        cache
            .patch(
                &key,
                Object::new()
                    .append_data(b"second".to_vec())
                    .with_data_type("raw"),
            )
            .await
            .unwrap();
        cache
    }

    fn header() -> CacheGetHeader {
        CacheGetHeader {
            version: 2,
            data_type: "raw".to_string(),
        }
    }

    fn part(index: usize, len: u64) -> ObjectPart {
        ObjectPart { index, len }
    }

    fn data(cache: Arc<ObjectCache>, parts: Vec<ObjectPart>) -> ResponseData {
        ResponseData {
            stage: ResponseStage::Header,
            cache,
            key: ObjectKey::try_from(KEY).unwrap(),
            header: Some(header()),
            parts: parts.into_iter(),
            current: None,
            offset: 0,
            _guard: None,
        }
    }

    /// Run one handler for the current stage and return its output.
    async fn step(data: ResponseData) -> (Option<Result<CacheGetResponse, Status>>, ResponseData) {
        let mut state = from(data);
        let output = state.execute().await;
        (output, state.into_data())
    }

    fn two_parts() -> Vec<ObjectPart> {
        vec![part(0, 5), part(1, 6)]
    }

    #[tokio::test]
    async fn full_response_walks_every_stage() {
        let mut data = data(cache_with_object().await, two_parts());
        let mut seen = Vec::new();
        loop {
            let (output, next) = step(data).await;
            data = next;
            seen.push(data.stage);
            if output.is_none() {
                break;
            }
        }
        use ResponseStage::*;
        // A part that fits one chunk completes in the stage that starts it.
        assert_eq!(seen, [Next, Next, Next, Done]);
    }

    #[tokio::test]
    async fn large_part_stays_in_reading_until_complete() {
        let cache = cache_with_object().await;
        let key = ObjectKey::try_from("app/ssn/large").unwrap();
        let size = CHUNK_SIZE + 10;
        cache
            .put(
                key.clone(),
                Object::new()
                    .append_data(vec![7u8; size])
                    .with_data_type("raw"),
            )
            .await
            .unwrap();
        let mut data = data(cache, vec![part(0, size as u64)]);
        data.key = key;
        data.stage = ResponseStage::Next;
        let mut seen = Vec::new();
        loop {
            let (output, next) = step(data).await;
            data = next;
            seen.push(data.stage);
            if output.is_none() {
                break;
            }
        }
        use ResponseStage::*;
        assert_eq!(seen, [Reading, Next, Done]);
    }

    #[tokio::test]
    async fn full_response_emits_header_then_parts() {
        let cache = cache_with_object().await;
        let messages: Vec<_> = response_stream(
            cache,
            ObjectKey::try_from(KEY).unwrap(),
            header(),
            two_parts(),
            None,
        )
        .collect()
        .await;
        let payloads: Vec<_> = messages
            .into_iter()
            .map(|message| message.unwrap().payload.unwrap())
            .collect();
        assert_eq!(payloads.len(), 3);
        assert!(matches!(
            payloads[0],
            cache_get_response::Payload::Header(_)
        ));
        let chunk = |index: usize| match &payloads[index] {
            cache_get_response::Payload::Chunk(chunk) => chunk.clone(),
            _ => panic!("expected chunk"),
        };
        assert_eq!(chunk(1).version, 1);
        assert_eq!(chunk(1).data.as_ref(), b"first");
        assert_eq!(chunk(2).version, 2);
        assert_eq!(chunk(2).data.as_ref(), b"second");
    }

    #[tokio::test]
    async fn not_modified_yields_header_then_end() {
        let cache = cache_with_object().await;
        let mut stream = response_stream(
            cache,
            ObjectKey::try_from(KEY).unwrap(),
            header(),
            Vec::new(),
            None,
        );
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(
            first.payload,
            Some(cache_get_response::Payload::Header(_))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn read_error_yields_status_then_end() {
        let cache = cache_with_object().await;
        // The part claims more bytes than the object holds.
        let parts = vec![part(0, 100), part(1, 6)];
        let mut data = data(cache, parts);
        data.stage = ResponseStage::Next;
        let (output, data) = step(data).await;
        assert!(output.unwrap().is_err());
        assert_eq!(data.stage, ResponseStage::Done);
        assert!(data.current.is_none());
        assert_eq!(data.parts.len(), 0);
        let (output, _) = step(data).await;
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn guard_is_released_when_the_stream_ends() {
        let cache = cache_with_object().await;
        let lock = cache.get_key_lock(KEY).unwrap();
        let guard = Arc::clone(&lock).read_owned().await;
        let mut stream = response_stream(
            cache,
            ObjectKey::try_from(KEY).unwrap(),
            header(),
            two_parts(),
            Some(guard),
        );
        while let Some(message) = stream.next().await {
            message.unwrap();
            if lock.try_write().is_ok() {
                panic!("guard released before the stream ended");
            }
        }
        assert!(lock.try_write().is_ok());
    }
}

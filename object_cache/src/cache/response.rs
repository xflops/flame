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
//!             |              +-> Error -> Done
//!             +-> Done (no parts left)
//! ```
//!
//! The optional read guard keeps writers out until the stream is dropped.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use common::FlameError;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use rpc::flame::v1::{cache_get_response, CacheGetChunk, CacheGetHeader, CacheGetResponse};
use tokio::sync::OwnedRwLockReadGuard;
use tonic::Status;

use super::grpc::ResponseStream;
#[cfg(test)]
use super::grpc::CHUNK_SIZE;
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
    Error,
    Done,
}

/// The context shared by all stages of one response.
struct ResponseData {
    cache: Arc<ObjectCache>,
    key: ObjectKey,
    header: Option<CacheGetHeader>,
    parts: std::vec::IntoIter<ObjectPart>,
    /// Held until the stream ends or is dropped, including after an error.
    _guard: Option<OwnedRwLockReadGuard<()>>,
}

struct StateResult {
    response: Option<CacheGetResponse>,
    next: Box<dyn State>,
}

/// Emits the next message and transfers ownership to the next response stage.
#[async_trait]
trait State: Send {
    async fn execute(self: Box<Self>) -> StateResult;

    fn stage(&self) -> ResponseStage;

    fn error(&self) -> Option<Status> {
        None
    }
}

fn failed(data: ResponseData, error: Status) -> StateResult {
    StateResult {
        response: None,
        next: Box::new(ErrorState { data, error }),
    }
}

struct HeaderState {
    data: ResponseData,
}

#[async_trait]
impl State for HeaderState {
    async fn execute(mut self: Box<Self>) -> StateResult {
        let Some(header) = self.data.header.take() else {
            return StateResult {
                response: None,
                next: Box::new(DoneState { _data: self.data }),
            };
        };
        StateResult {
            response: Some(CacheGetResponse {
                payload: Some(cache_get_response::Payload::Header(header)),
            }),
            next: Box::new(NextPartState { data: self.data }),
        }
    }

    fn stage(&self) -> ResponseStage {
        ResponseStage::Header
    }
}

struct NextPartState {
    data: ResponseData,
}

#[async_trait]
impl State for NextPartState {
    async fn execute(mut self: Box<Self>) -> StateResult {
        let Some(part) = self.data.parts.next() else {
            return StateResult {
                response: None,
                next: Box::new(DoneState { _data: self.data }),
            };
        };
        let reader = match self
            .data
            .cache
            .storage
            .read_part(&self.data.key, part.index)
            .await
        {
            Ok(reader) => reader,
            Err(error) => return failed(self.data, Status::from(error)),
        };
        // A part has no message of its own; emit its first chunk.
        Box::new(ReadingState {
            data: self.data,
            part,
            offset: 0,
            reader,
        })
        .execute()
        .await
    }

    fn stage(&self) -> ResponseStage {
        ResponseStage::Next
    }
}

struct ReadingState {
    data: ResponseData,
    part: ObjectPart,
    offset: u64,
    reader: BoxStream<'static, Result<Bytes, FlameError>>,
}

impl ReadingState {
    /// At the advertised boundary, poll through EOF before sending the last
    /// chunk: integrity failures can arrive after all payload bytes were read.
    async fn read_chunk(&mut self) -> Result<CacheGetResponse, Status> {
        let data = self
            .reader
            .next()
            .await
            .ok_or_else(|| Status::data_loss("object part ended before its advertised length"))?
            .map_err(Status::from)?;
        let offset = self
            .offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| Status::data_loss("object part length overflow"))?;
        if offset > self.part.len || (data.is_empty() && self.part.len != 0) {
            return Err(Status::data_loss("object part length mismatch"));
        }
        self.offset = offset;
        if self.offset == self.part.len {
            match self.reader.next().await {
                Some(Err(error)) => return Err(Status::from(error)),
                Some(Ok(_)) => {
                    return Err(Status::data_loss(
                        "object part exceeds its advertised length",
                    ))
                }
                None => {}
            }
        }
        Ok(CacheGetResponse {
            payload: Some(cache_get_response::Payload::Chunk(CacheGetChunk {
                version: self.part.index as u64 + 1,
                data,
            })),
        })
    }
}

#[async_trait]
impl State for ReadingState {
    async fn execute(mut self: Box<Self>) -> StateResult {
        match self.read_chunk().await {
            Ok(response) => {
                let next: Box<dyn State> = if self.offset == self.part.len {
                    Box::new(NextPartState { data: self.data })
                } else {
                    self
                };
                StateResult {
                    response: Some(response),
                    next,
                }
            }
            Err(error) => failed(self.data, error),
        }
    }

    fn stage(&self) -> ResponseStage {
        ResponseStage::Reading
    }
}

struct ErrorState {
    data: ResponseData,
    error: Status,
}

#[async_trait]
impl State for ErrorState {
    async fn execute(self: Box<Self>) -> StateResult {
        StateResult {
            response: None,
            next: Box::new(DoneState { _data: self.data }),
        }
    }

    fn stage(&self) -> ResponseStage {
        ResponseStage::Error
    }

    fn error(&self) -> Option<Status> {
        Some(self.error.clone())
    }
}

struct DoneState {
    _data: ResponseData,
}

#[async_trait]
impl State for DoneState {
    async fn execute(self: Box<Self>) -> StateResult {
        StateResult {
            response: None,
            next: self,
        }
    }

    fn stage(&self) -> ResponseStage {
        ResponseStage::Done
    }
}

/// The get response: the header, then every part chunk by chunk.
pub(super) fn response_stream(
    cache: Arc<ObjectCache>,
    key: ObjectKey,
    header: CacheGetHeader,
    parts: Vec<ObjectPart>,
    guard: Option<OwnedRwLockReadGuard<()>>,
) -> ResponseStream<CacheGetResponse> {
    let state: Box<dyn State> = Box::new(HeaderState {
        data: ResponseData {
            cache,
            key,
            header: Some(header),
            parts: parts.into_iter(),
            _guard: guard,
        },
    });
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if state.stage() == ResponseStage::Done {
                return None;
            }
            let error = state.error();
            let StateResult { response, next } = state.execute().await;
            state = next;
            if let Some(error) = error {
                return Some((Err(error), state));
            }
            if let Some(response) = response {
                return Some((Ok(response), state));
            }
        }
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
            cache,
            key: ObjectKey::try_from(KEY).unwrap(),
            header: Some(header()),
            parts: parts.into_iter(),
            _guard: None,
        }
    }

    fn two_parts() -> Vec<ObjectPart> {
        vec![part(0, 5), part(1, 6)]
    }

    #[tokio::test]
    async fn full_response_walks_every_stage() {
        let mut state: Box<dyn State> = Box::new(HeaderState {
            data: data(cache_with_object().await, two_parts()),
        });
        let mut seen = Vec::new();
        loop {
            let StateResult {
                response: output,
                next,
            } = state.execute().await;
            state = next;
            seen.push(state.stage());
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
        let mut state: Box<dyn State> = Box::new(NextPartState { data });
        let mut seen = Vec::new();
        loop {
            let StateResult {
                response: output,
                next,
            } = state.execute().await;
            state = next;
            seen.push(state.stage());
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
        let state: Box<dyn State> = Box::new(NextPartState {
            data: data(cache, parts),
        });
        let StateResult {
            response: output,
            next: state,
        } = state.execute().await;
        assert!(output.is_some());
        let StateResult {
            response: output,
            next: state,
        } = state.execute().await;
        assert!(output.is_none());
        assert_eq!(state.stage(), ResponseStage::Error);
        assert_eq!(state.error().unwrap().code(), tonic::Code::DataLoss);
        let StateResult {
            response: output,
            next: state,
        } = state.execute().await;
        assert!(output.is_none());
        assert_eq!(state.stage(), ResponseStage::Done);
        let StateResult {
            response: output, ..
        } = state.execute().await;
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn integrity_error_after_the_last_payload_chunk_is_not_dropped() {
        let state: Box<dyn State> = Box::new(ReadingState {
            data: data(cache_with_object().await, Vec::new()),
            part: part(0, 5),
            offset: 0,
            reader: Box::pin(stream::iter(vec![
                Ok(Bytes::from_static(b"first")),
                Err(FlameError::DataLoss("checksum mismatch".to_string())),
            ])),
        });
        let StateResult {
            response: output,
            next: state,
        } = state.execute().await;
        assert!(output.is_none());
        assert_eq!(state.stage(), ResponseStage::Error);
        assert_eq!(state.error().unwrap().code(), tonic::Code::DataLoss);
        let StateResult {
            response: output,
            next: state,
        } = state.execute().await;
        assert!(output.is_none());
        assert_eq!(state.stage(), ResponseStage::Done);
        let StateResult {
            response: output, ..
        } = state.execute().await;
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

    #[tokio::test]
    async fn guard_is_retained_after_an_error_until_the_stream_ends() {
        let cache = cache_with_object().await;
        let lock = cache.get_key_lock(KEY).unwrap();
        let guard = Arc::clone(&lock).read_owned().await;
        let mut stream = response_stream(
            cache,
            ObjectKey::try_from(KEY).unwrap(),
            header(),
            vec![part(99, 1)],
            Some(guard),
        );
        stream.next().await.unwrap().unwrap();
        assert!(stream.next().await.unwrap().is_err());
        assert!(lock.try_write().is_err());
        assert!(stream.next().await.is_none());
        assert!(lock.try_write().is_ok());
    }

    #[tokio::test]
    async fn guard_is_released_when_a_reading_stream_is_dropped() {
        let cache = cache_with_object().await;
        let key = ObjectKey::try_from("app/ssn/cancelled").unwrap();
        let size = CHUNK_SIZE + 10;
        cache
            .put(key.clone(), Object::new().append_data(vec![7u8; size]))
            .await
            .unwrap();
        let lock = cache.get_key_lock(&key.to_key().unwrap()).unwrap();
        {
            let guard = Arc::clone(&lock).read_owned().await;
            let mut stream = response_stream(
                cache,
                key,
                header(),
                vec![part(0, size as u64)],
                Some(guard),
            );
            stream.next().await.unwrap().unwrap();
            stream.next().await.unwrap().unwrap();
            assert!(lock.try_write().is_err());
        }
        assert!(lock.try_write().is_ok());
    }
}

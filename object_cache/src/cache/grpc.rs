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

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{stream, Stream};
use rpc::flame::v1::object_cache_service_server::ObjectCacheService;
use rpc::flame::v1::{
    cache_get_response, cache_write_request, CacheChunkKind, CacheDeleteRequest,
    CacheDeleteResponse, CacheGetChunk, CacheGetHeader, CacheGetMetadataRequest, CacheGetMode,
    CacheGetRequest, CacheGetResponse, CacheListRequest, CacheObjectMetadata, CacheWriteRequest,
};
use stdng::lock_ptr;
use tonic::{Request, Response, Status, Streaming};

use super::{Object, ObjectCache, ObjectKey, ObjectMetadata};

const CHUNK_SIZE: usize = 1024 * 1024;

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

pub struct GrpcCacheServer {
    cache: Arc<ObjectCache>,
}

impl GrpcCacheServer {
    pub fn new(cache: Arc<ObjectCache>) -> Self {
        Self { cache }
    }
}

impl From<ObjectMetadata> for CacheObjectMetadata {
    fn from(metadata: ObjectMetadata) -> Self {
        Self {
            endpoint: metadata.endpoint,
            key: metadata.key,
            version: metadata.version,
            size: metadata.size,
            delta_count: metadata.delta_count,
            creation_time: metadata.creation_time,
            data_type: metadata.data_type,
        }
    }
}

struct ObjectPart {
    kind: CacheChunkKind,
    version: u64,
    data: Bytes,
}

fn object_part(object: &Object, kind: CacheChunkKind) -> ObjectPart {
    ObjectPart {
        kind,
        version: object.version,
        data: object.data.clone(),
    }
}

fn response_stream(
    header: CacheGetHeader,
    parts: Vec<ObjectPart>,
) -> ResponseStream<CacheGetResponse> {
    let mut header = Some(header);
    let mut parts = parts.into_iter();
    let mut current: Option<ObjectPart> = None;
    let mut offset = 0;
    Box::pin(stream::unfold((), move |_| {
        let response = if let Some(header) = header.take() {
            Some(CacheGetResponse {
                payload: Some(cache_get_response::Payload::Header(header)),
            })
        } else {
            if current.is_none() {
                current = parts.next();
                offset = 0;
            }
            if let Some(part) = current.as_ref() {
                let end = (offset + CHUNK_SIZE).min(part.data.len());
                let chunk = CacheGetChunk {
                    kind: part.kind as i32,
                    version: part.version,
                    data: part.data.slice(offset..end),
                };
                offset = end;
                if end == part.data.len() {
                    current = None;
                }
                Some(CacheGetResponse {
                    payload: Some(cache_get_response::Payload::Chunk(chunk)),
                })
            } else {
                None
            }
        };
        async move { response.map(|response| (Ok(response), ())) }
    }))
}

async fn collect_write(
    request: Request<Streaming<CacheWriteRequest>>,
) -> Result<(ObjectKey, String, Bytes), Status> {
    let mut messages = request.into_inner();
    let first = messages
        .message()
        .await?
        .ok_or_else(|| Status::invalid_argument("missing write header"))?;
    let header = match first.payload {
        Some(cache_write_request::Payload::Header(header)) => header,
        _ => {
            return Err(Status::invalid_argument(
                "first write message must be a header",
            ))
        }
    };
    let key = ObjectKey::from_path(&header.key).map_err(Status::from)?;
    if header.data_type.is_empty() || header.data_type.len() > 128 {
        return Err(Status::invalid_argument(
            "data_type must contain 1 to 128 UTF-8 bytes",
        ));
    }
    let mut data = Vec::new();
    while let Some(message) = messages.message().await? {
        match message.payload {
            Some(cache_write_request::Payload::Data(chunk)) => {
                if chunk.len() > CHUNK_SIZE {
                    return Err(Status::invalid_argument("write chunk exceeds 1 MiB"));
                }
                data.extend_from_slice(&chunk);
            }
            _ => {
                return Err(Status::invalid_argument(
                    "write stream contains another header",
                ))
            }
        }
    }
    Ok((key, header.data_type, Bytes::from(data)))
}

#[tonic::async_trait]
impl ObjectCacheService for GrpcCacheServer {
    type GetStream = ResponseStream<CacheGetResponse>;
    type ListStream = ResponseStream<CacheObjectMetadata>;

    async fn put(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let (key, data_type, data) = collect_write(request).await?;
        let object = Object::new_typed(0, data, data_type);
        let metadata = self.cache.put(key, object).await.map_err(Status::from)?;
        Ok(Response::new(metadata.into()))
    }

    async fn patch(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let (key, data_type, data) = collect_write(request).await?;
        if key.object_id.is_none() {
            return Err(Status::invalid_argument("patch requires a full object key"));
        }
        let metadata = self
            .cache
            .patch(&key, Object::new_typed(0, data, data_type))
            .await
            .map_err(Status::from)?;
        Ok(Response::new(metadata.into()))
    }

    async fn get(
        &self,
        request: Request<CacheGetRequest>,
    ) -> Result<Response<Self::GetStream>, Status> {
        let request = request.into_inner();
        let key = ObjectKey::try_from(request.key.as_str()).map_err(Status::from)?;
        let key_lock = self
            .cache
            .get_key_lock(&request.key)
            .map_err(Status::from)?;
        let _guard = key_lock.read().await;

        let not_modified = |version| CacheGetHeader {
            mode: CacheGetMode::NotModified as i32,
            version,
            data_type: String::new(),
        };
        if request.client_version != 0
            && self
                .cache
                .resident_version(&request.key)
                .map_err(Status::from)?
                == Some(request.client_version)
        {
            self.cache.eviction_policy.on_access(&request.key);
            return Ok(Response::new(response_stream(
                not_modified(request.client_version),
                Vec::new(),
            )));
        }

        let object = self.cache.get(&key).await.map_err(Status::from)?;
        let server_version = object.current_version();
        if request.client_version != 0 && server_version == request.client_version {
            return Ok(Response::new(response_stream(
                not_modified(server_version),
                Vec::new(),
            )));
        }

        let mut parts = Vec::new();
        let mut mode = CacheGetMode::Full;
        if request.client_version != 0
            && request.client_version <= server_version
            && object.version <= request.client_version
        {
            let needed: Vec<_> = object
                .deltas
                .iter()
                .filter(|delta| delta.version > request.client_version)
                .collect();
            let expected = server_version.saturating_sub(request.client_version) as usize;
            if needed.len() == expected
                && needed.iter().enumerate().all(|(index, delta)| {
                    delta.version == request.client_version + index as u64 + 1
                })
            {
                mode = CacheGetMode::Patches;
                for delta in needed {
                    parts.push(object_part(delta, CacheChunkKind::Patch));
                }
            }
        }
        if mode == CacheGetMode::Full {
            parts.push(object_part(&object, CacheChunkKind::Base));
            for delta in &object.deltas {
                parts.push(object_part(delta, CacheChunkKind::Patch));
            }
        }
        let header = CacheGetHeader {
            mode: mode as i32,
            version: server_version,
            data_type: object.data_type.clone(),
        };
        Ok(Response::new(response_stream(header, parts)))
    }

    async fn delete(
        &self,
        request: Request<CacheDeleteRequest>,
    ) -> Result<Response<CacheDeleteResponse>, Status> {
        let key = ObjectKey::from_path(&request.into_inner().key).map_err(Status::from)?;
        self.cache.delete(&key).await.map_err(Status::from)?;
        Ok(Response::new(CacheDeleteResponse {}))
    }

    async fn get_metadata(
        &self,
        request: Request<CacheGetMetadataRequest>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let key = request.into_inner().key;
        ObjectKey::try_from(key.as_str()).map_err(Status::from)?;
        let metadata =
            lock_ptr!(self.cache.metadata).map_err(|error| Status::internal(error.to_string()))?;
        metadata
            .get(&key)
            .cloned()
            .map(Into::into)
            .map(Response::new)
            .ok_or_else(|| Status::not_found(format!("object <{}> not found", key)))
    }

    async fn list(
        &self,
        _request: Request<CacheListRequest>,
    ) -> Result<Response<Self::ListStream>, Status> {
        let metadata = self.cache.list_all().await.map_err(Status::from)?;
        let items = metadata.into_iter().map(|item| Ok(item.into()));
        Ok(Response::new(Box::pin(stream::iter(items))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheEndpoint;
    use rpc::flame::v1::object_cache_service_client::ObjectCacheServiceClient;
    use rpc::flame::v1::object_cache_service_server::ObjectCacheServiceServer;
    use rpc::flame::v1::{cache_get_response, cache_write_request, CacheWriteHeader};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    async fn client() -> ObjectCacheServiceClient<tonic::transport::Channel> {
        let cache = Arc::new(
            ObjectCache::new(
                CacheEndpoint {
                    scheme: "grpc".to_string(),
                    host: "127.0.0.1".to_string(),
                    port: 9090,
                },
                crate::storage::connect("none").await.unwrap(),
                None,
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ObjectCacheServiceServer::new(GrpcCacheServer::new(cache)))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        ObjectCacheServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap()
    }

    fn write_messages(key: &str, data_type: &str, data: &[u8]) -> Vec<CacheWriteRequest> {
        let mut messages = vec![CacheWriteRequest {
            payload: Some(cache_write_request::Payload::Header(CacheWriteHeader {
                key: key.to_string(),
                data_type: data_type.to_string(),
            })),
        }];
        for chunk in data.chunks(CHUNK_SIZE) {
            messages.push(CacheWriteRequest {
                payload: Some(cache_write_request::Payload::Data(Bytes::copy_from_slice(
                    chunk,
                ))),
            });
        }
        messages
    }

    async fn get_responses(
        client: &mut ObjectCacheServiceClient<tonic::transport::Channel>,
        key: &str,
        client_version: u64,
    ) -> Vec<CacheGetResponse> {
        let mut stream = client
            .get(CacheGetRequest {
                key: key.to_string(),
                client_version,
            })
            .await
            .unwrap()
            .into_inner();
        let mut responses = Vec::new();
        while let Some(message) = stream.message().await.unwrap() {
            responses.push(message);
        }
        responses
    }

    #[tokio::test]
    async fn grpc_put_get_patch_conditional_and_delete() {
        let mut client = client().await;
        let put = client
            .put(stream::iter(write_messages(
                "app/session",
                "arrow.table",
                b"base",
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(put.version, 1);
        assert_eq!(put.size, 4);
        assert_eq!(put.data_type, "arrow.table");
        assert!(put.creation_time > 0);

        let full = get_responses(&mut client, &put.key, 0).await;
        assert_eq!(full.len(), 2);
        let Some(cache_get_response::Payload::Header(header)) = &full[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.mode, CacheGetMode::Full as i32);
        assert_eq!(header.data_type, "arrow.table");
        let Some(cache_get_response::Payload::Chunk(chunk)) = &full[1].payload else {
            panic!("expected base chunk")
        };
        assert_eq!(chunk.kind, CacheChunkKind::Base as i32);
        assert_eq!(chunk.data, b"base".as_slice());

        let patched = client
            .patch(stream::iter(write_messages(
                &put.key,
                "arrow.table",
                b"patch",
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(patched.version, 2);
        assert_eq!(patched.delta_count, 1);
        let suffix = get_responses(&mut client, &put.key, 1).await;
        assert_eq!(suffix.len(), 2);
        let Some(cache_get_response::Payload::Header(header)) = &suffix[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.mode, CacheGetMode::Patches as i32);
        assert_eq!(header.data_type, "arrow.table");
        let Some(cache_get_response::Payload::Chunk(chunk)) = &suffix[1].payload else {
            panic!("expected patch chunk")
        };
        assert_eq!(chunk.kind, CacheChunkKind::Patch as i32);
        assert_eq!(chunk.version, 2);
        assert_eq!(chunk.data, b"patch".as_slice());

        let full_with_patch = get_responses(&mut client, &put.key, 0).await;
        assert_eq!(full_with_patch.len(), 3);
        let Some(cache_get_response::Payload::Chunk(base)) = &full_with_patch[1].payload else {
            panic!("expected base chunk")
        };
        let Some(cache_get_response::Payload::Chunk(patch)) = &full_with_patch[2].payload else {
            panic!("expected patch chunk")
        };
        assert_eq!(base.kind, CacheChunkKind::Base as i32);
        assert_eq!(patch.kind, CacheChunkKind::Patch as i32);

        let mismatched = client
            .patch(stream::iter(write_messages(
                &put.key,
                "cloudpickle",
                b"bad",
            )))
            .await;
        assert!(mismatched.is_err());

        let mismatched_codec_suffix = client
            .patch(stream::iter(write_messages(
                &put.key,
                "arrow.table.zstd",
                b"bad",
            )))
            .await;
        assert!(mismatched_codec_suffix.is_err());

        let unchanged = get_responses(&mut client, &put.key, 2).await;
        assert_eq!(unchanged.len(), 1);
        let Some(cache_get_response::Payload::Header(header)) = &unchanged[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.mode, CacheGetMode::NotModified as i32);

        let metadata = client
            .get_metadata(CacheGetMetadataRequest {
                key: put.key.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(metadata.version, 2);
        assert_eq!(metadata.data_type, "arrow.table");
        client
            .delete(CacheDeleteRequest {
                key: put.key.clone(),
            })
            .await
            .unwrap();
        assert!(client
            .get(CacheGetRequest {
                key: put.key,
                client_version: 0,
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn grpc_empty_payload_round_trip() {
        let mut client = client().await;
        let put = client
            .put(stream::iter(write_messages("app/session", "raw", b"")))
            .await
            .unwrap()
            .into_inner();
        let responses = get_responses(&mut client, &put.key, 0).await;
        assert_eq!(responses.len(), 2);
        let Some(cache_get_response::Payload::Chunk(chunk)) = &responses[1].payload else {
            panic!("expected empty base chunk")
        };
        assert!(chunk.data.is_empty());
        let Some(cache_get_response::Payload::Header(header)) = &responses[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.data_type, "raw");
    }

    #[tokio::test]
    async fn grpc_preserves_compressed_bytes_with_type_suffix() {
        let mut client = client().await;
        let stored = b"client-zstd-frame";
        let put = client
            .put(stream::iter(write_messages(
                "app/session",
                "cloudpickle.zstd",
                stored,
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(put.data_type, "cloudpickle.zstd");
        let responses = get_responses(&mut client, &put.key, 0).await;
        let Some(cache_get_response::Payload::Header(header)) = &responses[0].payload else {
            panic!("expected response header")
        };
        let Some(cache_get_response::Payload::Chunk(chunk)) = &responses[1].payload else {
            panic!("expected data chunk")
        };
        assert_eq!(header.data_type, "cloudpickle.zstd");
        assert_eq!(chunk.data, stored.as_slice());
    }
}

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

use common::FlameError;
use futures::{stream, Stream, TryStreamExt};
use rpc::flame::v1::object_cache_service_server::ObjectCacheService;
use rpc::flame::v1::{
    cache_write_request, CacheDeleteRequest, CacheDeleteResponse, CacheGetHeader,
    CacheGetMetadataRequest, CacheGetRequest, CacheGetResponse, CacheListRequest,
    CacheObjectMetadata, CacheWriteRequest,
};
use stdng::lock_ptr;
use tonic::{Request, Response, Status, Streaming};

use super::response::{response_stream, ObjectPart};
use super::{Object, ObjectCache, ObjectKey, ObjectMetadata, WriteRequest};

/// Largest data chunk exchanged on the wire, in either direction.
pub(super) const CHUNK_SIZE: usize = 1024 * 1024;

pub(super) type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

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
            creation_time: metadata.creation_time,
            data_type: metadata.data_type,
        }
    }
}

/// Read the write header, and return the request with the remaining data chunks
/// as a stream.
async fn write_request(
    request: Request<Streaming<CacheWriteRequest>>,
) -> Result<WriteRequest, Status> {
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
    let chunks = stream::try_unfold(messages, |mut messages| async move {
        let Some(message) = messages.message().await? else {
            return Ok(None);
        };
        match message.payload {
            Some(cache_write_request::Payload::Data(chunk)) => {
                if chunk.len() > CHUNK_SIZE {
                    return Err(FlameError::InvalidConfig(
                        "write chunk exceeds 1 MiB".to_string(),
                    ));
                }
                Ok(Some((chunk, messages)))
            }
            _ => Err(FlameError::InvalidConfig(
                "write stream contains another header".to_string(),
            )),
        }
    });
    Ok(WriteRequest {
        key,
        data_type: header.data_type,
        chunks: Box::pin(chunks),
    })
}

#[tonic::async_trait]
impl ObjectCacheService for GrpcCacheServer {
    type GetStream = ResponseStream<CacheGetResponse>;
    type ListStream = ResponseStream<CacheObjectMetadata>;

    async fn put(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let request = write_request(request).await?;
        let metadata = self.cache.put_stream(request).await.map_err(Status::from)?;
        Ok(Response::new(metadata.into()))
    }

    async fn patch(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let WriteRequest {
            key,
            data_type,
            chunks,
        } = write_request(request).await?;
        if key.object_id.is_none() {
            return Err(Status::invalid_argument(
                "patch requires a complete object key",
            ));
        }
        // The data of a patch is small by construction, so it is buffered in memory.
        let data = chunks
            .try_fold(Vec::new(), |mut data, chunk| async move {
                data.extend_from_slice(&chunk);
                Ok(data)
            })
            .await
            .map_err(Status::from)?;
        let metadata = self
            .cache
            .patch(
                &key,
                Object::new().append_data(data).with_data_type(data_type),
            )
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
        let guard = key_lock.read_owned().await;

        let up_to_date = |version| CacheGetHeader {
            version,
            data_type: String::new(),
        };
        let metadata = self
            .cache
            .metadata_of(&request.key)
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("object <{}> not found", request.key)))?;
        self.cache.storage.touch(&key);
        if request.version != 0 && metadata.version == request.version {
            return Ok(Response::new(response_stream(
                Arc::clone(&self.cache),
                key,
                up_to_date(request.version),
                Vec::new(),
                None,
            )));
        }

        // The parts are described from storage metadata; payload bytes are
        // read chunk by chunk while the response is sent.
        let not_found = || Status::not_found(format!("object <{}> not found", request.key));
        let stat = self
            .cache
            .storage
            .stat(&key)
            .await
            .map_err(Status::from)?
            .ok_or_else(not_found)?;
        let server_version = stat.version();

        // A client that has a version of this object needs only the data after
        // it, and one that has all of it (version 0) gets it from version 1. A
        // version the object does not have means the client has another object
        // of the same key, which it must discard.
        if request.version > server_version {
            return Err(Status::from(FlameError::InvalidVersion(format!(
                "version {} is ahead of version {} of object <{}>",
                request.version, server_version, request.key
            ))));
        }
        let first = request.version as usize;
        let parts: Vec<_> = (first..stat.records.len())
            .map(|index| ObjectPart {
                index,
                len: stat.records[index].len,
            })
            .collect();
        let header = CacheGetHeader {
            version: server_version,
            data_type: stat.data_type,
        };
        // Every part is read from storage, so keep writers out until the
        // response ends.
        let guard = (!parts.is_empty()).then_some(guard);
        Ok(Response::new(response_stream(
            Arc::clone(&self.cache),
            key,
            header,
            parts,
            guard,
        )))
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
    use bytes::Bytes;
    use rpc::flame::v1::object_cache_service_client::ObjectCacheServiceClient;
    use rpc::flame::v1::object_cache_service_server::ObjectCacheServiceServer;
    use rpc::flame::v1::{cache_get_response, cache_write_request, CacheWriteHeader};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    async fn client() -> ObjectCacheServiceClient<tonic::transport::Channel> {
        client_with_storage("none").await.0
    }

    /// Bytes the cache holds in memory on top of its storage engine.
    fn resident(cache: &ObjectCache) -> (usize, u64) {
        cache.storage.resident()
    }

    async fn client_with_storage(
        storage: &str,
    ) -> (
        ObjectCacheServiceClient<tonic::transport::Channel>,
        Arc<ObjectCache>,
    ) {
        let cache = Arc::new(
            ObjectCache::new(
                CacheEndpoint {
                    scheme: "grpc".to_string(),
                    host: "127.0.0.1".to_string(),
                    port: 9090,
                },
                crate::storage::connect(storage, None).await.unwrap(),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = GrpcCacheServer::new(Arc::clone(&cache));
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ObjectCacheServiceServer::new(server))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let client = ObjectCacheServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        (client, cache)
    }

    fn chunk_data(response: &CacheGetResponse) -> (u64, &Bytes) {
        let Some(cache_get_response::Payload::Chunk(chunk)) = &response.payload else {
            panic!("expected chunk")
        };
        (chunk.version, &chunk.data)
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
        version: u64,
    ) -> Vec<CacheGetResponse> {
        let mut stream = client
            .get(CacheGetRequest {
                key: key.to_string(),
                version,
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
    async fn grpc_put_get_append_versions_and_delete() {
        let mut client = client().await;
        let put = client
            .put(stream::iter(write_messages(
                "app/session",
                "arrow.table",
                b"first",
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(put.version, 1);
        assert_eq!(put.size, 5);
        assert_eq!(put.data_type, "arrow.table");
        assert!(put.creation_time > 0);

        let all = get_responses(&mut client, &put.key, 0).await;
        assert_eq!(all.len(), 2);
        let Some(cache_get_response::Payload::Header(header)) = &all[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.data_type, "arrow.table");
        let Some(cache_get_response::Payload::Chunk(chunk)) = &all[1].payload else {
            panic!("expected first chunk")
        };
        assert_eq!(chunk.version, 1);
        assert_eq!(chunk.data, b"first".as_slice());

        let appended = client
            .patch(stream::iter(write_messages(
                &put.key,
                "arrow.table",
                b"second",
            )))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(appended.version, 2);
        let suffix = get_responses(&mut client, &put.key, 1).await;
        assert_eq!(suffix.len(), 2);
        let Some(cache_get_response::Payload::Header(header)) = &suffix[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.data_type, "arrow.table");
        let Some(cache_get_response::Payload::Chunk(chunk)) = &suffix[1].payload else {
            panic!("expected second chunk")
        };
        assert_eq!(chunk.version, 2);
        assert_eq!(chunk.data, b"second".as_slice());

        let full_with_second = get_responses(&mut client, &put.key, 0).await;
        assert_eq!(full_with_second.len(), 3);
        let Some(cache_get_response::Payload::Chunk(first)) = &full_with_second[1].payload else {
            panic!("expected first chunk")
        };
        let Some(cache_get_response::Payload::Chunk(second)) = &full_with_second[2].payload else {
            panic!("expected second chunk")
        };
        assert_eq!((first.version, second.version), (1, 2));

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

        // A version the object does not have is an error, not another object.
        let ahead = client
            .get(CacheGetRequest {
                key: put.key.clone(),
                version: 3,
            })
            .await;
        assert_eq!(ahead.unwrap_err().code(), tonic::Code::InvalidArgument);

        let unchanged = get_responses(&mut client, &put.key, 2).await;
        assert_eq!(unchanged.len(), 1);
        let Some(cache_get_response::Payload::Header(header)) = &unchanged[0].payload else {
            panic!("expected response header")
        };
        // Nothing after the client's version: the header alone.
        assert_eq!(header.version, 2);

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
                version: 0,
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
            panic!("expected empty first chunk")
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

    #[tokio::test]
    async fn grpc_streams_large_object_from_storage_through_open_files() {
        let directory = tempfile::tempdir().unwrap();
        let (mut client, cache) = client_with_storage(directory.path().to_str().unwrap()).await;

        // Many chunks and not a multiple of the chunk size.
        let data: Vec<u8> = (0..8 * CHUNK_SIZE + CHUNK_SIZE / 2)
            .map(|index| (index % 251) as u8)
            .collect();
        let put = client
            .put(stream::iter(write_messages("app/session", "raw", &data)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(put.size, data.len() as u64);
        // Nothing is read yet, so nothing is resident.
        assert_eq!(resident(&cache).0, 0);

        let all = get_responses(&mut client, &put.key, 0).await;
        let Some(cache_get_response::Payload::Header(header)) = &all[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.version, 1);
        let mut received = Vec::new();
        for response in &all[1..] {
            let (version, chunk) = chunk_data(response);
            assert_eq!(version, 1);
            assert!(chunk.len() <= CHUNK_SIZE);
            received.extend_from_slice(chunk);
        }
        assert_eq!(all.len(), 1 + data.len().div_ceil(CHUNK_SIZE));
        assert!(received == data);
        // Only the open files of the object are resident, no payload bytes.
        let (count, bytes) = resident(&cache);
        assert_eq!(count, 1);
        assert!(bytes < 1024);

        let appended = client
            .patch(stream::iter(write_messages(&put.key, "raw", b"second")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(appended.version, 2);
        assert_eq!(appended.size, data.len() as u64 + 6);

        let suffix = get_responses(&mut client, &put.key, 1).await;
        assert_eq!(suffix.len(), 2);
        let Some(cache_get_response::Payload::Header(header)) = &suffix[0].payload else {
            panic!("expected response header")
        };
        assert_eq!(header.version, 2);
        let (version, chunk) = chunk_data(&suffix[1]);
        assert_eq!(version, 2);
        assert_eq!(chunk.as_ref(), b"second");

        let all = get_responses(&mut client, &put.key, 0).await;
        let (version, chunk) = chunk_data(all.last().unwrap());
        assert_eq!(version, 2);
        assert_eq!(chunk.as_ref(), b"second");
        assert_eq!(all.len(), 2 + data.len().div_ceil(CHUNK_SIZE));

        let unchanged = get_responses(&mut client, &put.key, 2).await;
        assert_eq!(unchanged.len(), 1);
        assert!(resident(&cache).1 < 1024);
    }

    #[tokio::test]
    async fn grpc_streams_large_object_under_none_from_the_only_copy() {
        let (mut client, cache) = client_with_storage("none").await;

        let data: Vec<u8> = (0..8 * CHUNK_SIZE + CHUNK_SIZE / 2)
            .map(|index| (index % 251) as u8)
            .collect();
        let put = client
            .put(stream::iter(write_messages("app/session", "raw", &data)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(put.size, data.len() as u64);

        let all = get_responses(&mut client, &put.key, 0).await;
        let mut received = Vec::new();
        for response in &all[1..] {
            let (version, chunk) = chunk_data(response);
            assert_eq!(version, 1);
            assert!(chunk.len() <= CHUNK_SIZE);
            received.extend_from_slice(chunk);
        }
        assert_eq!(all.len(), 1 + data.len().div_ceil(CHUNK_SIZE));
        assert!(received == data);

        // The resident copy is the only one, whatever the object's size.
        let small = client
            .put(stream::iter(write_messages("app/session", "raw", b"small")))
            .await
            .unwrap()
            .into_inner();
        let responses = get_responses(&mut client, &small.key, 0).await;
        assert_eq!(first_bytes(&responses).1, b"small");
        assert_eq!(resident(&cache), (2, data.len() as u64 + 5));
    }

    /// The payload of a get response of version 0: its version and the first data.
    fn first_bytes(responses: &[CacheGetResponse]) -> (u64, Vec<u8>) {
        let Some(cache_get_response::Payload::Header(header)) = &responses[0].payload else {
            panic!("expected response header")
        };
        let mut data = Vec::new();
        for response in &responses[1..] {
            let (version, chunk) = chunk_data(response);
            if version == 1 {
                data.extend_from_slice(chunk);
            }
        }
        (header.version, data)
    }
}

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

use super::{Controller, ObjectFilter, ObjectKey, ObjectMetadata, WriteRequest};
use crate::storage::ChunkStream;
use common::FlameError;
use futures::{stream, Stream, StreamExt};
use rpc::flame::v1::object_cache_service_server::ObjectCacheService;
use rpc::flame::v1::{
    cache_get_response, cache_write_request, CacheGetChunk, CacheGetHeader, CacheGetMode,
    CacheGetRequest, CacheGetResponse, CacheObjectMetadata, CacheWriteRequest,
};
use std::pin::Pin;
use std::sync::Arc;
use tonic::{Request, Response, Status, Streaming};

pub(super) const CHUNK_SIZE: usize = crate::storage::CHUNK_SIZE;
pub(crate) type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
pub struct GrpcCacheServer {
    controller: Arc<Controller>,
}
impl GrpcCacheServer {
    /// Expose the shared controller through the cache gRPC service.
    pub fn new(controller: Arc<Controller>) -> Self {
        Self { controller }
    }

    /// Query exact object metadata, optionally selecting a committed prefix.
    async fn lookup_metadata(
        &self,
        key: &ObjectKey,
        version: Option<u64>,
    ) -> Result<ObjectMetadata, Status> {
        self.controller
            .list(&ObjectFilter::ByKey {
                key: key.clone(),
                version,
            })
            .await
            .map_err(Status::from)?
            .into_iter()
            .next()
            .ok_or_else(|| Status::not_found("object not found"))
    }

    /// Translate reader identity changes at the transport boundary.
    fn read_error(error: FlameError) -> Status {
        match error {
            FlameError::InvalidVersion(message) => Status::failed_precondition(message),
            other => Status::from(other),
        }
    }

    /// Reject a changed UUID before checking that the requested part count is available.
    fn validate_cursor(request: &CacheGetRequest, metadata: &ObjectMetadata) -> Result<(), Status> {
        if !request.uuid.is_empty() && request.uuid != metadata.uuid {
            return Err(Status::failed_precondition("object UUID changed"));
        }
        if request.version > metadata.version {
            return Err(Status::invalid_argument(
                "requested version is ahead of object",
            ));
        }
        Ok(())
    }

    /// Return metadata directly without opening or admitting a payload reader.
    async fn get_metadata(
        &self,
        request: CacheGetRequest,
    ) -> Result<Response<ResponseStream<CacheGetResponse>>, Status> {
        let key = ObjectKey::try_from(request.key.as_str()).map_err(Status::from)?;
        let metadata = self.lookup_metadata(&key, None).await?;
        Ok(Response::new(Box::pin(stream::iter([Ok(
            self.header(metadata)
        )]))))
    }

    /// Select full-object or prefix metadata, then open its immutable parts from zero.
    async fn get_object(
        &self,
        request: CacheGetRequest,
    ) -> Result<Response<ResponseStream<CacheGetResponse>>, Status> {
        let key = ObjectKey::try_from(request.key.as_str()).map_err(Status::from)?;
        let latest = self.lookup_metadata(&key, None).await?;
        Self::validate_cursor(&request, &latest)?;
        let mut metadata = if request.version == 0 || request.version == latest.version {
            latest
        } else {
            let prefix = self.lookup_metadata(&key, Some(request.version)).await?;
            Self::validate_cursor(&request, &prefix)?;
            prefix
        };
        let chunks = self
            .controller
            .storage
            .read_object(&mut metadata)
            .await
            .map_err(Self::read_error)?;
        Ok(self.get_response(metadata, chunks))
    }

    /// Select current metadata and open the suffix after the UUID-aware client cursor.
    async fn get_patches(
        &self,
        request: CacheGetRequest,
    ) -> Result<Response<ResponseStream<CacheGetResponse>>, Status> {
        let key = ObjectKey::try_from(request.key.as_str()).map_err(Status::from)?;
        let metadata = self.lookup_metadata(&key, None).await?;
        Self::validate_cursor(&request, &metadata)?;
        if request.version > 0 && request.uuid.is_empty() {
            return Err(Status::invalid_argument(
                "PATCHES cursor requires the object UUID",
            ));
        }
        let mut metadata = metadata.with_version(request.version);
        let chunks = self
            .controller
            .storage
            .read_patch(&mut metadata)
            .await
            .map_err(Self::read_error)?;
        Ok(self.get_response(metadata, chunks))
    }

    /// Attach this replica's advertised endpoint to a public metadata header.
    fn header(&self, metadata: ObjectMetadata) -> CacheGetResponse {
        CacheGetResponse {
            payload: Some(cache_get_response::Payload::Header(CacheGetHeader {
                metadata: Some(
                    metadata
                        .with_endpoint(self.controller.endpoint.to_uri())
                        .into(),
                ),
            })),
        }
    }

    /// Prepend the selected snapshot metadata and map domain chunks directly to RPC frames.
    fn get_response(
        &self,
        metadata: ObjectMetadata,
        chunks: ChunkStream,
    ) -> Response<ResponseStream<CacheGetResponse>> {
        let header = stream::iter([Ok(self.header(metadata))]);
        let body = chunks.map(|result| {
            result
                .map(|chunk| CacheGetResponse {
                    payload: Some(cache_get_response::Payload::Chunk(CacheGetChunk {
                        part: chunk.part,
                        data: chunk.data,
                    })),
                })
                .map_err(Status::from)
        });
        Response::new(Box::pin(header.chain(body)))
    }
}
impl From<ObjectMetadata> for CacheObjectMetadata {
    /// Translate domain metadata into its gRPC representation.
    fn from(metadata: ObjectMetadata) -> Self {
        Self {
            uuid: metadata.uuid,
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
        size: header.size,
        chunks: Box::pin(chunks),
    })
}

#[tonic::async_trait]
impl ObjectCacheService for GrpcCacheServer {
    type GetStream = ResponseStream<CacheGetResponse>;
    /// Parse an upload and forward its bytes to the storage-owned write operation.
    async fn put(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let request = write_request(request).await?;
        Ok(Response::new(
            self.controller
                .put_stream(request)
                .await
                .map_err(Status::from)?
                .into(),
        ))
    }
    /// Forward patch bytes to storage, which owns commit and cleanup despite RPC cancellation.
    async fn patch(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let request = write_request(request).await?;
        if request.key.object_id.is_none() {
            return Err(Status::invalid_argument(
                "patch requires a complete object key",
            ));
        }
        Ok(Response::new(
            self.controller
                .patch_stream(request)
                .await
                .map_err(Status::from)?
                .into(),
        ))
    }
    /// Dispatch metadata, object-prefix, and patch-suffix reads to their mode-specific handlers.
    async fn get(
        &self,
        request: Request<CacheGetRequest>,
    ) -> Result<Response<Self::GetStream>, Status> {
        let request = request.into_inner();
        match CacheGetMode::try_from(request.mode)
            .map_err(|_| Status::invalid_argument("unknown GET mode"))?
        {
            CacheGetMode::Metadata => self.get_metadata(request).await,
            CacheGetMode::Object => self.get_object(request).await,
            CacheGetMode::Patches => self.get_patches(request).await,
        }
    }
}

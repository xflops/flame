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
use common::apis::{valid_subject_id, UserIdentity};
use common::security::SecurityManager;
use futures::{stream, Stream};
use rpc::flame::v1::object_cache_service_server::ObjectCacheService;
use rpc::flame::v1::{
    cache_get_response, cache_write_request, CacheChunkKind, CacheDelegateRequest,
    CacheDelegateResponse, CacheDeleteRequest, CacheDeleteResponse, CacheGetChunk, CacheGetHeader,
    CacheGetMetadataRequest, CacheGetMode, CacheGetRequest, CacheGetResponse, CacheListRequest,
    CacheObjectMetadata, CacheWriteRequest,
};
use stdng::lock_ptr;
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::{Request, Response, Status, Streaming};

use super::{Object, ObjectCache, ObjectKey, ObjectMetadata};

const CHUNK_SIZE: usize = 1024 * 1024;
const DELEGATION_TOKEN_HEADER: &str = "x-flame-delegation-token";
type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

pub struct GrpcCacheServer {
    cache: Arc<ObjectCache>,
    security: Option<Arc<dyn SecurityManager>>,
}

impl GrpcCacheServer {
    pub fn new(cache: Arc<ObjectCache>) -> Self {
        Self {
            cache,
            security: None,
        }
    }

    pub fn secured(cache: Arc<ObjectCache>, security: Arc<dyn SecurityManager>) -> Self {
        Self {
            cache,
            security: Some(security),
        }
    }

    fn auth<T>(&self, request: &Request<T>) -> Result<CacheAuth, Status> {
        let Some(security) = self.security.as_ref() else {
            return Ok(CacheAuth {
                identity: None,
                certificate_identity: None,
                secured: false,
            });
        };
        let peer = request.extensions().get::<TlsConnectInfo<TcpConnectInfo>>();
        let token = request
            .metadata()
            .get(DELEGATION_TOKEN_HEADER)
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| Status::unauthenticated("invalid cache user token"))
            })
            .transpose()?;
        let certificate_identity = security.identify(peer, None)?;
        let identity = security.identify(peer, token.as_deref())?;
        Ok(CacheAuth {
            identity,
            certificate_identity,
            secured: true,
        })
    }

    fn signed_metadata(&self, metadata: ObjectMetadata) -> Result<CacheObjectMetadata, Status> {
        let mut response: CacheObjectMetadata = metadata.into();
        if let Some(security) = &self.security {
            response.signature = security.sign(&format!("object:{}", response.key))?;
        }
        Ok(response)
    }

    fn verify_object_key(&self, key: &str, signature: &str) -> Result<(), Status> {
        if let Some(security) = &self.security {
            if !security.verify(signature, &format!("object:{key}")) {
                return Err(Status::unauthenticated("invalid object key signature"));
            }
        }
        Ok(())
    }
}

struct CacheAuth {
    identity: Option<UserIdentity>,
    certificate_identity: Option<UserIdentity>,
    secured: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheOperation {
    Read,
    Write,
    Delete,
}

impl CacheAuth {
    fn can_delegate(&self) -> bool {
        !self.secured || matches!(self.certificate_identity, Some(UserIdentity::TenantUser(_)))
    }

    fn allow_key(&self, key: &ObjectKey, operation: CacheOperation) -> Result<(), Status> {
        if !self.secured {
            return Ok(());
        }
        if matches!(self.identity, Some(UserIdentity::SystemCache(_))) {
            return Ok(());
        }
        if matches!(self.identity, Some(UserIdentity::SystemNode(_)))
            && operation == CacheOperation::Read
            && key.session_id == "pkg"
        {
            return Ok(());
        }
        // A verified tenant certificate or delegated user token grants data
        // access across applications. Package reads and bootstrap stay system-only.
        let tenant_may_access = !key.is_all_sessions()
            && key.session_id != "bootstrap"
            && (key.session_id != "pkg" || operation != CacheOperation::Read);
        if tenant_may_access && matches!(self.identity, Some(UserIdentity::TenantUser(_))) {
            return Ok(());
        }
        Err(Status::unauthenticated(
            "authorized client certificate or delegation token required",
        ))
    }

    fn allow_list(&self) -> Result<(), Status> {
        if !self.secured || matches!(self.identity, Some(UserIdentity::SystemCache(_))) {
            Ok(())
        } else {
            Err(Status::unauthenticated("system cache certificate required"))
        }
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
            signature: String::new(),
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
    auth: &CacheAuth,
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
    if key.is_all_sessions() {
        return Err(Status::invalid_argument("write requires a session key"));
    }
    auth.allow_key(&key, CacheOperation::Write)?;
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

    async fn delegate(
        &self,
        request: Request<CacheDelegateRequest>,
    ) -> Result<Response<CacheDelegateResponse>, Status> {
        let auth = self.auth(&request)?;
        if !auth.can_delegate() {
            return Err(Status::unauthenticated(
                "verified tenant certificate required",
            ));
        }
        let key = request.into_inner().key;
        if !valid_subject_id(&key) {
            return Err(Status::invalid_argument(
                "invalid application delegation key",
            ));
        }
        let token = match self.security.as_ref() {
            Some(security) => security.delegate(auth.certificate_identity.as_ref().unwrap())?,
            None => String::new(),
        };
        Ok(Response::new(CacheDelegateResponse { token }))
    }

    async fn put(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let auth = self.auth(&request)?;
        let (key, data_type, data) = collect_write(request, &auth).await?;
        let object = Object::new_typed(0, data, data_type);
        let metadata = self.cache.put(key, object).await.map_err(Status::from)?;
        Ok(Response::new(self.signed_metadata(metadata)?))
    }

    async fn patch(
        &self,
        request: Request<Streaming<CacheWriteRequest>>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let auth = self.auth(&request)?;
        let (key, data_type, data) = collect_write(request, &auth).await?;
        if key.object_id.is_none() {
            return Err(Status::invalid_argument("patch requires a full object key"));
        }
        let metadata = self
            .cache
            .patch(&key, Object::new_typed(0, data, data_type))
            .await
            .map_err(Status::from)?;
        Ok(Response::new(self.signed_metadata(metadata)?))
    }

    async fn get(
        &self,
        request: Request<CacheGetRequest>,
    ) -> Result<Response<Self::GetStream>, Status> {
        let auth = self.auth(&request)?;
        let request = request.into_inner();
        let key = ObjectKey::try_from(request.key.as_str()).map_err(Status::from)?;
        auth.allow_key(&key, CacheOperation::Read)?;
        self.verify_object_key(&request.key, &request.signature)?;
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
        let auth = self.auth(&request)?;
        let key = ObjectKey::from_path(&request.into_inner().key).map_err(Status::from)?;
        auth.allow_key(&key, CacheOperation::Delete)?;
        self.cache.delete(&key).await.map_err(Status::from)?;
        Ok(Response::new(CacheDeleteResponse {}))
    }

    async fn get_metadata(
        &self,
        request: Request<CacheGetMetadataRequest>,
    ) -> Result<Response<CacheObjectMetadata>, Status> {
        let auth = self.auth(&request)?;
        let key = request.into_inner().key;
        let parsed = ObjectKey::try_from(key.as_str()).map_err(Status::from)?;
        auth.allow_key(&parsed, CacheOperation::Read)?;
        let metadata =
            lock_ptr!(self.cache.metadata).map_err(|error| Status::internal(error.to_string()))?;
        let metadata = metadata
            .get(&key)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("object <{}> not found", key)))?;
        Ok(Response::new(self.signed_metadata(metadata)?))
    }

    async fn list(
        &self,
        request: Request<CacheListRequest>,
    ) -> Result<Response<Self::ListStream>, Status> {
        self.auth(&request)?.allow_list()?;
        let metadata = self.cache.list_all().await.map_err(Status::from)?;
        let items = metadata
            .into_iter()
            .map(|item| self.signed_metadata(item))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(Ok);
        Ok(Response::new(Box::pin(stream::iter(items))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheEndpoint, CACHE_DELEGATION_DOMAIN};
    use common::ctx::FlameTls;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509Name, X509};
    use rpc::flame::v1::object_cache_service_client::ObjectCacheServiceClient;
    use rpc::flame::v1::object_cache_service_server::ObjectCacheServiceServer;
    use rpc::flame::v1::{cache_get_response, cache_write_request, CacheWriteHeader};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    fn test_security() -> (tempfile::TempDir, Arc<dyn SecurityManager>) {
        let dir = tempfile::tempdir().unwrap();
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509Name::builder().unwrap();
        name.append_entry_by_text("CN", "cache-test").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(
            BigNum::from_u32(1)
                .unwrap()
                .to_asn1_integer()
                .unwrap()
                .as_ref(),
        )
        .unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
            .unwrap();
        cert.set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert_file = dir.path().join("cache.crt");
        let key_file = dir.path().join("cache.key");
        std::fs::write(&cert_file, cert.build().to_pem().unwrap()).unwrap();
        std::fs::write(&key_file, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        let config = common::ctx::FlameSecurity {
            tls: FlameTls {
                cert_file: cert_file.to_string_lossy().into_owned(),
                key_file: key_file.to_string_lossy().into_owned(),
                ca_file: None,
            },
            trust_domain: "flame.local".to_string(),
        };
        let security = common::security::new(Some(&config))
            .with_delegation(CACHE_DELEGATION_DOMAIN)
            .unwrap();
        (dir, Arc::from(security))
    }

    #[test]
    fn user_token_can_access_multiple_applications_with_namespace_restrictions() {
        let (_dir, _security) = test_security();
        let auth = CacheAuth {
            identity: Some(UserIdentity::TenantUser("alice".to_string())),
            certificate_identity: None,
            secured: true,
        };
        let session = ObjectKey::try_from("app/session/object").unwrap();
        let foreign = ObjectKey::try_from("other/session/object").unwrap();
        let package = ObjectKey::try_from("app/pkg/object").unwrap();
        let bootstrap = ObjectKey::try_from("app/bootstrap/object").unwrap();
        let wildcard = ObjectKey::from_path("app/*").unwrap();

        assert!(auth.allow_key(&session, CacheOperation::Read).is_ok());
        assert!(auth.allow_key(&session, CacheOperation::Write).is_ok());
        assert!(auth.allow_key(&foreign, CacheOperation::Write).is_ok());
        assert!(auth.allow_key(&package, CacheOperation::Read).is_err());
        assert!(auth.allow_key(&package, CacheOperation::Write).is_ok());
        assert!(auth.allow_key(&package, CacheOperation::Delete).is_ok());
        assert!(auth.allow_key(&bootstrap, CacheOperation::Read).is_err());
        assert!(auth.allow_key(&bootstrap, CacheOperation::Write).is_err());
        assert!(auth.allow_key(&wildcard, CacheOperation::Delete).is_err());
    }

    #[test]
    fn user_token_identifies_signing_user() {
        let (_dir, security) = test_security();
        let user = UserIdentity::TenantUser("alice".to_string());
        let token = security.delegate(&user).unwrap();
        assert_eq!(security.identify(None, Some(&token)).unwrap(), Some(user));
        assert!(security.identify(None, Some(&format!("{token}x"))).is_err());
    }

    #[test]
    fn secure_cache_accepts_tenant_certificate_or_token() {
        let (_dir, _security) = test_security();
        let session = ObjectKey::try_from("app/session/object").unwrap();
        let package = ObjectKey::try_from("app/pkg/object").unwrap();

        let tenant = CacheAuth {
            identity: Some(UserIdentity::TenantUser("alice".to_string())),
            certificate_identity: Some(UserIdentity::TenantUser("alice".to_string())),
            secured: true,
        };
        assert!(tenant.can_delegate());
        assert!(tenant.allow_key(&session, CacheOperation::Read).is_ok());
        assert!(tenant.allow_key(&package, CacheOperation::Read).is_err());
        assert!(tenant.allow_key(&package, CacheOperation::Write).is_ok());
        assert!(tenant.allow_list().is_err());

        let system_node = CacheAuth {
            identity: Some(UserIdentity::SystemNode("executor".to_string())),
            certificate_identity: Some(UserIdentity::SystemNode("executor".to_string())),
            secured: true,
        };
        assert!(!system_node.can_delegate());
        assert!(system_node
            .allow_key(&package, CacheOperation::Read)
            .is_ok());
        assert!(system_node
            .allow_key(&package, CacheOperation::Write)
            .is_err());
        assert!(system_node
            .allow_key(&package, CacheOperation::Delete)
            .is_err());
        assert!(system_node
            .allow_key(&session, CacheOperation::Read)
            .is_err());
        assert!(system_node.allow_list().is_err());

        let system_cache = CacheAuth {
            identity: Some(UserIdentity::SystemCache("cache".to_string())),
            certificate_identity: Some(UserIdentity::SystemCache("cache".to_string())),
            secured: true,
        };
        assert!(!system_cache.can_delegate());
        assert!(system_cache
            .allow_key(&session, CacheOperation::Read)
            .is_ok());
        assert!(system_cache
            .allow_key(&package, CacheOperation::Read)
            .is_ok());
        assert!(system_cache.allow_list().is_ok());
    }

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

    async fn token_client() -> (ObjectCacheServiceClient<tonic::transport::Channel>, String) {
        let (_dir, security) = test_security();
        let token = security
            .delegate(&UserIdentity::TenantUser("alice".to_string()))
            .unwrap();
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
                .add_service(ObjectCacheServiceServer::new(GrpcCacheServer::secured(
                    cache, security,
                )))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let client = ObjectCacheServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        (client, token)
    }

    #[tokio::test]
    async fn grpc_user_token_checks_every_data_request() {
        let (mut client, user_token) = token_client().await;
        let token = |message: Vec<CacheWriteRequest>| {
            let mut request = Request::new(stream::iter(message));
            request
                .metadata_mut()
                .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
            request
        };
        let put = client
            .put(token(write_messages("app/session", "raw", b"payload")))
            .await
            .unwrap()
            .into_inner();
        assert!(client
            .get(CacheGetRequest {
                key: put.key.clone(),
                client_version: 0,
                signature: put.signature.clone(),
            })
            .await
            .is_err());
        let mut get = Request::new(CacheGetRequest {
            key: put.key.clone(),
            client_version: 0,
            signature: put.signature.clone(),
        });
        get.metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert!(client.get(get).await.is_ok());
        let mut wrong_signature = Request::new(CacheGetRequest {
            key: put.key.clone(),
            client_version: 0,
            signature: "invalid".to_string(),
        });
        wrong_signature
            .metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert_eq!(
            client.get(wrong_signature).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        let mut missing_signature = Request::new(CacheGetRequest {
            key: put.key.clone(),
            client_version: 0,
            signature: String::new(),
        });
        missing_signature
            .metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert_eq!(
            client.get(missing_signature).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        let other = client
            .put(token(write_messages("other/session", "raw", b"cross-app")))
            .await
            .unwrap()
            .into_inner();
        let mut swapped_signature = Request::new(CacheGetRequest {
            key: put.key.clone(),
            client_version: 0,
            signature: other.signature,
        });
        swapped_signature
            .metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert_eq!(
            client.get(swapped_signature).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        let package = client
            .put(token(write_messages("app/pkg", "raw", b"package")))
            .await
            .unwrap()
            .into_inner();
        let mut package_get = Request::new(CacheGetRequest {
            key: package.key,
            client_version: 0,
            signature: package.signature,
        });
        package_get
            .metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert!(client.get(package_get).await.is_err());

        let mut wildcard_delete = Request::new(CacheDeleteRequest {
            key: "app/*".to_string(),
        });
        wildcard_delete
            .metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert!(client.delete(wildcard_delete).await.is_err());

        let mut retained_get = Request::new(CacheGetRequest {
            key: put.key,
            client_version: 0,
            signature: put.signature,
        });
        retained_get
            .metadata_mut()
            .insert(DELEGATION_TOKEN_HEADER, user_token.parse().unwrap());
        assert!(client.get(retained_get).await.is_ok());
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
                signature: String::new(),
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
                signature: String::new(),
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

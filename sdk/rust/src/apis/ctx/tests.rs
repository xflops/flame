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
use crate::apis::flame::v1 as rpc;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;
use tonic::codegen::{http, BoxFuture, Service};

/// Test-only certificates are generated once and never depend on local credentials.
struct TestIdentity {
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
    other_key: Vec<u8>,
}

/// Generate a private CA and a localhost identity valid for server and client authentication.
fn identity() -> &'static TestIdentity {
    static IDENTITY: OnceLock<TestIdentity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let result = Command::new("openssl")
                .current_dir(dir.path())
                .args(args)
                .output()
                .expect("OpenSSL is required for TLS test fixtures");
            assert!(result.status.success(), "test certificate generation failed");
        };
        run(&[
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj",
            "/CN=Flame SDK Test CA", "-addext", "basicConstraints=critical,CA:TRUE",
            "-keyout", "ca.key", "-out", "ca.crt",
        ]);
        run(&[
            "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout",
            "identity.key", "-out", "identity.csr",
        ]);
        fs::write(
            dir.path().join("identity.ext"),
            "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth,clientAuth\n",
        )
        .unwrap();
        run(&[
            "x509", "-req", "-in", "identity.csr", "-CA", "ca.crt", "-CAkey", "ca.key",
            "-CAcreateserial", "-days", "1", "-extfile", "identity.ext", "-out",
            "identity.crt",
        ]);
        TestIdentity {
            ca: fs::read(dir.path().join("ca.crt")).unwrap(),
            cert: fs::read(dir.path().join("identity.crt")).unwrap(),
            key: fs::read(dir.path().join("identity.key")).unwrap(),
            other_key: fs::read(dir.path().join("ca.key")).unwrap(),
        }
    })
}

/// Build complete in-memory credentials, without retaining fixture certificate files.
fn inline_identity() -> FlameClientTls {
    let identity = identity();
    FlameClientTls {
        ca_data: Some(STANDARD.encode(&identity.ca)),
        cert_data: Some(STANDARD.encode(&identity.cert)),
        key_data: Some(STANDARD.encode(&identity.key)),
        ..Default::default()
    }
}

/// Write legacy file credentials only when exercising the supported file-based path.
fn identity_files() -> (tempfile::TempDir, FlameClientTls) {
    let dir = tempfile::tempdir().unwrap();
    let identity = identity();
    for (name, contents) in [
        ("ca.crt", &identity.ca),
        ("client.crt", &identity.cert),
        ("client.key", &identity.key),
    ] {
        fs::write(dir.path().join(name), contents).unwrap();
    }
    let tls = FlameClientTls {
        ca_file: Some(dir.path().join("ca.crt").to_string_lossy().into_owned()),
        cert_file: Some(dir.path().join("client.crt").to_string_lossy().into_owned()),
        key_file: Some(dir.path().join("client.key").to_string_lossy().into_owned()),
        ..Default::default()
    };
    (dir, tls)
}

/// Assert validation fails without displaying either raw or encoded private-key material.
fn invalid_without_secrets(tls: FlameClientTls, expected: &str) {
    let error = tls.client_tls_config("localhost").unwrap_err().to_string();
    assert!(error.contains(expected), "unexpected validation category");
    assert!(!error.contains(&STANDARD.encode(&identity().key)));
    assert!(!error.contains(std::str::from_utf8(&identity().key).unwrap()));
}

#[test]
fn a_missing_file_is_an_error_that_says_how_to_configure() {
    let error = FlameContext::from_file(Some("/nonexistent/flame.yaml".to_string())).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("no Flame cluster configured"));
    assert!(message.contains("/nonexistent/flame.yaml"));
    assert!(message.contains("endpoint: \"http://<head>:8080\""));
    assert!(!message.contains("FLAME_ENDPOINT"));
}

#[test]
fn invalid_yaml_names_its_location_without_echoing_secret_values() {
    let dir = tempfile::tempdir().unwrap();
    for source in [
        "current-context: [".to_string(),
        format!(
            "current-context: test\ncontexts:\n  - name: test\n    cluster:\n      endpoint: https://localhost\n      tls: {}\n",
            STANDARD.encode(&identity().key)
        ),
    ] {
        let path = dir.path().join("flame.yaml");
        fs::write(&path, source).unwrap();
        let error = FlameContext::from_file(Some(path.to_string_lossy().into_owned()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("is not a valid Flame configuration"));
        assert!(error.contains("line"));
        assert!(error.contains(path.to_str().unwrap()));
        assert!(!error.contains(&STANDARD.encode(&identity().key)));
    }
}

#[test]
fn client_identity_requires_a_certificate_and_private_key_from_any_source() {
    for tls in [
        FlameClientTls {
            cert_file: Some("client.crt".into()),
            ..Default::default()
        },
        FlameClientTls {
            key_file: Some("client.key".into()),
            ..Default::default()
        },
        FlameClientTls {
            cert_data: inline_identity().cert_data,
            ..Default::default()
        },
        FlameClientTls {
            key_data: inline_identity().key_data,
            ..Default::default()
        },
    ] {
        invalid_without_secrets(tls, "requires both");
    }
}

#[test]
fn identity_supports_files_inline_and_mixed_sources() {
    let (_dir, file_tls) = identity_files();
    file_tls.client_tls_config("localhost").unwrap();
    inline_identity().client_tls_config("localhost").unwrap();
    FlameClientTls {
        ca_data: inline_identity().ca_data,
        cert_data: inline_identity().cert_data,
        key_file: file_tls.key_file.clone(),
        ..Default::default()
    }
    .client_tls_config("localhost")
    .unwrap();
    FlameClientTls {
        ca_file: file_tls.ca_file,
        cert_file: file_tls.cert_file,
        key_data: inline_identity().key_data,
        ..Default::default()
    }
    .client_tls_config("localhost")
    .unwrap();
    FlameClientTls::default()
        .client_tls_config("localhost")
        .unwrap();
}

#[test]
fn every_credential_rejects_conflicting_or_empty_sources() {
    for name in ["ca", "cert", "key"] {
        for empty in [false, true] {
            let mut tls = inline_identity();
            let (file, data) = match name {
                "ca" => (&mut tls.ca_file, &mut tls.ca_data),
                "cert" => (&mut tls.cert_file, &mut tls.cert_data),
                _ => (&mut tls.key_file, &mut tls.key_data),
            };
            if empty {
                *data = Some(String::new());
            } else {
                *file = Some("unused.pem".into());
            }
            invalid_without_secrets(tls, if empty { "must not be empty" } else { "cannot both" });
        }
    }
}

#[test]
fn rejects_bad_base64_pem_and_mismatched_identity() {
    for value in ["not*base64", "Zg", "Zh==", "Zg==\n"] {
        invalid_without_secrets(
            FlameClientTls {
                ca_data: Some(value.into()),
                ..Default::default()
            },
            "base64",
        );
    }
    for bytes in [
        b"not PEM".as_slice(),
        b"-----BEGIN CERTIFICATE-----\nZg==\n-----END CERTIFICATE-----\n",
        &[0xff],
    ] {
        invalid_without_secrets(
            FlameClientTls {
                ca_data: Some(STANDARD.encode(bytes)),
                ..Default::default()
            },
            "PEM certificates",
        );
    }
    let mut tls = inline_identity();
    tls.key_data = Some(STANDARD.encode(b"not a private key"));
    invalid_without_secrets(tls, "private key");
    let mut tls = inline_identity();
    tls.key_data = Some(STANDARD.encode(&identity().other_key));
    invalid_without_secrets(tls, "matching identity");
    let mut tls = inline_identity();
    tls.cert_data = tls.key_data.clone();
    invalid_without_secrets(tls, "PEM certificates");
}

#[test]
fn inline_identity_round_trips_but_debug_is_redacted() {
    let tls = inline_identity();
    let encoded = serde_yaml::to_string(&tls).unwrap();
    let decoded: FlameClientTls = serde_yaml::from_str(&encoded).unwrap();
    assert!(decoded.ca_data == tls.ca_data);
    assert!(decoded.cert_data == tls.cert_data);
    assert!(decoded.key_data == tls.key_data);
    let debug = format!("{tls:?}");
    for data in [&tls.ca_data, &tls.cert_data, &tls.key_data] {
        assert!(!debug.contains(data.as_ref().unwrap()));
    }
    assert!(debug.contains("<redacted>"));
}

#[test]
fn environment_files_do_not_conflict_with_inline_yaml_and_can_complete_identity() {
    let (_dir, files) = identity_files();
    let mut target = Some(inline_identity());
    merge_tls(&mut target, &files);
    let target = target.unwrap();
    assert!(target.ca_file.is_none());
    assert!(target.cert_file.is_none());
    assert!(target.key_file.is_none());
    target.client_tls_config("localhost").unwrap();
    let mut partial = Some(FlameClientTls {
        ca_data: inline_identity().ca_data,
        cert_data: inline_identity().cert_data,
        ..Default::default()
    });
    merge_tls(&mut partial, &files);
    let partial = partial.unwrap();
    assert!(partial.cert_file.is_none());
    assert!(partial.key_file.is_some());
    partial.client_tls_config("localhost").unwrap();
}

#[derive(Clone)]
struct TlsService<const CACHE: bool> {
    endpoint: String,
}

impl<const CACHE: bool> tonic::server::NamedService for TlsService<CACHE> {
    const NAME: &'static str = if CACHE {
        "flame.v1.ObjectCacheService"
    } else {
        "flame.v1.Frontend"
    };
}

struct ListNodes;

impl tonic::server::UnaryService<rpc::ListNodesRequest> for ListNodes {
    type Response = rpc::NodeList;
    type Future = std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;

    fn call(&mut self, _: tonic::Request<rpc::ListNodesRequest>) -> Self::Future {
        std::future::ready(Ok(tonic::Response::new(rpc::NodeList::default())))
    }
}

struct PutObject(String);

impl tonic::server::ClientStreamingService<rpc::CacheWriteRequest> for PutObject {
    type Response = rpc::CacheObjectMetadata;
    type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;

    fn call(
        &mut self,
        request: tonic::Request<tonic::Streaming<rpc::CacheWriteRequest>>,
    ) -> Self::Future {
        let endpoint = self.0.clone();
        Box::pin(async move {
            let mut stream = request.into_inner();
            let header = stream
                .message()
                .await?
                .ok_or_else(|| tonic::Status::invalid_argument("missing header"))?;
            let Some(rpc::cache_write_request::Payload::Header(header)) = header.payload else {
                return Err(tonic::Status::invalid_argument("expected header"));
            };
            let mut size = 0;
            while let Some(request) = stream.message().await? {
                let Some(rpc::cache_write_request::Payload::Data(data)) = request.payload else {
                    return Err(tonic::Status::invalid_argument("expected data"));
                };
                size += data.len() as u64;
            }
            if size != header.size {
                return Err(tonic::Status::invalid_argument("incomplete upload"));
            }
            Ok(tonic::Response::new(rpc::CacheObjectMetadata {
                key: header.key,
                endpoint,
                version: 1,
                size,
                data_type: header.data_type,
                uuid: "test-object".into(),
                ..Default::default()
            }))
        })
    }
}

impl<const CACHE: bool> Service<http::Request<tonic::body::Body>> for TlsService<CACHE> {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let endpoint = self.endpoint.clone();
        Box::pin(async move {
            Ok(if CACHE {
                let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
                grpc.client_streaming(PutObject(endpoint), request).await
            } else {
                let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
                grpc.unary(ListNodes, request).await
            })
        })
    }
}

/// Start actual TLS services sharing a certificate and optional mandatory client authentication.
async fn tls_server(
    require_client: bool,
) -> (
    u16,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    stdng::tls::install_crypto_provider();
    let material = identity();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let endpoint = format!("grpcs://localhost:{port}");
    let mut tls = tonic::transport::ServerTlsConfig::new().identity(Identity::from_pem(
        material.cert.clone(),
        material.key.clone(),
    ));
    if require_client {
        tls = tls.client_ca_root(Certificate::from_pem(material.ca.clone()));
    }
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(TlsService::<false> {
                endpoint: endpoint.clone(),
            })
            .add_service(TlsService::<true> { endpoint })
            .serve_with_incoming_shutdown(
                tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    (port, shutdown, server)
}

/// Exercise the public cluster and package-upload cache paths using portable YAML only.
async fn exercise_clients(port: u16, tls: FlameClientTls) {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("package.whl");
    fs::write(&package, b"package content").unwrap();
    for scheme in ["grpcs", "grpc+tls", "grpcs-proxy"] {
        let context = FlameContext {
            current_context: "test".into(),
            contexts: vec![FlameContextEntry {
                name: "test".into(),
                cluster: FlameClusterConfig {
                    endpoint: format!("https://localhost:{port}"),
                    tls: Some(tls.clone()),
                },
                cache: Some(FlameClientCache {
                    endpoint: Some(format!("{scheme}://localhost:{port}")),
                    tls: Some(tls.clone()),
                    storage: None,
                }),
                package: None,
                app: None,
            }],
        };
        let config = dir.path().join("flame.yaml");
        fs::write(&config, serde_yaml::to_string(&context).unwrap()).unwrap();
        let loaded = FlameContext::from_file(Some(config.to_string_lossy().into_owned())).unwrap();
        fs::remove_file(config).unwrap();
        let connection = tokio::time::timeout(
            Duration::from_secs(5),
            crate::connect_with_context(loaded.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(connection.list_nodes().await.unwrap().is_empty());
        let reference = tokio::time::timeout(
            Duration::from_secs(5),
            crate::upload_package(&loaded, "test", "package.whl", &package),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reference.key, "test/pkg/package.whl");
    }
}

/// TLS rejection may arrive while connecting or on the first RPC, depending on TLS version.
async fn rejects_cluster(endpoint: &str, tls: Option<&FlameClientTls>) {
    let rejected = tokio::time::timeout(Duration::from_secs(5), async {
        match crate::client::connect_with_tls(endpoint, tls).await {
            Ok(connection) => connection.list_nodes().await.is_err(),
            Err(_) => true,
        }
    })
    .await
    .unwrap();
    assert!(rejected);
}

#[tokio::test]
async fn ca_only_inline_tls_reaches_cluster_and_cache_and_checks_hostname() {
    let (port, shutdown, server) = tls_server(false).await;
    let tls = FlameClientTls {
        ca_data: inline_identity().ca_data,
        ..Default::default()
    };
    exercise_clients(port, tls.clone()).await;
    rejects_cluster(&format!("https://127.0.0.1:{port}"), Some(&tls)).await;
    rejects_cluster(&format!("https://localhost:{port}"), None).await;
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn inline_mtls_authenticates_cluster_and_cache_without_certificate_files() {
    let (port, shutdown, server) = tls_server(true).await;
    exercise_clients(port, inline_identity()).await;
    let ca_only = FlameClientTls {
        ca_data: inline_identity().ca_data,
        ..Default::default()
    };
    rejects_cluster(&format!("https://localhost:{port}"), Some(&ca_only)).await;
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn file_based_mtls_remains_compatible_for_cluster_and_cache() {
    let (port, shutdown, server) = tls_server(true).await;
    let (_files, tls) = identity_files();
    exercise_clients(port, tls).await;
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

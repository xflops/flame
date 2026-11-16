/* Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License. */

use super::*;
use crate::controller::gc::FrontendLivenessProvider;
use tonic::codegen::{http, BoxFuture, Service};

#[derive(Clone)]
struct FrontendStub {
    sessions: Arc<Vec<flame_rpc::Session>>,
    applications: Arc<Vec<flame_rpc::Application>>,
}
impl tonic::server::NamedService for FrontendStub {
    const NAME: &'static str = "flame.v1.Frontend";
}
struct SessionLookup(Arc<Vec<flame_rpc::Session>>);
impl tonic::server::UnaryService<flame_rpc::ListSessionsRequest> for SessionLookup {
    type Response = flame_rpc::SessionList;
    type Future = std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;
    fn call(&mut self, request: tonic::Request<flame_rpc::ListSessionsRequest>) -> Self::Future {
        let request = request.into_inner();
        assert_eq!(request.ids.len(), 1);
        assert!(request.application.is_none());
        assert!(request.state.is_none());
        let paging = request.paging.unwrap();
        assert_eq!((paging.limit, paging.number), (1, 1));
        if request.ids[0] == "unreadable" {
            return std::future::ready(Err(tonic::Status::unavailable(
                "durable owner metadata unreadable",
            )));
        }
        let sessions: Vec<_> = if request.ids[0] == "malformed" {
            vec![flame_rpc::Session::default()]
        } else {
            self.0
                .iter()
                .filter(|s| s.metadata.as_ref().unwrap().id == request.ids[0])
                .cloned()
                .collect()
        };
        std::future::ready(Ok(tonic::Response::new(flame_rpc::SessionList {
            paging: Some(flame_rpc::Paging {
                limit: 1,
                number: 1,
                total: Some(sessions.len() as u64),
            }),
            sessions,
        })))
    }
}
struct ApplicationLookup(Arc<Vec<flame_rpc::Application>>);
impl tonic::server::UnaryService<flame_rpc::ListApplicationsRequest> for ApplicationLookup {
    type Response = flame_rpc::ApplicationList;
    type Future = std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;
    fn call(
        &mut self,
        request: tonic::Request<flame_rpc::ListApplicationsRequest>,
    ) -> Self::Future {
        let request = request.into_inner();
        assert_eq!(request.names.len(), 1);
        assert!(request.state.is_none());
        let paging = request.paging.unwrap();
        assert_eq!((paging.limit, paging.number), (1, 1));
        if request.names[0] == "unreadable" {
            return std::future::ready(Err(tonic::Status::unavailable(
                "durable application unreadable",
            )));
        }
        let applications: Vec<_> = self
            .0
            .iter()
            .filter(|a| a.metadata.as_ref().unwrap().name == request.names[0])
            .cloned()
            .collect();
        std::future::ready(Ok(tonic::Response::new(flame_rpc::ApplicationList {
            paging: Some(flame_rpc::Paging {
                limit: 1,
                number: 1,
                total: Some(applications.len() as u64),
            }),
            applications,
        })))
    }
}
impl Service<http::Request<tonic::body::Body>> for FrontendStub {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        match request.uri().path() {
            "/flame.v1.Frontend/ListSessions" => {
                let method = SessionLookup(self.sessions.clone());
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
                    Ok(grpc.unary(method, request).await)
                })
            }
            "/flame.v1.Frontend/ListApplications" => {
                let method = ApplicationLookup(self.applications.clone());
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
                    Ok(grpc.unary(method, request).await)
                })
            }
            path => panic!("unexpected RPC: {path}"),
        }
    }
}

/// Exact owner RPCs work above the old global cap and uncertainty never proves absence.
#[tokio::test]
async fn exact_owner_rpc_is_independent_of_global_count_and_fails_closed() {
    let stub = FrontendStub {
        sessions: Arc::new(
            (0..150)
                .map(|i| session("app", &format!("s{i}"), 1))
                .collect(),
        ),
        applications: Arc::new(
            (0..150)
                .map(|i| application(&format!("app{i}"), 0, 1))
                .collect(),
        ),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(stub)
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let cluster = common::ctx::FlameCluster {
        endpoint,
        ..Default::default()
    };
    let mut provider = FrontendLivenessProvider::new(&cluster).unwrap();
    let retained = key("app/s149/object");
    assert_eq!(
        classify_stale(
            &provider.snapshot(&retained).await.unwrap(),
            &retained,
            1,
            100_000
        ),
        None
    );
    let application = key("app149/pkg/object");
    assert_eq!(
        classify_stale(
            &provider.snapshot(&application).await.unwrap(),
            &application,
            1,
            100_000
        ),
        None
    );
    let absent = key("app/gone/object");
    assert_eq!(
        classify_stale(
            &provider.snapshot(&absent).await.unwrap(),
            &absent,
            1,
            100_000
        ),
        Some(StaleReason::MissingSession)
    );
    for path in [
        "app/unreadable/object",
        "app/malformed/object",
        "unreadable/pkg/object",
    ] {
        assert!(provider.snapshot(&key(path)).await.is_err());
    }
    let _ = shutdown.send(());
    server.await.unwrap();
}

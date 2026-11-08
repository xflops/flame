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

use super::{connect, rpc, Connection, FlameError, NodeState};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tonic::codegen::{http, BoxFuture, Service};

#[derive(Clone, Default)]
struct FrontendStub {
    list_calls: Arc<AtomicUsize>,
    deleted: Arc<Mutex<Vec<String>>>,
}

impl tonic::server::NamedService for FrontendStub {
    const NAME: &'static str = "flame.v1.Frontend";
}

struct ListNodes;

impl tonic::server::UnaryService<rpc::ListNodesRequest> for ListNodes {
    type Response = rpc::NodeList;
    type Future = std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;

    fn call(&mut self, _request: tonic::Request<rpc::ListNodesRequest>) -> Self::Future {
        let nodes = [
            ("ready", rpc::NodeState::Ready),
            ("unknown", rpc::NodeState::Unknown),
            ("busy", rpc::NodeState::NotReady),
        ]
        .into_iter()
        .map(|(name, state)| rpc::Node {
            metadata: Some(rpc::Metadata {
                name: name.into(),
                ..Default::default()
            }),
            status: Some(rpc::NodeStatus {
                state: state as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
        std::future::ready(Ok(tonic::Response::new(rpc::NodeList { nodes })))
    }
}

struct DeleteNode(Arc<Mutex<Vec<String>>>);

impl tonic::server::UnaryService<rpc::DeleteNodeRequest> for DeleteNode {
    type Response = rpc::Result;
    type Future = std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<rpc::DeleteNodeRequest>) -> Self::Future {
        let name = request.into_inner().name;
        self.0.lock().unwrap().push(name.clone());
        std::future::ready(if name == "busy" {
            Err(tonic::Status::failed_precondition(
                "node still has executors",
            ))
        } else {
            Ok(tonic::Response::new(rpc::Result::default()))
        })
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
            "/flame.v1.Frontend/ListNodes" => {
                self.list_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
                    Ok(grpc.unary(ListNodes, request).await)
                })
            }
            "/flame.v1.Frontend/DeleteNode" => {
                let method = DeleteNode(self.deleted.clone());
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
                    Ok(grpc.unary(method, request).await)
                })
            }
            path => panic!("unexpected RPC: {path}"),
        }
    }
}

async fn start_server(
    stub: FrontendStub,
) -> (
    Connection,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(stub)
            .serve_with_incoming_shutdown(
                tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let connection = connect(&endpoint).await.unwrap();
    (connection, shutdown, server)
}

#[tokio::test]
async fn list_nodes_decodes_every_node_state() {
    let stub = FrontendStub::default();
    let (connection, shutdown, server) = start_server(stub.clone()).await;
    let nodes = tokio::time::timeout(std::time::Duration::from_secs(5), connection.list_nodes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        nodes
            .iter()
            .map(|node| (node.name.as_str(), node.state))
            .collect::<Vec<_>>(),
        [
            ("ready", NodeState::Ready),
            ("unknown", NodeState::Unknown),
            ("busy", NodeState::NotReady),
        ]
    );
    assert_eq!(stub.list_calls.load(Ordering::SeqCst), 1);
    assert!(stub.deleted.lock().unwrap().is_empty());
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn delete_node_validates_names_and_propagates_server_errors() {
    let stub = FrontendStub::default();
    let (connection, shutdown, server) = start_server(stub.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for name in ["", " \t"] {
            assert!(matches!(connection.delete_node(name).await, Err(FlameError::InvalidConfig(_))));
        }
        assert!(stub.deleted.lock().unwrap().is_empty());
        connection.delete_node("stale-worker").await.unwrap();
        let error = connection.delete_node("busy").await.unwrap_err();
        assert!(matches!(error, FlameError::Network(ref message) if message == "node still has executors"));
        assert_eq!(*stub.deleted.lock().unwrap(), ["stale-worker", "busy"]);
        assert_eq!(stub.list_calls.load(Ordering::SeqCst), 0);
    }).await.unwrap();
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

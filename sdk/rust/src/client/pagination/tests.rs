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

use std::sync::{Arc, Mutex};

use futures::StreamExt;

use super::*;

#[test]
fn explicit_default_page_remains_present_and_missing_response_is_rejected() {
    let paging = rpc::PageRequest::from(&PageRequest::default());
    assert_eq!(paging.page_size, 0);
    assert_eq!(paging.page_number, 0);
    assert!(PageInfo::required(None).is_err());
    assert_eq!(
        PageInfo::required(Some(rpc::PageInfo::default()))
            .unwrap()
            .total_count,
        0
    );
}

#[test]
fn page_configuration_and_counts_preserve_the_wire_contract() {
    let request = PageRequest {
        page_size: 7,
        page_number: 3,
        order_by: "creation_time desc".into(),
    };
    let wire = rpc::PageRequest::from(&request);
    assert_eq!(wire.page_size, 7);
    assert_eq!(wire.page_number, 3);
    assert_eq!(wire.order_by, "creation_time desc");
    let info: PageInfo = rpc::PageInfo {
        total_count: 3,
        resource_count: 9,
        state_counts: [("Open".into(), 2)].into_iter().collect(),
        totals: [("running_tasks".into(), 8)].into_iter().collect(),
        ..Default::default()
    }
    .into();
    assert_eq!(info.total_count, 3);
    assert_eq!(info.resource_count, 9);
    assert_eq!(info.state_counts["Open"], 2);
    assert_eq!(info.totals["running_tasks"], 8);
    assert!(!TaskFilter::default().include_payloads);
    assert!(SessionFilter::default().to_request().paging.is_none());
}

#[tokio::test]
async fn traversal_is_lazy_and_continues_through_an_empty_page() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let stream = pages(
        PageRequest {
            page_size: 2,
            order_by: "id desc".into(),
            ..Default::default()
        },
        move |request| {
            let recorded = recorded.clone();
            async move {
                let first = request.page_number == 0;
                recorded.lock().unwrap().push(request);
                Ok(Page {
                    items: if first { Vec::new() } else { vec![1] },
                    paging: PageInfo {
                        page_number: if first { 1 } else { 2 },
                        page_size: 2,
                        total_pages: 2,
                        ..Default::default()
                    },
                })
            }
        },
    );
    assert!(seen.lock().unwrap().is_empty());
    futures::pin_mut!(stream);
    assert!(stream.next().await.unwrap().unwrap().items.is_empty());
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(stream.next().await.unwrap().unwrap().items, [1]);
    assert!(stream.next().await.is_none());
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].page_number, 2);
    assert_eq!(seen[1].order_by, "id desc");
    assert_eq!(seen[1].page_size, 2);
}

#[tokio::test]
async fn traversal_stops_on_error_without_requesting_another_page() {
    let stream = pages::<(), _, _>(PageRequest::default(), |_| async {
        Err(FlameError::Network("failed".into()))
    });
    futures::pin_mut!(stream);
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}

/// Exercise the actual protobuf requests and streaming metadata without a running cluster.
#[derive(Clone, Default)]
struct PageService {
    nodes: Arc<Mutex<Vec<rpc::ListNodesRequest>>>,
    tasks: Arc<Mutex<Vec<rpc::ListTasksRequest>>>,
}

impl tonic::server::NamedService for PageService {
    const NAME: &'static str = "flame.v1.Frontend";
}

impl tonic::server::UnaryService<rpc::ListNodesRequest> for PageService {
    type Response = rpc::NodeList;
    type Future = std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<rpc::ListNodesRequest>) -> Self::Future {
        let request = request.into_inner();
        let paging = request
            .paging
            .as_ref()
            .filter(|_| !request.names.iter().any(|name| name == "old-server"))
            .map(|_| rpc::PageInfo {
                total_count: 4,
                ..Default::default()
            });
        self.nodes.lock().unwrap().push(request);
        std::future::ready(Ok(tonic::Response::new(rpc::NodeList {
            nodes: Vec::new(),
            paging,
        })))
    }
}

impl tonic::server::ServerStreamingService<rpc::ListTasksRequest> for PageService {
    type Response = rpc::Task;
    type ResponseStream =
        futures::stream::Iter<std::vec::IntoIter<Result<rpc::Task, tonic::Status>>>;
    type Future = std::future::Ready<Result<tonic::Response<Self::ResponseStream>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<rpc::ListTasksRequest>) -> Self::Future {
        let request = request.into_inner();
        let mode = request.session_id.clone();
        self.tasks.lock().unwrap().push(request);
        // A malformed body proves missing metadata wins before task conversion.
        let tasks = if mode == "old-server" {
            vec![Ok(rpc::Task::default())]
        } else {
            Vec::new()
        };
        let mut response = tonic::Response::new(futures::stream::iter(tasks));
        if mode != "old-server" {
            let bytes = if mode == "invalid" {
                vec![0xff]
            } else {
                rpc::PageInfo {
                    total_count: 8,
                    ..Default::default()
                }
                .encode_to_vec()
            };
            response.metadata_mut().insert_bin(
                "flame-page-bin",
                tonic::metadata::MetadataValue::from_bytes(&bytes),
            );
        }
        std::future::ready(Ok(response))
    }
}

impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for PageService {
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
        let service = self.clone();
        Box::pin(async move {
            if request.uri().path().ends_with("/ListNodes") {
                Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                    .unary(service, request)
                    .await)
            } else {
                Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                    .server_streaming(service, request)
                    .await)
            }
        })
    }
}

#[tokio::test]
async fn grpc_pages_forward_filters_and_require_response_metadata_even_for_empty_tasks() {
    let service = PageService::default();
    let requests = service.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let connection = super::super::connect(&format!("http://{address}"))
        .await
        .unwrap();
    assert!(connection.list_nodes().await.unwrap().is_empty());
    let paging = PageRequest {
        page_size: 3,
        page_number: 3,
        order_by: "name desc".into(),
    };
    let page = connection
        .list_nodes_page(
            &NodeFilter {
                state: Some(NodeState::Ready),
                names: vec!["n".into()],
            },
            &paging,
        )
        .await
        .unwrap();
    assert_eq!(page.paging.total_count, 4);
    {
        let requests = requests.nodes.lock().unwrap();
        assert!(requests[0].paging.is_none());
        assert_eq!(requests[1].paging, Some((&paging).into()));
        assert_eq!(requests[1].state, Some(NodeState::Ready as i32));
        assert_eq!(requests[1].names, ["n"]);
    }
    let filter = TaskFilter {
        state: Some(TaskState::Failed),
        ids: vec![12],
        include_payloads: true,
    };
    let page = connection
        .list_tasks_page(&"s".into(), &filter, &paging)
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.paging.total_count, 8);
    {
        let requests = requests.tasks.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].session_id, "s");
        assert_eq!(requests[0].state, Some(TaskState::Failed as i32));
        assert_eq!(requests[0].ids, [12]);
        assert!(requests[0].include_payloads);
        assert_eq!(requests[0].paging, Some((&paging).into()));
    }
    for (mode, expected) in [
        ("old-server", "did not return pagination metadata"),
        ("invalid", "invalid task pagination metadata"),
    ] {
        let error = connection
            .list_tasks_page(
                &mode.into(),
                &TaskFilter::default(),
                &PageRequest::default(),
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert!(connection
        .list_nodes_page(
            &NodeFilter {
                names: vec!["old-server".into()],
                ..Default::default()
            },
            &PageRequest::default()
        )
        .await
        .is_err());
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn empty_collection_is_a_single_terminal_page() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let stream = pages::<(), _, _>(PageRequest::default(), move |_| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async {
            Ok(Page {
                items: Vec::new(),
                paging: PageInfo {
                    page_number: 1,
                    page_size: 100,
                    total_pages: 0,
                    ..Default::default()
                },
            })
        }
    });
    futures::pin_mut!(stream);
    assert!(stream.next().await.unwrap().unwrap().items.is_empty());
    assert!(stream.next().await.is_none());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

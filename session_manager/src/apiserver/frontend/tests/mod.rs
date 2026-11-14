/*
Copyright 2023 The Flame Authors.
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
use futures::{Stream, StreamExt};

async fn watch_task_stream<S>(
    controller: crate::controller::ControllerPtr,
    watch_requests: S,
    tx: mpsc::Sender<Result<Task, Status>>,
) -> Result<(), Status>
where
    S: Stream<Item = Result<WatchTaskRequest, Status>> + Unpin + Send + 'static,
{
    let flame = Flame {
        controller,
        cluster_default_resreq: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| Status::internal(error.to_string()))?;
    let address = listener.local_addr().unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        let (socket, _) = listener.accept().await.ok()?;
        Some((Ok::<_, std::io::Error>(socket), listener))
    });
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(rpc::frontend_server::FrontendServer::new(flame))
            .serve_with_incoming(Box::pin(incoming)),
    );
    let result = async {
            let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .map_err(|error| Status::internal(error.to_string()))?;
            let channel = endpoint
                .connect()
                .await
                .map_err(|error| Status::internal(error.to_string()))?;
            let mut client = rpc::frontend_client::FrontendClient::new(channel);
            let response = tokio::select! {
                _ = tx.closed() => return Ok(()),
                response = client.watch_tasks(watch_requests.map(|watch_request| watch_request.unwrap())) => response?,
            };
            let mut task_updates = response.into_inner();
            loop {
                tokio::select! {
                    _ = tx.closed() => return Ok(()),
                    task_update = task_updates.message() => match task_update? {
                        Some(task_update) => tx.send(Ok(task_update)).await
                            .map_err(|_| Status::cancelled("task watch stream closed"))?,
                        None => return Ok(()),
                    },
                }
            }
        }
        .await;
    server.abort();
    result
}

async fn watch_test_storage_and_controller() -> (
    crate::storage::StoragePtr,
    crate::controller::ControllerPtr,
    apis::SessionID,
) {
    task_test_controller("none".to_string()).await
}

async fn task_test_controller(
    storage_url: String,
) -> (
    crate::storage::StoragePtr,
    crate::controller::ControllerPtr,
    apis::SessionID,
) {
    let config = common::ctx::FlameClusterContext {
        cluster: common::ctx::FlameCluster {
            storage: storage_url,
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = crate::storage::new_ptr(&config).await.unwrap();
    let controller = crate::controller::new_ptr(storage.clone());
    controller
        .register_application("test-app".to_string(), ApplicationAttributes::default())
        .await
        .unwrap();
    // File-backed event managers share a root; isolate each fixture's session files.
    let ssn_id = format!("watch-test-session-{}", uuid::Uuid::new_v4());
    controller
        .create_session(SessionAttributes {
            id: ssn_id.clone(),
            application: "test-app".to_string(),
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    (storage, controller, ssn_id)
}

async fn watch_test_controller() -> (crate::controller::ControllerPtr, apis::SessionID) {
    let (_, controller, ssn_id) = watch_test_storage_and_controller().await;
    (controller, ssn_id)
}

fn watch_request(ssn_id: &str, task_id: apis::TaskID) -> WatchTaskRequest {
    WatchTaskRequest {
        session_id: ssn_id.to_string(),
        task_id: task_id.to_string(),
    }
}

type WatchRequest = Result<WatchTaskRequest, Status>;

fn watch_requests(
    ssn_id: &str,
    task_id: apis::TaskID,
) -> (mpsc::Sender<WatchRequest>, ReceiverStream<WatchRequest>) {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(Ok(watch_request(ssn_id, task_id))).unwrap();
    (tx, ReceiverStream::new(rx))
}

fn test_flame(controller: crate::controller::ControllerPtr) -> Flame {
    Flame {
        controller,
        cluster_default_resreq: None,
    }
}

fn rr(cpu: u64, memory: u64, gpu: i32) -> ResourceRequirement {
    ResourceRequirement { cpu, memory, gpu }
}

mod frontend;

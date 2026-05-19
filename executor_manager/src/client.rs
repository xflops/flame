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
use std::time::Duration;

use stdng::{lock_ptr, MutexPtr};
use tokio_stream::Stream;
use tonic::transport::Channel;
use tonic::Streaming;

use ::rpc::flame::v1 as rpc;
use ::rpc::flame::v1::backend_client::BackendClient as FlameBackendClient;
use ::rpc::flame::v1::{
    BindExecutorCompletedRequest, BindExecutorRequest, CompleteTaskRequest, LaunchTaskRequest,
    RegisterExecutorRequest, RegisterNodeRequest, ReleaseNodeRequest, SyncNodeRequest,
    UnbindExecutorCompletedRequest, UnbindExecutorRequest, UnregisterExecutorRequest,
    WatchNodeRequest, WatchNodeResponse,
};

use crate::executor::Executor;
use common::apis::{
    Application, FlameResult, Node, ResourceRequirement, Session, SessionContext, Shim,
    TaskContext, TaskResult,
};
use common::ctx::FlameClusterContext;
use common::net::host_for_uri;
use common::FlameError;

const DEFAULT_PORT: u16 = 8080;

pub type FlameClient = FlameBackendClient<Channel>;

#[derive(Clone, Debug)]
pub struct BackendClient {
    client: FlameClient,
}

impl BackendClient {
    pub async fn new(ctx: &FlameClusterContext) -> Result<Self, FlameError> {
        let url = url::Url::parse(&ctx.cluster.endpoint).map_err(|_| {
            FlameError::InvalidConfig(format!("invalid endpoint <{}>", ctx.cluster.endpoint))
        })?;
        let port = url.port().unwrap_or(DEFAULT_PORT) + 1;

        let endpoint = format!(
            "{}://{}:{port}",
            url.scheme(),
            host_for_uri(url.host_str().unwrap_or("localhost"))
        );

        tracing::info!("Connecting to flame backend at {}", endpoint);
        let mut channel_builder = Channel::from_shared(endpoint.clone()).map_err(|e| {
            FlameError::Network(format!("Failed to create channel for <{endpoint}>: {e}"))
        })?;

        // Apply TLS if endpoint uses https://
        if endpoint.starts_with("https://") {
            let tls_config = if let Some(ref tls) = ctx.cluster.tls {
                tls.client_tls_config()?
            } else {
                // Use default TLS config (system CA bundle)
                tonic::transport::ClientTlsConfig::new()
            };
            channel_builder = channel_builder
                .tls_config(tls_config)
                .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;
            tracing::info!("TLS enabled for backend client");
        }

        let channel = channel_builder
            .connect()
            .await
            .map_err(|e| FlameError::Network(format!("Failed to connect to <{endpoint}>: {e}")))?;

        let client = FlameBackendClient::new(channel);

        Ok(Self { client })
    }

    #[cfg(test)]
    pub fn default() -> Self {
        use tonic::transport::Endpoint;
        let channel = Endpoint::from_static("http://[::1]:50051").connect_lazy();
        Self {
            client: FlameBackendClient::new(channel),
        }
    }

    pub async fn register_node(
        &mut self,
        node: &Node,
        executors: &[Executor],
    ) -> Result<(), FlameError> {
        let req = RegisterNodeRequest {
            node: Some(node.clone().into()),
            executors: executors.iter().map(rpc::Executor::from).collect(),
        };

        self.client
            .register_node(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }

    /// # Deprecated
    /// Use `watch_node` streaming RPC instead for better efficiency.
    /// `sync_node` uses polling which is less efficient than server-push.
    #[deprecated(since = "0.6.0", note = "Use watch_node streaming RPC instead")]
    pub async fn sync_node(
        &mut self,
        node: &Node,
        executors: Vec<Executor>,
    ) -> Result<Vec<Executor>, FlameError> {
        let req = SyncNodeRequest {
            node: Some(node.clone().into()),
            executors: executors.into_iter().map(rpc::Executor::from).collect(),
        };

        let resp = self.client.sync_node(req).await.map_err(FlameError::from)?;

        let executors = resp
            .into_inner()
            .executors
            .iter()
            .map(Executor::try_from)
            .collect::<Result<Vec<Executor>, FlameError>>()?;

        Ok(executors)
    }

    /// Establishes a bidirectional streaming connection for WatchNode.
    ///
    /// This method creates a streaming RPC that allows the client to send
    /// registration and heartbeat messages while receiving executor action
    /// notifications from the server.
    ///
    /// # Arguments
    ///
    /// * `request_stream` - A stream of WatchNodeRequest messages to send
    ///
    /// # Returns
    ///
    /// Returns a streaming response that yields WatchNodeResponse messages.
    pub async fn watch_node<S>(
        &mut self,
        request_stream: S,
    ) -> Result<Streaming<WatchNodeResponse>, FlameError>
    where
        S: Stream<Item = WatchNodeRequest> + Send + 'static,
    {
        let resp = self
            .client
            .watch_node(request_stream)
            .await
            .map_err(FlameError::from)?;

        Ok(resp.into_inner())
    }

    pub async fn release_node(&mut self, node: &Node) -> Result<(), FlameError> {
        let req = ReleaseNodeRequest {
            node_name: node.name.clone(),
        };

        self.client
            .release_node(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }

    pub async fn register_executor(&mut self, exe: &Executor) -> Result<(), FlameError> {
        let req = RegisterExecutorRequest {
            executor_id: exe.id.clone(),
            executor_spec: Some(rpc::ExecutorSpec {
                resreq: Some(exe.resreq.clone().into()),
                node: exe.node.clone(),
                shim: rpc::Shim::from(exe.shim).into(), // Include shim in registration
            }),
        };

        self.client
            .register_executor(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }

    pub async fn bind_executor(
        &mut self,
        exe: &Executor,
    ) -> Result<Option<SessionContext>, FlameError> {
        let req = BindExecutorRequest {
            executor_id: exe.id.clone(),
        };

        let resp = self
            .client
            .bind_executor(req)
            .await
            .map_err(FlameError::from)?;

        let resp = resp.into_inner();
        let ssn = resp.clone().session;
        let app = resp.clone().application;

        match (app, ssn) {
            (Some(app), Some(ssn)) => Ok(Some(SessionContext::try_from((app, ssn))?)),
            _ => Ok(None),
        }
    }

    pub async fn bind_executor_completed(
        &mut self,
        exe: &Executor,
        result: Option<FlameResult>,
    ) -> Result<(), FlameError> {
        let req = BindExecutorCompletedRequest {
            executor_id: exe.id.clone(),
            result: result.map(rpc::Result::from),
        };

        self.client
            .bind_executor_completed(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }

    pub async fn unbind_executor(&mut self, exe: &Executor) -> Result<(), FlameError> {
        let req = UnbindExecutorRequest {
            executor_id: exe.id.clone(),
        };

        self.client
            .unbind_executor(req)
            .await
            .map_err(FlameError::from)?;
        Ok(())
    }

    pub async fn unbind_executor_completed(&mut self, exe: &Executor) -> Result<(), FlameError> {
        let req = UnbindExecutorCompletedRequest {
            executor_id: exe.id.clone(),
        };

        self.client
            .unbind_executor_completed(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }

    pub async fn launch_task(&mut self, exe: &Executor) -> Result<Option<TaskContext>, FlameError> {
        let req = LaunchTaskRequest {
            executor_id: exe.id.clone(),
        };

        let resp = self
            .client
            .launch_task(req)
            .await
            .map_err(FlameError::from)?;

        if let Some(t) = resp.into_inner().task {
            return Ok(Some(TaskContext::try_from(t)?));
        }

        Ok(None)
    }

    pub async fn complete_task(
        &mut self,
        exe: &Executor,
        task_result: &TaskResult,
    ) -> Result<(), FlameError> {
        let req = CompleteTaskRequest {
            executor_id: exe.id.clone(),
            task_result: Some(task_result.clone().try_into()?),
        };

        self.client
            .complete_task(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }

    pub async fn unregister_executor(&mut self, exe: &Executor) -> Result<(), FlameError> {
        let req = UnregisterExecutorRequest {
            executor_id: exe.id.clone(),
        };

        self.client
            .unregister_executor(req)
            .await
            .map_err(FlameError::from)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_for_uri_brackets_ipv6() {
        assert_eq!(host_for_uri("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(host_for_uri("localhost"), "localhost");
    }
}

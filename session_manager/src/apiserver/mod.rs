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

use std::sync::Arc;
use std::time::Duration;
use tonic::transport::Server;

use common::apis::ResourceRequirement;
use common::ctx::FlameClusterContext;
use rpc::flame::v1::backend_server::BackendServer;
use rpc::flame::v1::frontend_server::FrontendServer;

use crate::controller::ControllerPtr;
use crate::{FlameError, FlameThread};

mod backend;
mod frontend;

const DEFAULT_PORT: u16 = 8080;
const ALL_HOST_ADDRESS: &str = "0.0.0.0";

pub struct Flame {
    controller: ControllerPtr,
    /// Cluster-wide default `resreq` (from `cluster.resreq`
    /// in `flame-cluster.yaml`), used by the frontend when a session spec
    /// supplies no explicit `resreq`. The backend service does not need this
    /// — it is always `None` for the backend `Flame` instance.
    cluster_default_resreq: Option<ResourceRequirement>,
}

pub fn new_frontend(controller: ControllerPtr) -> Arc<dyn FlameThread> {
    Arc::new(FrontendRunner { controller })
}

pub fn new_backend(controller: ControllerPtr) -> Arc<dyn FlameThread> {
    Arc::new(BackendRunner { controller })
}

struct FrontendRunner {
    controller: ControllerPtr,
}

#[async_trait::async_trait]
impl FlameThread for FrontendRunner {
    async fn run(&self, ctx: FlameClusterContext) -> Result<(), FlameError> {
        let url = url::Url::parse(&ctx.cluster.endpoint).map_err(|_| {
            FlameError::InvalidConfig(format!("invalid endpoint <{}>", ctx.cluster.endpoint))
        })?;

        let port = url.port().unwrap_or(DEFAULT_PORT);

        // The fsm will bind to all addresses of host directly.
        let address_str = format!("{ALL_HOST_ADDRESS}:{port}");
        tracing::info!("Listening apiserver frontend at {}", address_str);
        let address = address_str.parse().map_err(|_| {
            FlameError::InvalidConfig(format!("failed to parse url <{address_str}>"))
        })?;

        let frontend_service = Flame {
            controller: self.controller.clone(),
            cluster_default_resreq: ctx.cluster.resreq.clone(),
        };

        let mut builder = Server::builder().tcp_keepalive(Some(Duration::from_secs(1)));

        // Apply TLS if configured
        if let Some(ref tls_config) = ctx.cluster.tls {
            let tls = tls_config.server_tls_config()?;
            builder = builder
                .tls_config(tls)
                .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;
            tracing::info!("TLS enabled for frontend apiserver");
        }

        builder
            .add_service(FrontendServer::new(frontend_service))
            .serve(address)
            .await
            .map_err(|e| FlameError::Network(e.to_string()))?;

        Ok(())
    }
}

struct BackendRunner {
    controller: ControllerPtr,
}

#[async_trait::async_trait]
impl FlameThread for BackendRunner {
    async fn run(&self, ctx: FlameClusterContext) -> Result<(), FlameError> {
        let url = url::Url::parse(&ctx.cluster.endpoint).map_err(|_| {
            FlameError::InvalidConfig(format!("invalid endpoint <{}>", ctx.cluster.endpoint))
        })?;
        let port = url.port().unwrap_or(DEFAULT_PORT) + 1;

        // The fsm will bind to all addresses of host directly.
        let address_str = format!("{ALL_HOST_ADDRESS}:{port}");
        tracing::info!("Listening apiserver backend at {}", address_str);
        let address = address_str.parse().map_err(|_| {
            FlameError::InvalidConfig(format!("failed to parse url <{address_str}>"))
        })?;

        let backend_service = Flame {
            controller: self.controller.clone(),
            // Backend never resolves session resreq — it serves executor-side
            // RPCs only. Always `None` here.
            cluster_default_resreq: None,
        };

        let mut builder = Server::builder().tcp_keepalive(Some(Duration::from_secs(1)));

        // Apply TLS if configured
        if let Some(ref tls_config) = ctx.cluster.tls {
            let tls = tls_config.server_tls_config()?;
            builder = builder
                .tls_config(tls)
                .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;
            tracing::info!("TLS enabled for backend apiserver");
        }

        builder
            .add_service(BackendServer::new(backend_service))
            .serve(address)
            .await
            .map_err(|e| FlameError::Network(e.to_string()))?;

        Ok(())
    }
}

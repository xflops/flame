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

use common::ctx::FlameClusterContext;
use rpc::flame::admin_server::AdminServer;
use rpc::flame::backend_server::BackendServer;
use rpc::flame::frontend_server::FrontendServer;

use crate::cert::CertManagerImpl;
use crate::controller::ControllerPtr;
use crate::{FlameError, FlameThread};

use self::auth::AuthLayer;

mod admin;
pub mod auth;
mod backend;
mod frontend;

const DEFAULT_PORT: u16 = 8080;
const ALL_HOST_ADDRESS: &str = "0.0.0.0";

pub struct Flame {
    controller: ControllerPtr,
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

        let address_str = format!("{ALL_HOST_ADDRESS}:{port}");
        tracing::info!("Listening apiserver frontend at {}", address_str);
        let address = address_str.parse().map_err(|_| {
            FlameError::InvalidConfig(format!("failed to parse url <{address_str}>"))
        })?;

        let mut builder = Server::builder().tcp_keepalive(Some(Duration::from_secs(1)));

        let auth_layer = if let Some(ref tls_config) = ctx.cluster.tls {
            let tls = tls_config.server_tls_config()?;
            builder = builder
                .tls_config(tls)
                .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;

            if let (Some(ca_file), Some(ca_key_file)) =
                (&tls_config.ca_file, &tls_config.ca_key_file)
            {
                tracing::info!("mTLS enabled: client certificates will be required and validated");
                let cert_manager = CertManagerImpl::new_ptr(ca_file, ca_key_file)
                    .map_err(|e| FlameError::InvalidConfig(format!("CertManager error: {}", e)))?;
                AuthLayer::new(self.controller.clone(), cert_manager)
            } else {
                tracing::info!(
                    "TLS enabled for frontend apiserver (server-only, no client cert required)"
                );
                AuthLayer::insecure()
            }
        } else {
            AuthLayer::insecure()
        };

        let frontend_service = Flame {
            controller: self.controller.clone(),
        };

        let admin_service = admin::AdminService::new(self.controller.clone());

        builder
            .layer(auth_layer)
            .add_service(FrontendServer::new(frontend_service))
            .add_service(AdminServer::new(admin_service))
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

        let address_str = format!("{ALL_HOST_ADDRESS}:{port}");
        tracing::info!("Listening apiserver backend at {}", address_str);
        let address = address_str.parse().map_err(|_| {
            FlameError::InvalidConfig(format!("failed to parse url <{address_str}>"))
        })?;

        let mut builder = Server::builder().tcp_keepalive(Some(Duration::from_secs(1)));

        let auth_layer = if let Some(ref tls_config) = ctx.cluster.tls {
            let tls = tls_config.server_tls_config()?;
            builder = builder
                .tls_config(tls)
                .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;

            if let (Some(ca_file), Some(ca_key_file)) =
                (&tls_config.ca_file, &tls_config.ca_key_file)
            {
                tracing::info!("mTLS enabled for backend apiserver");
                let cert_manager = CertManagerImpl::new_ptr(ca_file, ca_key_file)
                    .map_err(|e| FlameError::InvalidConfig(format!("CertManager error: {}", e)))?;
                AuthLayer::new(self.controller.clone(), cert_manager)
            } else {
                tracing::info!("TLS enabled for backend apiserver");
                AuthLayer::insecure()
            }
        } else {
            AuthLayer::insecure()
        };

        let backend_service = Flame {
            controller: self.controller.clone(),
        };

        builder
            .layer(auth_layer)
            .add_service(BackendServer::new(backend_service))
            .serve(address)
            .await
            .map_err(|e| FlameError::Network(e.to_string()))?;

        Ok(())
    }
}

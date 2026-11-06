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

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::Server;

use common::apis::ResourceRequirement;
use common::ctx::FlameClusterContext;
use common::net::ip_addr;
use rpc::flame::v1::backend_server::BackendServer;
use rpc::flame::v1::frontend_server::FrontendServer;

use crate::controller::ControllerPtr;
use crate::{FlameError, FlameThread};

mod backend;
mod frontend;

const DEFAULT_PORT: u16 = 8080;
/// Bind to the selected local IP using the endpoint port plus the listener's
/// shift: zero for frontend, one for backend.
fn bind_address(ctx: &FlameClusterContext, port_shift: u16) -> Result<SocketAddr, FlameError> {
    let url = url::Url::parse(&ctx.cluster.endpoint).map_err(|_| {
        FlameError::InvalidConfig(format!("invalid endpoint <{}>", ctx.cluster.endpoint))
    })?;
    let port = url
        .port()
        .unwrap_or(DEFAULT_PORT)
        .checked_add(port_shift)
        .ok_or_else(|| {
            FlameError::InvalidConfig("cluster.endpoint has no available listener port".to_string())
        })?;
    let ip = match ctx
        .cluster
        .advertise
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(value) => ip_addr(value)?.ok_or_else(|| {
            FlameError::InvalidConfig(format!(
                "cluster.advertise <{value}> is not an IP address or matching network interface"
            ))
        })?,
        None => Ipv4Addr::UNSPECIFIED.into(),
    };
    Ok(SocketAddr::new(ip, port))
}

#[derive(Clone)]
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
        let address = bind_address(&ctx, 0)?;
        tracing::info!("Listening apiserver frontend at {}", address);

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
        let address = bind_address(&ctx, 1)?;
        tracing::info!("Listening apiserver backend at {}", address);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn bind_defaults_and_literal_ips_do_not_discover_interfaces() {
        for (value, expected) in [
            (None, "0.0.0.0"),
            (Some(""), "0.0.0.0"),
            (Some("127.0.0.1"), "127.0.0.1"),
            (Some("192.0.2.10"), "192.0.2.10"),
            (Some("::1"), "::1"),
        ] {
            let mut ctx = FlameClusterContext::default();
            ctx.cluster.advertise = value.map(str::to_string);
            assert_eq!(
                bind_address(&ctx, 0).unwrap().ip(),
                expected.parse::<IpAddr>().unwrap()
            );
        }
    }

    #[test]
    fn bind_invalid_interfaces_fail_without_wildcard_fallback() {
        for value in ["[", "^flame_nonexistent_interface$", "grpc://cache:9090"] {
            let mut ctx = FlameClusterContext::default();
            ctx.cluster.advertise = Some(value.into());
            assert!(bind_address(&ctx, 0).is_err());
        }
        let mut ctx = FlameClusterContext::default();
        ctx.cluster.advertise = Some("^lo0?$".into());
        assert_eq!(
            bind_address(&ctx, 0).unwrap().ip(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn both_listener_addresses_use_advertise_and_endpoint_ports() {
        let mut ctx = FlameClusterContext::default();
        ctx.cluster.endpoint = "http://service.flame:18080".into();
        ctx.cluster.advertise = Some("127.0.0.1".into());
        assert_eq!(
            bind_address(&ctx, 0).unwrap(),
            "127.0.0.1:18080".parse().unwrap()
        );
        assert_eq!(
            bind_address(&ctx, 1).unwrap(),
            "127.0.0.1:18081".parse().unwrap()
        );
        ctx.cluster.advertise = Some("::1".into());
        ctx.cluster.endpoint = "http://service.flame".into();
        assert_eq!(
            bind_address(&ctx, 1).unwrap(),
            "[::1]:8081".parse().unwrap()
        );
        ctx.cluster.endpoint = "http://service.flame:65535".into();
        assert!(bind_address(&ctx, 1).is_err());
        ctx.cluster.endpoint = "invalid endpoint".into();
        assert!(bind_address(&ctx, 0).is_err());
    }
}

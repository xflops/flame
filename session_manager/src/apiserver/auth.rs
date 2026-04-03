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
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::future::BoxFuture;
use tonic::body::BoxBody;
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::transport::CertificateDer;
use tonic::Status;
use tower::{Layer, Service};
use x509_parser::prelude::*;

use common::authz::{AuthzContext, AuthzError, CredentialScope};
use common::rbac::{WORKSPACE_DEFAULT, WORKSPACE_SYSTEM};

use crate::cert::CertManager;
use crate::controller::ControllerPtr;

const WORKSPACE_HEADER: &str = "x-flame-workspace";

type Request = http::Request<tonic::body::BoxBody>;
type Response = http::Response<BoxBody>;

fn extract_cn_from_certs(certs: &[CertificateDer<'static>]) -> Option<String> {
    let first_cert = certs.first()?;
    let (_, cert) = X509Certificate::from_der(first_cert.as_ref()).ok()?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(String::from);
    cn
}

fn extract_cert_pem_from_certs(certs: &[CertificateDer<'static>]) -> Option<Vec<u8>> {
    use base64::Engine;
    let first_cert = certs.first()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(first_cert.as_ref());
    let pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        b64.chars()
            .collect::<Vec<_>>()
            .chunks(64)
            .map(|c| c.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    );
    Some(pem.into_bytes())
}

pub struct CredentialInfo {
    pub cn: String,
    pub cert_pem: Option<Vec<u8>>,
    pub workspace_header: Option<String>,
}

#[async_trait]
pub trait CredentialVerifier: Send + Sync {
    fn matches(&self, cn: &str) -> bool;
    async fn verify(&self, info: &CredentialInfo, method: &str)
        -> Result<AuthzContext, AuthzError>;
}

pub struct SessionCredentialVerifier {
    cert_manager: Arc<dyn CertManager>,
}

impl SessionCredentialVerifier {
    pub fn new(cert_manager: Arc<dyn CertManager>) -> Self {
        Self { cert_manager }
    }
}

#[async_trait]
impl CredentialVerifier for SessionCredentialVerifier {
    fn matches(&self, cn: &str) -> bool {
        cn.starts_with("session:") || cn.starts_with("delegate:")
    }

    async fn verify(
        &self,
        info: &CredentialInfo,
        _method: &str,
    ) -> Result<AuthzContext, AuthzError> {
        let cert_pem = info.cert_pem.as_ref().ok_or_else(|| {
            AuthzError::CertificateError("session certificate PEM not available".to_string())
        })?;

        let claims = self
            .cert_manager
            .verify(cert_pem)
            .await
            .map_err(|e| AuthzError::CertificateError(e.to_string()))?;

        let requested_workspace = info
            .workspace_header
            .as_deref()
            .unwrap_or(&claims.workspace);
        if requested_workspace != claims.workspace {
            return Err(AuthzError::PermissionDenied(format!(
                "session cert for workspace '{}' cannot access workspace '{}'",
                claims.workspace, requested_workspace
            )));
        }

        let mut ctx =
            AuthzContext::new(claims.subject.clone(), claims.workspace).with_scope(claims.scope);

        if let Some(parent) = claims.parent {
            ctx = ctx.with_parent(parent);
        }

        Ok(ctx)
    }
}

pub struct UserCredentialVerifier {
    controller: ControllerPtr,
}

impl UserCredentialVerifier {
    pub fn new(controller: ControllerPtr) -> Self {
        Self { controller }
    }

    async fn check_permission(
        &self,
        user_name: &str,
        workspace: &str,
        resource: &str,
        action: &str,
    ) -> Result<(), AuthzError> {
        let roles = self
            .controller
            .get_user_roles(user_name)
            .await
            .map_err(|e| AuthzError::Internal(e.to_string()))?;

        let required = format!("{}:{}", resource, action);

        for role in &roles {
            let has_workspace = role.workspaces.iter().any(|w| w == "*" || w == workspace);
            if !has_workspace {
                continue;
            }

            if role.has_permission(&required) {
                return Ok(());
            }
        }

        Err(AuthzError::PermissionDenied(format!(
            "user '{}' does not have permission '{}' in workspace '{}'",
            user_name, required, workspace
        )))
    }

    async fn is_root_user(&self, user_name: &str) -> Result<bool, AuthzError> {
        let roles = self
            .controller
            .get_user_roles(user_name)
            .await
            .map_err(|e| AuthzError::Internal(e.to_string()))?;

        for role in roles {
            if role.workspaces.contains(&"*".to_string())
                && role.permissions.contains(&"*:*".to_string())
            {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

#[async_trait]
impl CredentialVerifier for UserCredentialVerifier {
    fn matches(&self, cn: &str) -> bool {
        !cn.starts_with("session:") && !cn.starts_with("delegate:")
    }

    async fn verify(
        &self,
        info: &CredentialInfo,
        method: &str,
    ) -> Result<AuthzContext, AuthzError> {
        let user = self
            .controller
            .get_user_by_cn(&info.cn)
            .await
            .map_err(|e| AuthzError::Internal(e.to_string()))?
            .ok_or_else(|| AuthzError::UserNotFound(info.cn.clone()))?;

        if !user.enabled {
            return Err(AuthzError::UserDisabled(info.cn.clone()));
        }

        let workspace = info
            .workspace_header
            .as_deref()
            .unwrap_or(WORKSPACE_DEFAULT);

        if workspace == WORKSPACE_SYSTEM {
            let is_root = self.is_root_user(&user.name).await?;
            if !is_root {
                return Err(AuthzError::PermissionDenied(format!(
                    "user '{}' cannot access system workspace",
                    info.cn
                )));
            }
        }

        let (resource, action) = extract_permission_from_method(method);
        self.check_permission(&user.name, workspace, resource, action)
            .await?;

        Ok(AuthzContext::new(user.name, workspace.to_string()).with_scope(CredentialScope::User))
    }
}

#[derive(Clone)]
pub struct AuthLayer {
    verifiers: Arc<Vec<Arc<dyn CredentialVerifier>>>,
}

impl AuthLayer {
    pub fn new(controller: ControllerPtr, cert_manager: Arc<dyn CertManager>) -> Self {
        let verifiers: Vec<Arc<dyn CredentialVerifier>> = vec![
            Arc::new(SessionCredentialVerifier::new(cert_manager)),
            Arc::new(UserCredentialVerifier::new(controller)),
        ];
        Self {
            verifiers: Arc::new(verifiers),
        }
    }

    pub fn insecure() -> Self {
        Self {
            verifiers: Arc::new(vec![]),
        }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, service: S) -> Self::Service {
        AuthMiddleware {
            verifiers: self.verifiers.clone(),
            service,
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddleware<S> {
    verifiers: Arc<Vec<Arc<dyn CredentialVerifier>>>,
    service: S,
}

async fn authenticate(
    verifiers: Arc<Vec<Arc<dyn CredentialVerifier>>>,
    mut req: Request,
) -> Result<Request, Status> {
    if verifiers.is_empty() {
        req.extensions_mut().insert(AuthzContext::default());
        return Ok(req);
    }

    let tls_info: Option<TlsConnectInfo<TcpConnectInfo>> = req
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .cloned();

    let certs = tls_info
        .as_ref()
        .and_then(|tls| tls.peer_certs())
        .ok_or_else(|| Status::unauthenticated("no client certificate presented"))?;

    let cn = extract_cn_from_certs(&certs)
        .ok_or_else(|| Status::unauthenticated("could not extract CN from certificate"))?;

    let cert_pem = extract_cert_pem_from_certs(&certs);

    let workspace_header = req
        .headers()
        .get(WORKSPACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let method = req.uri().path().to_string();

    let credential_info = CredentialInfo {
        cn: cn.clone(),
        cert_pem,
        workspace_header,
    };

    for verifier in verifiers.iter() {
        if verifier.matches(&cn) {
            let authz = verifier
                .verify(&credential_info, &method)
                .await
                .map_err(Status::from)?;
            tracing::debug!("AuthzContext: {:?}", authz);
            req.extensions_mut().insert(authz);
            return Ok(req);
        }
    }

    Err(Status::unauthenticated(format!(
        "no verifier found for credential: {}",
        cn
    )))
}

impl<S> Service<Request> for AuthMiddleware<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let verifiers = self.verifiers.clone();
        let mut service = self.service.clone();

        Box::pin(async move {
            match authenticate(verifiers, request).await {
                Ok(req) => service.call(req).await,
                Err(status) => {
                    let response = status.into_http();
                    Ok(response)
                }
            }
        })
    }
}

fn extract_permission_from_method(method: &str) -> (&str, &str) {
    if method.contains("CreateSession") || method.contains("OpenSession") {
        return ("session", "create");
    }
    if method.contains("GetSession") || method.contains("ListSession") {
        return ("session", "read");
    }
    if method.contains("CloseSession") || method.contains("DeleteSession") {
        return ("session", "delete");
    }
    if method.contains("RegisterApplication") {
        return ("application", "create");
    }
    if method.contains("GetApplication") || method.contains("ListApplication") {
        return ("application", "read");
    }
    if method.contains("UpdateApplication") {
        return ("application", "update");
    }
    if method.contains("UnregisterApplication") {
        return ("application", "delete");
    }
    if method.contains("CreateTask") || method.contains("Task") {
        return ("session", "create");
    }
    if method.contains("GetTask") || method.contains("ListTask") || method.contains("WatchTask") {
        return ("session", "read");
    }
    if method.contains("DeleteTask") {
        return ("session", "delete");
    }
    if method.contains("Workspace") {
        return ("workspace", "*");
    }
    if method.contains("User") || method.contains("Role") {
        return ("admin", "*");
    }

    ("*", "*")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_permission_from_method() {
        assert_eq!(
            extract_permission_from_method("/flame.Frontend/CreateSession"),
            ("session", "create")
        );
        assert_eq!(
            extract_permission_from_method("/flame.Frontend/GetSession"),
            ("session", "read")
        );
        assert_eq!(
            extract_permission_from_method("/flame.Frontend/RegisterApplication"),
            ("application", "create")
        );
        assert_eq!(
            extract_permission_from_method("/flame.Admin/CreateUser"),
            ("admin", "*")
        );
    }
}

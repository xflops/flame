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
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, SignatureAlgorithm,
};
use thiserror::Error;
use x509_parser::prelude::*;

use common::authz::CredentialScope;

#[derive(Error, Debug)]
pub enum CertError {
    #[error("certificate generation failed: {0}")]
    GenerationFailed(String),

    #[error("certificate verification failed: {0}")]
    VerificationFailed(String),

    #[error("scope escalation not allowed: {0}")]
    ScopeEscalation(String),

    #[error("certificate expired")]
    CredentialExpired,

    #[error("invalid certificate: {0}")]
    InvalidCertificate(String),

    #[error("CA not configured")]
    CaNotConfigured,

    #[error("IO error: {0}")]
    IoError(String),
}

impl From<CertError> for common::FlameError {
    fn from(err: CertError) -> Self {
        match err {
            CertError::GenerationFailed(msg) => common::FlameError::Internal(msg),
            CertError::VerificationFailed(msg) => common::FlameError::InvalidState(msg),
            CertError::ScopeEscalation(msg) => common::FlameError::InvalidState(msg),
            CertError::CredentialExpired => {
                common::FlameError::InvalidState("credential expired".to_string())
            }
            CertError::InvalidCertificate(msg) => common::FlameError::InvalidConfig(msg),
            CertError::CaNotConfigured => {
                common::FlameError::InvalidConfig("CA not configured".to_string())
            }
            CertError::IoError(msg) => common::FlameError::Internal(msg),
        }
    }
}

pub struct IssueRequest {
    pub parent: String,
    pub parent_scope: CredentialScope,
    pub subject: String,
    pub workspace: String,
    pub scope: CredentialScope,
    pub ttl: Duration,
}

pub struct VerifiedClaims {
    pub subject: String,
    pub parent: Option<String>,
    pub workspace: String,
    pub scope: CredentialScope,
    pub expires_at: SystemTime,
}

pub struct SessionCredential {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
    pub ca_certificate: Vec<u8>,
    pub expires_at: SystemTime,
}

#[async_trait]
pub trait CertManager: Send + Sync {
    async fn issue(&self, request: IssueRequest) -> Result<SessionCredential, CertError>;
    async fn verify(&self, credential: &[u8]) -> Result<VerifiedClaims, CertError>;
}

pub struct CertManagerImpl {
    ca_cert_pem: Vec<u8>,
    ca_key_pem: Vec<u8>,
}

impl CertManagerImpl {
    pub fn new(ca_cert_path: &str, ca_key_path: &str) -> Result<Self, CertError> {
        let ca_cert_pem = std::fs::read(ca_cert_path)
            .map_err(|e| CertError::IoError(format!("failed to read CA cert: {}", e)))?;
        let ca_key_pem = std::fs::read(ca_key_path)
            .map_err(|e| CertError::IoError(format!("failed to read CA key: {}", e)))?;

        Ok(Self {
            ca_cert_pem,
            ca_key_pem,
        })
    }

    pub fn new_ptr(
        ca_cert_path: &str,
        ca_key_path: &str,
    ) -> Result<Arc<dyn CertManager>, CertError> {
        Ok(Arc::new(Self::new(ca_cert_path, ca_key_path)?))
    }

    fn build_common_name(&self, subject: &str, scope: CredentialScope, parent: &str) -> String {
        match scope {
            CredentialScope::User => format!("delegate:{}:{}", parent, subject),
            CredentialScope::Session => format!("session:{}", subject),
            CredentialScope::Unspecified => format!("session:{}", subject),
        }
    }

    fn build_san_uris(&self, request: &IssueRequest) -> Vec<String> {
        vec![
            format!("flame://workspace/{}", request.workspace),
            format!("flame://subject/{}", request.subject),
            format!("flame://parent/{}", request.parent),
            format!("flame://scope/{}", request.scope.as_str()),
        ]
    }
}

#[async_trait]
impl CertManager for CertManagerImpl {
    async fn issue(&self, request: IssueRequest) -> Result<SessionCredential, CertError> {
        if request.parent_scope == CredentialScope::Session
            && request.scope == CredentialScope::User
        {
            return Err(CertError::ScopeEscalation(
                "SESSION-scoped parent cannot issue USER-scoped credential".to_string(),
            ));
        }

        let ca_key = KeyPair::from_pem(&String::from_utf8_lossy(&self.ca_key_pem))
            .map_err(|e| CertError::GenerationFailed(format!("failed to parse CA key: {}", e)))?;

        let ca_cert_params =
            CertificateParams::from_ca_cert_pem(&String::from_utf8_lossy(&self.ca_cert_pem))
                .map_err(|e| {
                    CertError::GenerationFailed(format!("failed to parse CA cert: {}", e))
                })?;

        let ca_cert = ca_cert_params
            .self_signed(&ca_key)
            .map_err(|e| CertError::GenerationFailed(format!("failed to create CA cert: {}", e)))?;

        let subject_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| CertError::GenerationFailed(format!("failed to generate key: {}", e)))?;

        let cn = self.build_common_name(&request.subject, request.scope, &request.parent);
        let san_uris = self.build_san_uris(&request);

        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, cn);
        distinguished_name.push(DnType::OrganizationName, "Flame");

        let mut params = CertificateParams::new(vec![]).map_err(|e| {
            CertError::GenerationFailed(format!("failed to create cert params: {}", e))
        })?;

        params.distinguished_name = distinguished_name;
        params.not_before = rcgen::date_time_ymd(
            chrono::Utc::now()
                .format("%Y")
                .to_string()
                .parse()
                .unwrap_or(2024),
            chrono::Utc::now()
                .format("%m")
                .to_string()
                .parse()
                .unwrap_or(1),
            chrono::Utc::now()
                .format("%d")
                .to_string()
                .parse()
                .unwrap_or(1),
        );

        let expiry =
            chrono::Utc::now() + chrono::Duration::from_std(request.ttl).unwrap_or_default();
        params.not_after = rcgen::date_time_ymd(
            expiry.format("%Y").to_string().parse().unwrap_or(2024),
            expiry.format("%m").to_string().parse().unwrap_or(1),
            expiry.format("%d").to_string().parse().unwrap_or(1),
        );

        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];

        for uri in san_uris {
            params.subject_alt_names.push(SanType::URI(
                uri.try_into().map_err(|e| {
                    CertError::GenerationFailed(format!("invalid SAN URI: {:?}", e))
                })?,
            ));
        }

        let cert = params
            .signed_by(&subject_key, &ca_cert, &ca_key)
            .map_err(|e| CertError::GenerationFailed(format!("failed to sign cert: {}", e)))?;

        let expires_at = SystemTime::now() + request.ttl;

        Ok(SessionCredential {
            certificate: cert.pem().into_bytes(),
            private_key: subject_key.serialize_pem().into_bytes(),
            ca_certificate: self.ca_cert_pem.clone(),
            expires_at,
        })
    }

    async fn verify(&self, credential: &[u8]) -> Result<VerifiedClaims, CertError> {
        let pem = std::str::from_utf8(credential)
            .map_err(|e| CertError::InvalidCertificate(format!("invalid PEM encoding: {}", e)))?;

        let (_, pem_block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
            .map_err(|e| CertError::InvalidCertificate(format!("failed to parse PEM: {:?}", e)))?;

        let (_, cert) = X509Certificate::from_der(&pem_block.contents)
            .map_err(|e| CertError::InvalidCertificate(format!("failed to parse DER: {:?}", e)))?;

        let now = SystemTime::now();
        let not_after = cert.validity().not_after.to_datetime();
        let expires_at =
            SystemTime::UNIX_EPOCH + Duration::from_secs(not_after.unix_timestamp() as u64);

        if now > expires_at {
            return Err(CertError::CredentialExpired);
        }

        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
            .ok_or_else(|| CertError::InvalidCertificate("missing CN".to_string()))?
            .to_string();

        let mut workspace = String::new();
        let mut subject = String::new();
        let mut parent = None;
        let mut scope = CredentialScope::Session;

        if let Ok(Some(san_ext)) = cert.subject_alternative_name() {
            for name in &san_ext.value.general_names {
                if let x509_parser::extensions::GeneralName::URI(uri) = name {
                    if let Some(rest) = uri.strip_prefix("flame://workspace/") {
                        workspace = rest.to_string();
                    } else if let Some(rest) = uri.strip_prefix("flame://subject/") {
                        subject = rest.to_string();
                    } else if let Some(rest) = uri.strip_prefix("flame://parent/") {
                        parent = Some(rest.to_string());
                    } else if let Some(rest) = uri.strip_prefix("flame://scope/") {
                        scope = rest.parse().unwrap_or(CredentialScope::Session);
                    }
                }
            }
        }

        if subject.is_empty() {
            subject = if cn.starts_with("session:") {
                cn.strip_prefix("session:").unwrap_or(&cn).to_string()
            } else if cn.starts_with("delegate:") {
                cn.split(':').nth(2).unwrap_or(&cn).to_string()
            } else {
                cn.clone()
            };
        }

        Ok(VerifiedClaims {
            subject,
            parent,
            workspace,
            scope,
            expires_at,
        })
    }
}

pub struct NoCertManager;

#[async_trait]
impl CertManager for NoCertManager {
    async fn issue(&self, _request: IssueRequest) -> Result<SessionCredential, CertError> {
        Err(CertError::CaNotConfigured)
    }

    async fn verify(&self, _credential: &[u8]) -> Result<VerifiedClaims, CertError> {
        Err(CertError::CaNotConfigured)
    }
}

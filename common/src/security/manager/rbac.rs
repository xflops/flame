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

use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::Status;

use super::{SecurityManager, UserDelegation};
use crate::apis::{
    identity_kind, object_kind, operation, role_name, Object, Role, RoleRule, UserIdentity, ALL,
    ROOT_USER,
};
use crate::ctx::FlameTls;
use crate::security::delegation::DelegationManager;
use crate::storage::Storage;
use crate::FlameError;

/// Extract a Flame URI SAN from a TLS-verified X.509 leaf certificate.
fn peer_identity_from_der(der: &[u8], trust_domain: &str) -> Option<UserIdentity> {
    let uri = flame_uri_san(der)?;
    parse_identity(&uri, trust_domain)
}

fn parse_identity(uri: &str, trust_domain: &str) -> Option<UserIdentity> {
    let path = uri.strip_prefix("spiffe://")?;
    let (domain, path) = path.split_once('/')?;
    if domain != trust_domain {
        return None;
    }
    let path = path.strip_prefix("flame/")?;
    let (role, id) = path.split_once('/')?;
    match role {
        identity_kind::USER if !id.contains('/') => Some(UserIdentity::TenantUser(id.to_owned())),
        identity_kind::SYSTEM => {
            let (kind, name) = id.split_once('/')?;
            if name.contains('/') {
                return None;
            }
            match kind {
                identity_kind::NODE => Some(UserIdentity::SystemNode(name.to_owned())),
                identity_kind::CACHE => Some(UserIdentity::SystemCache(name.to_owned())),
                _ => None,
            }
        }
        _ => None,
    }
}

// Small, strict DER reader for the one X.509 extension we need. It refuses
// indefinite lengths and trailing data. TLS performs certificate validation.
fn item(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n || rest[0] == 0 {
            return None;
        }
        let mut len = 0usize;
        for byte in &rest[..n] {
            len = len.checked_mul(256)?.checked_add(usize::from(*byte))?;
        }
        if len < 128 {
            return None;
        }
        (len, &rest[n..])
    };
    if len > rest.len() {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

fn one(input: &[u8], tag: u8) -> Option<&[u8]> {
    let (actual, body, tail) = item(input)?;
    if actual != tag || !tail.is_empty() {
        return None;
    }
    Some(body)
}

fn flame_uri_san(der: &[u8]) -> Option<String> {
    let certificate = one(der, 0x30)?;
    let (tag, tbs, _) = item(certificate)?;
    if tag != 0x30 {
        return None;
    }
    let mut fields = tbs;
    let mut extensions = None;
    while !fields.is_empty() {
        let (tag, body, rest) = item(fields)?;
        if tag == 0xa3 {
            if extensions.is_some() {
                return None;
            }
            extensions = Some(one(body, 0x30)?);
        }
        fields = rest;
    }
    let mut extensions = extensions?;
    let mut found = None;
    while !extensions.is_empty() {
        let (tag, extension, rest) = item(extensions)?;
        if tag != 0x30 {
            return None;
        }
        let (tag, oid, rest_of_extension) = item(extension)?;
        if tag != 0x06 {
            return None;
        }
        if oid == [0x55, 0x1d, 0x11] {
            // subjectAltName, 2.5.29.17
            if found.is_some() {
                return None;
            }
            let (tag, body, rest) = item(rest_of_extension)?;
            let octet = if tag == 0x01 {
                let (tag, body, tail) = item(rest)?;
                if tag != 0x04 || !tail.is_empty() {
                    return None;
                }
                body
            } else {
                if tag != 0x04 || !rest.is_empty() {
                    return None;
                }
                body
            };
            let mut names = one(octet, 0x30)?;
            let mut uri = None;
            while !names.is_empty() {
                let (tag, body, rest) = item(names)?;
                if tag == 0x86 {
                    let value = std::str::from_utf8(body).ok()?;
                    if value.starts_with("spiffe://") && value.contains("/flame/") {
                        if uri.is_some() {
                            return None;
                        }
                        uri = Some(value.to_owned());
                    }
                }
                names = rest;
            }
            found = uri;
        }
        extensions = rest;
    }
    found
}

/// Role subjects are tenant users only.
fn role_subject_matches(grant: &str, subject: &str) -> bool {
    grant == subject || grant == ALL
}

fn system_allows(actor: &UserIdentity, operation: &str, object: Object<'_>) -> bool {
    match actor {
        UserIdentity::SystemNode(node) => {
            matches!(object, Object::Node(id) if id == node)
                && matches!(operation, operation::UPDATE | operation::DELETE)
        }
        UserIdentity::SystemCache(_) => {
            matches!(object, Object::Application(_)) && operation == operation::VIEW
        }
        UserIdentity::TenantUser(_) => false,
    }
}

fn grants_for<'a>(role: &'a Role, kind: &str) -> impl Iterator<Item = &'a RoleRule> {
    role.rules
        .get(kind)
        .into_iter()
        .flatten()
        .chain(role.rules.get(ALL).into_iter().flatten())
}

fn role_allows(roles: &[Role], subject: &str, operation: &str, target: Object<'_>) -> bool {
    let (kind, object_id) = match target {
        Object::Application(id) => (object_kind::APPLICATION, id),
        Object::Node(id) => (object_kind::NODE, id),
    };
    roles.iter().any(|role| {
        if matches!(target, Object::Node(_))
            && matches!(operation, operation::VIEW | operation::LIST)
            && role.name != role_name::ADMIN
        {
            return false;
        }
        role.users
            .iter()
            .any(|grant| role_subject_matches(grant, subject))
            && grants_for(role, kind).any(|rule| {
                rule.operations
                    .iter()
                    .any(|verb| verb == operation || verb == ALL)
                    && (rule.object_id == ALL || rule.object_id == object_id)
            })
    })
}

pub struct RBACSecManager {
    storage: Option<Arc<Storage>>,
    tls: FlameTls,
    trust_domain: String,
    delegation: Option<DelegationManager>,
}

fn default_admin_role() -> Role {
    Role {
        name: role_name::ADMIN.to_string(),
        users: vec![ROOT_USER.to_string()],
        rules: std::collections::HashMap::from([(
            ALL.to_string(),
            vec![RoleRule {
                object_id: ALL.to_string(),
                operations: vec![ALL.to_string()],
            }],
        )]),
    }
}

impl RBACSecManager {
    pub fn new(tls: FlameTls, trust_domain: String) -> Self {
        Self {
            storage: None,
            tls,
            trust_domain,
            delegation: None,
        }
    }

    pub fn with_delegation(mut self, domain: &str) -> Result<Self, FlameError> {
        self.delegation = Some(DelegationManager::from_tls(&self.tls, domain)?);
        Ok(self)
    }

    pub async fn with_role(mut self, storage: Arc<Storage>) -> Result<Self, FlameError> {
        let role = default_admin_role();
        if storage.get_role(&role.name)?.is_none() {
            storage.set_role(&role)?;
        }
        self.storage = Some(storage);
        Ok(self)
    }

    fn identity_from_token(&self, token: &str) -> Result<UserIdentity, Status> {
        let signer = self
            .delegation
            .as_ref()
            .ok_or_else(|| Status::unauthenticated("identity delegation unavailable"))?;
        let (encoded_data, signature) = token
            .split_once('.')
            .ok_or_else(|| Status::unauthenticated("invalid identity token"))?;
        let data = URL_SAFE_NO_PAD
            .decode(encoded_data)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| Status::unauthenticated("invalid identity token"))?;
        if !signer.verify(signature, &data) {
            return Err(Status::unauthenticated("invalid identity token"));
        }
        Ok(UserDelegation::from_json(&data)?.into_identity())
    }
}

#[async_trait]
impl SecurityManager for RBACSecManager {
    fn with_delegation(
        self: Box<Self>,
        domain: &str,
    ) -> Result<Box<dyn SecurityManager>, FlameError> {
        Ok(Box::new((*self).with_delegation(domain)?))
    }

    async fn with_role(
        self: Box<Self>,
        storage: Arc<Storage>,
    ) -> Result<Box<dyn SecurityManager>, FlameError> {
        Ok(Box::new((*self).with_role(storage).await?))
    }

    fn storage(&self) -> Option<&Arc<Storage>> {
        self.storage.as_ref()
    }

    fn identify(
        &self,
        peer: Option<&TlsConnectInfo<TcpConnectInfo>>,
        token: Option<&str>,
    ) -> Result<Option<UserIdentity>, Status> {
        if let Some(token) = token {
            return self.identity_from_token(token).map(Some);
        }
        let Some(certs) = peer.and_then(TlsConnectInfo::peer_certs) else {
            return Ok(None);
        };
        let leaf = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("verified client certificate required"))?;
        peer_identity_from_der(leaf.as_ref(), &self.trust_domain)
            .map(Some)
            .ok_or_else(|| Status::unauthenticated("invalid Flame certificate identity"))
    }

    fn delegate(&self, identity: &UserIdentity) -> Result<String, Status> {
        let data = UserDelegation::new(identity)?.to_json()?;
        let signer = self
            .delegation
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("identity delegation unavailable"))?;
        let signature = signer.sign(&data).map_err(Status::from)?;
        Ok(format!("{}.{}", URL_SAFE_NO_PAD.encode(data), signature))
    }

    fn sign(&self, data: &str) -> Result<String, Status> {
        self.delegation
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("delegation unavailable"))?
            .sign(data)
            .map_err(Status::from)
    }

    fn verify(&self, signature: &str, data: &str) -> bool {
        self.delegation
            .as_ref()
            .is_some_and(|signer| signer.verify(signature, data))
    }

    async fn authorize(
        &self,
        actor: &Option<UserIdentity>,
        operation: &str,
        object: Object<'_>,
    ) -> Result<(), Status> {
        let allowed = match actor.as_ref() {
            None => false,
            Some(UserIdentity::TenantUser(subject)) => {
                let roles = self.list_roles()?;
                role_allows(&roles, subject, operation, object)
            }
            Some(actor) => system_allows(actor, operation, object),
        };
        if allowed {
            Ok(())
        } else if actor.is_none() {
            Err(Status::unauthenticated(
                "valid delegation or client identity required",
            ))
        } else {
            Err(Status::not_found("resource not found"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{FlameCluster, FlameClusterContext};
    use crate::storage;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509Name, X509};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn test_delegation_tls() -> (TempDir, FlameTls) {
        let dir = tempfile::tempdir().unwrap();
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509Name::builder().unwrap();
        name.append_entry_by_text("CN", "cache-test").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
            .unwrap();
        cert.set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert_file = dir.path().join("cache.crt");
        let key_file = dir.path().join("cache.key");
        std::fs::write(&cert_file, cert.build().to_pem().unwrap()).unwrap();
        std::fs::write(&key_file, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        let tls = FlameTls {
            cert_file: cert_file.to_string_lossy().into_owned(),
            key_file: key_file.to_string_lossy().into_owned(),
            ca_file: None,
        };
        (dir, tls)
    }

    #[test]
    fn delegation_token_recovers_original_tenant_identity() {
        let (_dir, tls) = test_delegation_tls();
        let manager = RBACSecManager::new(tls.clone(), "cluster".to_string())
            .with_delegation("cache-v1")
            .unwrap();
        let alice = UserIdentity::TenantUser("alice".to_string());
        let token = manager.delegate(&alice).unwrap();
        let (encoded_data, _) = token.split_once('.').unwrap();
        let data = URL_SAFE_NO_PAD.decode(encoded_data).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(payload["name"], "alice");
        assert!(payload["signed_at"].as_u64().unwrap() > 0);
        assert_eq!(manager.identify(None, Some(&token)).unwrap(), Some(alice));
        assert_eq!(manager.identify(None, None).unwrap(), None);
        assert_eq!(
            manager
                .identify(None, Some(&format!("{token}x")))
                .unwrap_err()
                .code(),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            RBACSecManager::new(tls, "cluster".to_string())
                .with_delegation("other-v1")
                .unwrap()
                .identify(None, Some(&token))
                .unwrap_err()
                .code(),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            manager
                .delegate(&UserIdentity::SystemNode("node-a".to_string()))
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
    }

    #[tokio::test]
    async fn setup_seeds_root_admin_role_once() {
        let context = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = storage::new_ptr(&context).await.unwrap();
        let manager = RBACSecManager::new(FlameTls::default(), "cluster".to_string())
            .with_role(Arc::clone(&storage))
            .await
            .unwrap();
        let admin = storage.get_role(role_name::ADMIN).unwrap().unwrap();
        assert_eq!(admin, default_admin_role());
        assert!(manager
            .authorize(
                &Some(UserIdentity::TenantUser(ROOT_USER.to_string())),
                operation::VIEW,
                Object::Application("app"),
            )
            .await
            .is_ok());

        let custom = Role {
            users: vec!["alice".to_string()],
            ..admin
        };
        storage.set_role(&custom).unwrap();
        RBACSecManager::new(FlameTls::default(), "cluster".to_string())
            .with_role(Arc::clone(&storage))
            .await
            .unwrap();
        assert_eq!(storage.get_role(role_name::ADMIN).unwrap(), Some(custom));
    }

    #[tokio::test]
    async fn setup_preserves_loaded_admin_role() {
        let directory = tempfile::tempdir().unwrap();
        let context = FlameClusterContext {
            cluster: FlameCluster {
                storage: format!("fs://{}", directory.path().join("roles").display()),
                ..Default::default()
            },
            ..Default::default()
        };
        let custom = Role {
            users: vec!["alice".to_string()],
            ..default_admin_role()
        };
        {
            let storage = storage::new_ptr(&context).await.unwrap();
            storage.set_role(&custom).unwrap();
        }

        let recovered = storage::new_ptr(&context).await.unwrap();
        recovered.load_data().await.unwrap();
        RBACSecManager::new(FlameTls::default(), "cluster".to_string())
            .with_role(Arc::clone(&recovered))
            .await
            .unwrap();
        assert_eq!(recovered.get_role(role_name::ADMIN).unwrap(), Some(custom));
    }

    #[test]
    fn node_objects_have_distinct_selectors() {
        let role = Role {
            name: role_name::ADMIN.to_string(),
            users: vec!["alice".to_string()],
            rules: HashMap::from([(
                object_kind::NODE.to_string(),
                vec![RoleRule {
                    object_id: "node-a".to_string(),
                    operations: vec![operation::VIEW.to_string(), operation::LIST.to_string()],
                }],
            )]),
        };
        assert!(role_allows(
            std::slice::from_ref(&role),
            "alice",
            operation::VIEW,
            Object::Node("node-a")
        ));
        assert!(!role_allows(
            std::slice::from_ref(&role),
            "alice",
            operation::VIEW,
            Object::Node("node-b")
        ));
        assert!(!role_allows(
            &[role],
            "alice",
            operation::LIST,
            Object::Node(ALL)
        ));
    }

    #[test]
    fn application_grants_combine_exact_and_wildcard_rules() {
        let app_a = "nodes";
        let app_b = "app-b";
        let roles = vec![
            Role {
                name: "viewer".to_string(),
                users: vec!["alice".to_string()],
                rules: HashMap::from([(
                    object_kind::APPLICATION.to_string(),
                    vec![RoleRule {
                        object_id: app_a.to_string(),
                        operations: vec![operation::VIEW.to_string()],
                    }],
                )]),
            },
            Role {
                name: "lister".to_string(),
                users: vec!["alice".to_string()],
                rules: HashMap::from([(
                    object_kind::APPLICATION.to_string(),
                    vec![RoleRule {
                        object_id: app_b.to_string(),
                        operations: vec![operation::LIST.to_string()],
                    }],
                )]),
            },
        ];
        assert!(role_allows(
            &roles,
            "alice",
            operation::VIEW,
            Object::Application(app_a),
        ));
        assert!(!role_allows(
            &roles,
            "alice",
            operation::VIEW,
            Object::Application(app_b),
        ));
        assert!(role_allows(
            &roles,
            "alice",
            operation::LIST,
            Object::Application(app_b),
        ));
        assert!(!role_allows(
            &roles,
            "bob",
            operation::VIEW,
            Object::Application(app_a),
        ));
        let mut roles = roles;
        roles.push(Role {
            name: "global-viewer".to_string(),
            users: vec!["alice".to_string()],
            rules: HashMap::from([(
                ALL.to_string(),
                vec![RoleRule {
                    object_id: ALL.to_string(),
                    operations: vec![operation::VIEW.to_string()],
                }],
            )]),
        });
        assert!(role_allows(
            &roles,
            "alice",
            operation::VIEW,
            Object::Application(app_b),
        ));
        assert!(!role_allows(
            &roles,
            "alice",
            operation::LIST,
            Object::Application(app_a),
        ));
    }

    #[test]
    fn operations_remain_attached_to_each_object_id() {
        let role = Role {
            name: "editor".to_string(),
            users: vec!["alice".to_string()],
            rules: HashMap::from([(
                object_kind::APPLICATION.to_string(),
                vec![
                    RoleRule {
                        object_id: "app-a".to_string(),
                        operations: vec![operation::VIEW.to_string()],
                    },
                    RoleRule {
                        object_id: "app-b".to_string(),
                        operations: vec![operation::DELETE.to_string()],
                    },
                ],
            )]),
        };
        assert!(role_allows(
            std::slice::from_ref(&role),
            "alice",
            operation::VIEW,
            Object::Application("app-a"),
        ));
        assert!(!role_allows(
            std::slice::from_ref(&role),
            "alice",
            operation::DELETE,
            Object::Application("app-a"),
        ));
        assert!(role_allows(
            std::slice::from_ref(&role),
            "alice",
            operation::DELETE,
            Object::Application("app-b"),
        ));
        assert!(!role_allows(
            &[role],
            "alice",
            operation::VIEW,
            Object::Application("app-b"),
        ));
    }

    #[test]
    fn node_selector_does_not_grant_application_named_nodes() {
        let role = Role {
            name: role_name::ADMIN.to_string(),
            users: vec!["alice".to_string()],
            rules: HashMap::from([(
                object_kind::NODE.to_string(),
                vec![RoleRule {
                    object_id: ALL.to_string(),
                    operations: vec![operation::VIEW.to_string()],
                }],
            )]),
        };
        assert!(!role_allows(
            std::slice::from_ref(&role),
            "alice",
            operation::VIEW,
            Object::Application("nodes"),
        ));
        assert!(role_allows(
            &[role],
            "alice",
            operation::VIEW,
            Object::Node("node-a"),
        ));
    }
    fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if contents.len() < 128 {
            out.push(contents.len() as u8);
        } else {
            out.extend([0x81, contents.len() as u8]);
        }
        out.extend(contents);
        out
    }

    fn certificate_with_uris(uris: &[&str]) -> Vec<u8> {
        let names = uris
            .iter()
            .flat_map(|uri| der(0x86, uri.as_bytes()))
            .collect::<Vec<_>>();
        let extension = [
            der(0x06, &[0x55, 0x1d, 0x11]),
            der(0x04, &der(0x30, &names)),
        ]
        .concat();
        let extensions = der(0xa3, &der(0x30, &der(0x30, &extension)));
        der(0x30, &der(0x30, &extensions))
    }

    #[test]
    fn identity_requires_exact_domain_and_path() {
        let id = "00000000-0000-4000-8000-000000000001";
        assert_eq!(
            parse_identity(&format!("spiffe://cluster/flame/user/{id}"), "cluster"),
            Some(UserIdentity::TenantUser(id.to_owned()))
        );
        assert!(parse_identity(&format!("spiffe://cluster/flame/admin/{id}"), "cluster").is_none());
        assert!(parse_identity(&format!("spiffe://other/flame/user/{id}"), "cluster").is_none());
        assert_eq!(
            parse_identity("spiffe://cluster/flame/user/root", "cluster"),
            Some(UserIdentity::TenantUser("root".to_string()))
        );
        assert_eq!(
            parse_identity("spiffe://cluster/flame/system/cache/cache-1", "cluster"),
            Some(UserIdentity::SystemCache("cache-1".to_string()))
        );
        assert_eq!(
            parse_identity("spiffe://cluster/flame/system/node/node-1", "cluster"),
            Some(UserIdentity::SystemNode("node-1".to_string()))
        );
        assert_eq!(
            UserIdentity::SystemNode("node-1".to_string()).username(),
            "system:node:node-1"
        );
        assert_eq!(
            UserIdentity::SystemCache("cache-1".to_string()).username(),
            "system:cache:cache-1"
        );
        assert!(parse_identity("spiffe://cluster/flame/user/alice/bob", "cluster").is_none());
        assert_eq!(
            parse_identity("spiffe://cluster/flame/user/system:node:node-1", "cluster"),
            Some(UserIdentity::TenantUser("system:node:node-1".to_string()))
        );
        assert!(
            parse_identity("spiffe://cluster/flame/executor-manager/node/a", "cluster").is_none()
        );
        assert!(parse_identity("spiffe://cluster/flame/object-cache/cache-1", "cluster").is_none());
    }

    #[test]
    fn san_reader_rejects_ambiguous_flame_identities() {
        let first = "spiffe://cluster/flame/user/00000000-0000-4000-8000-000000000001";
        let second = "spiffe://cluster/flame/user/00000000-0000-4000-8000-000000000002";
        assert_eq!(
            flame_uri_san(&certificate_with_uris(&[first])),
            Some(first.to_string())
        );
        assert_eq!(
            flame_uri_san(&certificate_with_uris(&[first, second])),
            None
        );
        assert_eq!(flame_uri_san(&certificate_with_uris(&[])), None);
    }
}

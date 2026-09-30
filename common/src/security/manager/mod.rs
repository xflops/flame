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
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::Status;

use crate::apis::{role_name, Object, Role, UserDelegation, UserIdentity};
use crate::ctx::FlameSecurity;
use crate::storage::Storage;
use crate::FlameError;

mod none;
mod rbac;

pub use none::NoneSecManager;
pub use rbac::RBACSecManager;

pub fn new(security: Option<&FlameSecurity>) -> Box<dyn SecurityManager> {
    match security {
        Some(config) => Box::new(RBACSecManager::new(
            config.tls.clone(),
            config.trust_domain.clone(),
        )),
        None => Box::new(NoneSecManager),
    }
}

#[async_trait]
pub trait SecurityManager: Send + Sync {
    fn with_delegation(
        self: Box<Self>,
        domain: &str,
    ) -> Result<Box<dyn SecurityManager>, FlameError>;
    async fn with_role(
        self: Box<Self>,
        storage: Arc<Storage>,
    ) -> Result<Box<dyn SecurityManager>, FlameError>;
    fn storage(&self) -> Option<&Arc<Storage>>;
    fn identify(
        &self,
        peer: Option<&TlsConnectInfo<TcpConnectInfo>>,
        token: Option<&str>,
    ) -> Result<Option<UserIdentity>, Status>;
    fn delegate(&self, identity: &UserIdentity) -> Result<String, Status>;
    fn sign(&self, _: &str) -> Result<String, Status> {
        Err(Status::failed_precondition("delegation unavailable"))
    }
    fn verify(&self, _: &str, _: &str) -> bool {
        false
    }
    async fn authorize(
        &self,
        actor: &Option<UserIdentity>,
        operation: &str,
        object: Object<'_>,
    ) -> Result<(), Status>;

    fn get_role(&self, name: &str) -> Result<Option<Role>, Status> {
        self.storage()
            .ok_or_else(|| Status::failed_precondition("Role storage unavailable"))?
            .get_role(name)
            .map_err(Status::from)
    }

    fn set_role(&self, role: &Role) -> Result<(), Status> {
        self.storage()
            .ok_or_else(|| Status::failed_precondition("Role storage unavailable"))?
            .set_role(role)
            .map_err(Status::from)
    }

    fn delete_role(&self, name: &str) -> Result<(), Status> {
        if name == role_name::ADMIN {
            return Err(Status::failed_precondition("cannot delete built-in role"));
        }
        self.storage()
            .ok_or_else(|| Status::failed_precondition("Role storage unavailable"))?
            .delete_role(name)
            .map_err(Status::from)
    }

    fn list_roles(&self) -> Result<Vec<Role>, Status> {
        self.storage()
            .ok_or_else(|| Status::failed_precondition("Role storage unavailable"))?
            .list_roles()
            .map_err(Status::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apis::{object_kind, operation, RoleRule, ALL};
    use crate::ctx::{FlameCluster, FlameClusterContext};
    use crate::storage;

    async fn test_storage() -> Arc<Storage> {
        let context = FlameClusterContext {
            cluster: FlameCluster {
                storage: "none".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        storage::new_ptr(&context).await.unwrap()
    }

    #[tokio::test]
    async fn application_and_system_permissions() {
        let manager = RBACSecManager::new(Default::default(), "cluster".to_string())
            .with_role(test_storage().await)
            .await
            .unwrap();
        manager
            .set_role(&Role {
                name: "reader".to_string(),
                users: vec!["alice".to_string()],
                rules: std::collections::HashMap::from([(
                    object_kind::APPLICATION.to_string(),
                    vec![RoleRule {
                        object_id: "app-a".to_string(),
                        operations: vec![operation::VIEW.to_string()],
                    }],
                )]),
            })
            .unwrap();

        let tenant = Some(UserIdentity::TenantUser("alice".to_string()));
        assert!(manager
            .authorize(&tenant, operation::VIEW, Object::Application("app-a"))
            .await
            .is_ok());
        assert!(manager
            .authorize(&tenant, operation::VIEW, Object::Application("app-b"))
            .await
            .is_err());
        assert!(manager
            .authorize(&tenant, ALL, Object::Application(ALL))
            .await
            .is_err());

        let cache = Some(UserIdentity::SystemCache("cache-a".to_string()));
        assert!(manager
            .authorize(&cache, operation::VIEW, Object::Application(ALL))
            .await
            .is_ok());
        assert!(manager
            .authorize(&cache, operation::VIEW, Object::Application("app-a"))
            .await
            .is_ok());
        assert!(manager
            .authorize(&cache, ALL, Object::Application(ALL))
            .await
            .is_err());
        assert!(manager
            .authorize(&cache, operation::LIST, Object::Application(ALL))
            .await
            .is_err());

        let node = Some(UserIdentity::SystemNode("node-a".to_string()));
        assert!(manager
            .authorize(&node, operation::VIEW, Object::Application("app-a"))
            .await
            .is_err());
        assert!(manager
            .authorize(&node, ALL, Object::Application(ALL))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn builtin_role_cannot_be_deleted() {
        let manager = RBACSecManager::new(Default::default(), "cluster".to_string())
            .with_role(test_storage().await)
            .await
            .unwrap();
        assert_eq!(
            manager.delete_role(role_name::ADMIN).unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
    }
}

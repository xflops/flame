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

use super::{SecurityManager, UserDelegation};
use crate::apis::{Object, UserIdentity, ROOT_USER};
use crate::storage::Storage;
use crate::FlameError;

#[derive(Default)]
pub struct NoneSecManager;

#[async_trait]
impl SecurityManager for NoneSecManager {
    fn with_delegation(self: Box<Self>, _: &str) -> Result<Box<dyn SecurityManager>, FlameError> {
        Ok(self)
    }

    async fn with_role(
        self: Box<Self>,
        _: Arc<Storage>,
    ) -> Result<Box<dyn SecurityManager>, FlameError> {
        Ok(self)
    }

    fn storage(&self) -> Option<&Arc<Storage>> {
        None
    }

    fn identify(
        &self,
        _: Option<&TlsConnectInfo<TcpConnectInfo>>,
        token: Option<&str>,
    ) -> Result<Option<UserIdentity>, Status> {
        if let Some(data) = token {
            return Ok(Some(UserDelegation::from_json(data)?.into_identity()));
        }
        Ok(Some(UserIdentity::TenantUser(ROOT_USER.to_string())))
    }

    fn delegate(&self, identity: &UserIdentity) -> Result<String, Status> {
        UserDelegation::new(identity)?.to_json()
    }

    fn sign(&self, data: &str) -> Result<String, Status> {
        Ok(data.to_string())
    }

    fn verify(&self, signature: &str, data: &str) -> bool {
        signature == data
    }

    async fn authorize(
        &self,
        _: &Option<UserIdentity>,
        _: &str,
        _: Object<'_>,
    ) -> Result<(), Status> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delegated_username_is_recovered_without_role_storage() {
        let manager = NoneSecManager;
        let alice = UserIdentity::TenantUser("alice".to_string());
        let token = manager.delegate(&alice).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&token).unwrap();
        assert_eq!(payload["name"], "alice");
        assert!(payload["signed_at"].as_u64().unwrap() > 0);
        assert_eq!(manager.identify(None, Some(&token)).unwrap(), Some(alice));
        assert!(manager.storage().is_none());
        assert_eq!(
            manager
                .identify(None, Some("invalid/user"))
                .unwrap_err()
                .code(),
            tonic::Code::Unauthenticated
        );
    }
}

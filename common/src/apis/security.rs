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

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_derive::{Deserialize, Serialize};
use tonic::Status;

pub const ALL: &str = "*";
pub const NODE_PREFIX: &str = "node:";
pub const APPLICATION_PREFIX: &str = "application:";
pub const ROOT_USER: &str = "root";

pub mod object_kind {
    pub const APPLICATION: &str = "application";
    pub const NODE: &str = "node";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object<'a> {
    Application(&'a str),
    Node(&'a str),
}

pub mod identity_kind {
    pub const USER: &str = "user";
    pub const SYSTEM: &str = "system";
    pub const NODE: &str = "node";
    pub const CACHE: &str = "cache";
}

pub mod system_user {
    pub const NODE: &str = "system:node";
    pub const CACHE: &str = "system:cache";
}

pub mod operation {
    pub const VIEW: &str = "view";
    pub const LIST: &str = "list";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
}

pub mod role_name {
    pub const ADMIN: &str = "Admin";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleRule {
    pub object_id: String,
    pub operations: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub rules: HashMap<String, Vec<RoleRule>>,
    pub users: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserDelegation {
    pub name: String,
    pub signed_at: u64,
}

impl UserDelegation {
    pub(crate) fn new(identity: &UserIdentity) -> Result<Self, Status> {
        let UserIdentity::TenantUser(name) = identity else {
            return Err(Status::permission_denied("tenant identity required"));
        };
        let signed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Status::internal("system clock before Unix epoch"))?
            .as_secs();
        Ok(Self {
            name: name.clone(),
            signed_at,
        })
    }

    pub(crate) fn from_json(data: &str) -> Result<Self, Status> {
        serde_json::from_str(data).map_err(|_| Status::unauthenticated("invalid identity token"))
    }

    pub(crate) fn to_json(&self) -> Result<String, Status> {
        serde_json::to_string(self).map_err(|_| Status::internal("failed to encode identity"))
    }

    pub(crate) fn into_identity(self) -> UserIdentity {
        UserIdentity::TenantUser(self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserIdentity {
    TenantUser(String),
    SystemNode(String),
    SystemCache(String),
}

impl UserIdentity {
    pub fn username(&self) -> String {
        match self {
            Self::TenantUser(id) => id.clone(),
            Self::SystemNode(node) => format!("{}:{node}", system_user::NODE),
            Self::SystemCache(cache) => format!("{}:{cache}", system_user::CACHE),
        }
    }

    pub fn tenant_subject(&self) -> Option<&str> {
        match self {
            Self::TenantUser(id) => Some(id),
            Self::SystemNode(_) | Self::SystemCache(_) => None,
        }
    }
}

/// Tenant IDs and system resource names occupy one URI path segment.
pub fn valid_subject_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

/// Role membership names tenant users only. System permissions are predefined.
pub fn valid_role_subject(subject: &str) -> bool {
    subject == ALL || valid_subject_id(subject)
}

pub fn valid_role_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn valid_role_object(kind: &str, object_id: &str) -> bool {
    match kind {
        ALL => object_id == ALL,
        object_kind::APPLICATION => valid_subject_id(object_id),
        object_kind::NODE => object_id == ALL || valid_subject_id(object_id),
        _ => false,
    }
}

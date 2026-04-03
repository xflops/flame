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

use std::fmt;

use thiserror::Error;
use tonic::Status;

/// Credential scope determines what level of access a session has.
/// USER scope has full user permissions; SESSION scope is limited to session-specific operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CredentialScope {
    #[default]
    Unspecified,
    User,
    Session,
}

impl CredentialScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            CredentialScope::Unspecified => "unspecified",
            CredentialScope::User => "user",
            CredentialScope::Session => "session",
        }
    }
}

impl fmt::Display for CredentialScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for CredentialScope {
    type Err = AuthzError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "user" => Ok(CredentialScope::User),
            "session" => Ok(CredentialScope::Session),
            "unspecified" | "" => Ok(CredentialScope::Unspecified),
            _ => Err(AuthzError::InvalidCredentialScope(s.to_string())),
        }
    }
}

impl From<rpc::flame::CredentialScope> for CredentialScope {
    fn from(scope: rpc::flame::CredentialScope) -> Self {
        match scope {
            rpc::flame::CredentialScope::Unspecified => CredentialScope::Unspecified,
            rpc::flame::CredentialScope::User => CredentialScope::User,
            rpc::flame::CredentialScope::Session => CredentialScope::Session,
        }
    }
}

impl From<CredentialScope> for rpc::flame::CredentialScope {
    fn from(scope: CredentialScope) -> Self {
        match scope {
            CredentialScope::Unspecified => rpc::flame::CredentialScope::Unspecified,
            CredentialScope::User => rpc::flame::CredentialScope::User,
            CredentialScope::Session => rpc::flame::CredentialScope::Session,
        }
    }
}

impl From<i32> for CredentialScope {
    fn from(value: i32) -> Self {
        match value {
            1 => CredentialScope::User,
            2 => CredentialScope::Session,
            _ => CredentialScope::Unspecified,
        }
    }
}

impl From<CredentialScope> for i32 {
    fn from(scope: CredentialScope) -> Self {
        match scope {
            CredentialScope::Unspecified => 0,
            CredentialScope::User => 1,
            CredentialScope::Session => 2,
        }
    }
}

/// Authorization context passed through request extensions.
/// Contains the authenticated subject and their workspace/scope.
#[derive(Clone, Debug)]
pub struct AuthzContext {
    pub subject: String,
    pub workspace: String,
    pub scope: CredentialScope,
    pub parent: Option<String>,
}

impl AuthzContext {
    pub fn new(subject: String, workspace: String) -> Self {
        Self {
            subject,
            workspace,
            scope: CredentialScope::User,
            parent: None,
        }
    }

    pub fn with_scope(mut self, scope: CredentialScope) -> Self {
        self.scope = scope;
        self
    }

    pub fn with_parent(mut self, parent: String) -> Self {
        self.parent = Some(parent);
        self
    }

    pub fn is_session_scoped(&self) -> bool {
        self.scope == CredentialScope::Session
    }

    pub fn is_user_scoped(&self) -> bool {
        self.scope == CredentialScope::User
    }
}

impl Default for AuthzContext {
    fn default() -> Self {
        Self {
            subject: String::new(),
            workspace: crate::rbac::WORKSPACE_DEFAULT.to_string(),
            scope: CredentialScope::Unspecified,
            parent: None,
        }
    }
}

#[derive(Error, Debug)]
pub enum AuthzError {
    #[error("user not found: {0}")]
    UserNotFound(String),

    #[error("user disabled: {0}")]
    UserDisabled(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    #[error("invalid workspace: {0}")]
    InvalidWorkspace(String),

    #[error("invalid credential scope: {0}")]
    InvalidCredentialScope(String),

    #[error("scope escalation not allowed: {0}")]
    ScopeEscalation(String),

    #[error("certificate error: {0}")]
    CertificateError(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<AuthzError> for Status {
    fn from(err: AuthzError) -> Self {
        match err {
            AuthzError::UserNotFound(_) | AuthzError::Unauthenticated(_) => {
                Status::unauthenticated(err.to_string())
            }
            AuthzError::UserDisabled(_)
            | AuthzError::PermissionDenied(_)
            | AuthzError::ScopeEscalation(_) => Status::permission_denied(err.to_string()),
            AuthzError::InvalidWorkspace(_) | AuthzError::InvalidCredentialScope(_) => {
                Status::invalid_argument(err.to_string())
            }
            AuthzError::CertificateError(_) | AuthzError::Internal(_) => {
                Status::internal(err.to_string())
            }
        }
    }
}

impl From<AuthzError> for crate::FlameError {
    fn from(err: AuthzError) -> Self {
        match err {
            AuthzError::UserNotFound(msg) => crate::FlameError::NotFound(msg),
            AuthzError::UserDisabled(msg) | AuthzError::PermissionDenied(msg) => {
                crate::FlameError::InvalidState(msg)
            }
            AuthzError::Unauthenticated(msg) => crate::FlameError::InvalidConfig(msg),
            AuthzError::InvalidWorkspace(msg) | AuthzError::InvalidCredentialScope(msg) => {
                crate::FlameError::InvalidConfig(msg)
            }
            AuthzError::ScopeEscalation(msg) => crate::FlameError::InvalidState(msg),
            AuthzError::CertificateError(msg) | AuthzError::Internal(msg) => {
                crate::FlameError::Internal(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credential_scope_from_str() {
        assert_eq!(
            "user".parse::<CredentialScope>().unwrap(),
            CredentialScope::User
        );
        assert_eq!(
            "session".parse::<CredentialScope>().unwrap(),
            CredentialScope::Session
        );
        assert_eq!(
            "USER".parse::<CredentialScope>().unwrap(),
            CredentialScope::User
        );
        assert_eq!(
            "".parse::<CredentialScope>().unwrap(),
            CredentialScope::Unspecified
        );
        assert!("invalid".parse::<CredentialScope>().is_err());
    }

    #[test]
    fn test_credential_scope_from_i32() {
        assert_eq!(CredentialScope::from(0), CredentialScope::Unspecified);
        assert_eq!(CredentialScope::from(1), CredentialScope::User);
        assert_eq!(CredentialScope::from(2), CredentialScope::Session);
        assert_eq!(CredentialScope::from(99), CredentialScope::Unspecified);
    }

    #[test]
    fn test_authz_context() {
        let ctx = AuthzContext::new("alice".to_string(), "team-a".to_string())
            .with_scope(CredentialScope::User)
            .with_parent("parent-session".to_string());

        assert_eq!(ctx.subject, "alice");
        assert_eq!(ctx.workspace, "team-a");
        assert!(ctx.is_user_scoped());
        assert!(!ctx.is_session_scoped());
        assert_eq!(ctx.parent, Some("parent-session".to_string()));
    }

    #[test]
    fn test_authz_context_session_scoped() {
        let ctx = AuthzContext::new("session:abc123".to_string(), "team-a".to_string())
            .with_scope(CredentialScope::Session);

        assert!(ctx.is_session_scoped());
        assert!(!ctx.is_user_scoped());
    }

    #[test]
    fn test_authz_error_to_status() {
        let err = AuthzError::PermissionDenied("test".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::PermissionDenied);

        let err = AuthzError::Unauthenticated("test".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let err = AuthzError::InvalidWorkspace("test".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}

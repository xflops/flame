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

//! Role-Based Access Control (RBAC) module for Flame.
//!
//! This module provides permission checking logic for the mTLS authentication system.
//! It implements a simple RBAC model where:
//! - Subjects (users) are identified by certificate CN
//! - Roles define collections of permissions for workspaces
//! - Permissions follow the format "resource:action" (e.g., "session:create", "application:*")

use crate::apis::{Role, User};
use crate::FlameError;

/// Pre-defined workspace constants
pub const WORKSPACE_DEFAULT: &str = "default";
pub const WORKSPACE_SYSTEM: &str = "system";

/// Permission string format: "resource:action"
/// Examples: "session:create", "application:*", "*:*"
pub type Permission = String;

/// Resource types that can have permissions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Application,
    Session,
    Workspace,
    User,
    Role,
    Task,
}

impl Resource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Resource::Application => "application",
            Resource::Session => "session",
            Resource::Workspace => "workspace",
            Resource::User => "user",
            Resource::Role => "role",
            Resource::Task => "task",
        }
    }
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Actions that can be performed on resources
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    Read,
    Update,
    Delete,
    All,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Read => "read",
            Action::Update => "update",
            Action::Delete => "delete",
            Action::All => "*",
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Check if a role has the required permission.
///
/// Permission matching supports wildcards:
/// - "*:*" matches all permissions
/// - "session:*" matches all session actions
/// - "*:read" matches read on all resources
pub fn role_has_permission(role: &Role, resource: &str, action: &str) -> bool {
    for perm in &role.permissions {
        let (perm_resource, perm_action) = perm.split_once(':').unwrap_or((perm, "*"));

        let resource_match = perm_resource == "*" || perm_resource == resource;
        let action_match = perm_action == "*" || perm_action == action;

        if resource_match && action_match {
            return true;
        }
    }

    false
}

/// Check if a role grants access to the specified workspace.
///
/// Workspace matching supports wildcards:
/// - "*" matches all workspaces
pub fn role_has_workspace(role: &Role, workspace: &str) -> bool {
    role.workspaces.iter().any(|w| w == "*" || w == workspace)
}

/// Check if a user has permission for a specific action on a resource in a workspace.
///
/// This checks all roles assigned to the user and returns true if any role grants
/// both the permission AND access to the workspace.
pub fn user_has_permission(
    user: &User,
    roles: &[Role],
    workspace: &str,
    resource: &str,
    action: &str,
) -> bool {
    if !user.enabled {
        return false;
    }

    for role in roles {
        if !user.roles.contains(&role.name) {
            continue;
        }

        if !role_has_workspace(role, workspace) {
            continue;
        }

        if role_has_permission(role, resource, action) {
            return true;
        }
    }

    false
}

/// Format a permission string from resource and action
pub fn format_permission(resource: &str, action: &str) -> String {
    format!("{}:{}", resource, action)
}

/// Parse a permission string into resource and action
pub fn parse_permission(permission: &str) -> Result<(String, String), FlameError> {
    permission
        .split_once(':')
        .map(|(r, a)| (r.to_string(), a.to_string()))
        .ok_or_else(|| {
            FlameError::InvalidConfig(format!(
                "invalid permission format '{}', expected 'resource:action'",
                permission
            ))
        })
}

/// Validate a workspace name.
///
/// Workspace names must:
/// - Be alphanumeric with hyphens
/// - Be max 63 chars (DNS label format)
/// - Not be empty
pub fn validate_workspace_name(name: &str) -> Result<(), FlameError> {
    if name.is_empty() {
        return Err(FlameError::InvalidConfig(
            "workspace name cannot be empty".to_string(),
        ));
    }

    if name.len() > 63 {
        return Err(FlameError::InvalidConfig(format!(
            "workspace name '{}' exceeds 63 characters",
            name
        )));
    }

    let is_valid = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
        && name
            .chars()
            .last()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false);

    if !is_valid {
        return Err(FlameError::InvalidConfig(format!(
            "workspace name '{}' must be alphanumeric with hyphens, starting and ending with alphanumeric",
            name
        )));
    }

    Ok(())
}

/// Check if a workspace name is a pre-defined system workspace.
pub fn is_system_workspace(name: &str) -> bool {
    name == WORKSPACE_DEFAULT || name == WORKSPACE_SYSTEM
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_role(permissions: Vec<&str>, workspaces: Vec<&str>) -> Role {
        Role {
            name: "test-role".to_string(),
            description: Some("Test role".to_string()),
            permissions: permissions.into_iter().map(String::from).collect(),
            workspaces: workspaces.into_iter().map(String::from).collect(),
            creation_time: Utc::now(),
        }
    }

    fn test_user(roles: Vec<&str>, enabled: bool) -> User {
        User {
            name: "test-user".to_string(),
            display_name: Some("Test User".to_string()),
            email: None,
            certificate_cn: "test-user".to_string(),
            enabled,
            creation_time: Utc::now(),
            last_login_time: None,
            roles: roles.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn test_role_has_permission_exact_match() {
        let role = test_role(vec!["session:create", "application:read"], vec!["team-a"]);

        assert!(role_has_permission(&role, "session", "create"));
        assert!(role_has_permission(&role, "application", "read"));
        assert!(!role_has_permission(&role, "session", "delete"));
        assert!(!role_has_permission(&role, "workspace", "create"));
    }

    #[test]
    fn test_role_has_permission_action_wildcard() {
        let role = test_role(vec!["session:*"], vec!["team-a"]);

        assert!(role_has_permission(&role, "session", "create"));
        assert!(role_has_permission(&role, "session", "read"));
        assert!(role_has_permission(&role, "session", "delete"));
        assert!(!role_has_permission(&role, "application", "create"));
    }

    #[test]
    fn test_role_has_permission_resource_wildcard() {
        let role = test_role(vec!["*:read"], vec!["team-a"]);

        assert!(role_has_permission(&role, "session", "read"));
        assert!(role_has_permission(&role, "application", "read"));
        assert!(role_has_permission(&role, "workspace", "read"));
        assert!(!role_has_permission(&role, "session", "create"));
    }

    #[test]
    fn test_role_has_permission_full_wildcard() {
        let role = test_role(vec!["*:*"], vec!["*"]);

        assert!(role_has_permission(&role, "session", "create"));
        assert!(role_has_permission(&role, "application", "delete"));
        assert!(role_has_permission(&role, "workspace", "update"));
    }

    #[test]
    fn test_role_has_workspace() {
        let role = test_role(vec!["session:*"], vec!["team-a", "team-b"]);

        assert!(role_has_workspace(&role, "team-a"));
        assert!(role_has_workspace(&role, "team-b"));
        assert!(!role_has_workspace(&role, "team-c"));
    }

    #[test]
    fn test_role_has_workspace_wildcard() {
        let role = test_role(vec!["session:*"], vec!["*"]);

        assert!(role_has_workspace(&role, "any-workspace"));
        assert!(role_has_workspace(&role, "production"));
        assert!(role_has_workspace(&role, "system"));
    }

    #[test]
    fn test_user_has_permission() {
        let user = test_user(vec!["test-role"], true);
        let roles = vec![test_role(
            vec!["session:*", "application:read"],
            vec!["team-a"],
        )];

        assert!(user_has_permission(
            &user, &roles, "team-a", "session", "create"
        ));
        assert!(user_has_permission(
            &user,
            &roles,
            "team-a",
            "application",
            "read"
        ));
        assert!(!user_has_permission(
            &user,
            &roles,
            "team-a",
            "application",
            "create"
        ));
        assert!(!user_has_permission(
            &user, &roles, "team-b", "session", "create"
        ));
    }

    #[test]
    fn test_user_has_permission_disabled() {
        let user = test_user(vec!["test-role"], false);
        let roles = vec![test_role(vec!["session:*"], vec!["team-a"])];

        assert!(!user_has_permission(
            &user, &roles, "team-a", "session", "create"
        ));
    }

    #[test]
    fn test_user_has_permission_no_role() {
        let user = test_user(vec!["other-role"], true);
        let roles = vec![test_role(vec!["session:*"], vec!["team-a"])];

        assert!(!user_has_permission(
            &user, &roles, "team-a", "session", "create"
        ));
    }

    #[test]
    fn test_validate_workspace_name() {
        assert!(validate_workspace_name("team-a").is_ok());
        assert!(validate_workspace_name("production").is_ok());
        assert!(validate_workspace_name("my-workspace-123").is_ok());
        assert!(validate_workspace_name("a").is_ok());

        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("-invalid").is_err());
        assert!(validate_workspace_name("invalid-").is_err());
        assert!(validate_workspace_name("has space").is_err());
        assert!(validate_workspace_name("has_underscore").is_err());

        let long_name = "a".repeat(64);
        assert!(validate_workspace_name(&long_name).is_err());
        let valid_long_name = "a".repeat(63);
        assert!(validate_workspace_name(&valid_long_name).is_ok());
    }

    #[test]
    fn test_is_system_workspace() {
        assert!(is_system_workspace("default"));
        assert!(is_system_workspace("system"));
        assert!(!is_system_workspace("team-a"));
        assert!(!is_system_workspace("production"));
    }

    #[test]
    fn test_parse_permission() {
        let (resource, action) = parse_permission("session:create").unwrap();
        assert_eq!(resource, "session");
        assert_eq!(action, "create");

        let (resource, action) = parse_permission("*:*").unwrap();
        assert_eq!(resource, "*");
        assert_eq!(action, "*");

        assert!(parse_permission("invalid").is_err());
    }

    #[test]
    fn test_format_permission() {
        assert_eq!(format_permission("session", "create"), "session:create");
        assert_eq!(format_permission("*", "*"), "*:*");
    }
}

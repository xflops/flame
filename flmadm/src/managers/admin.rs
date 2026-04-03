use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

use rpc::flame::admin_client::AdminClient as GrpcAdminClient;
use rpc::flame::frontend_client::FrontendClient as GrpcFrontendClient;
use rpc::flame::{
    CreateRoleRequest, CreateUserRequest, CreateWorkspaceRequest, DeleteRoleRequest,
    DeleteUserRequest, DeleteWorkspaceRequest, GetRoleRequest, GetUserRequest, GetWorkspaceRequest,
    ListRolesRequest, ListUsersRequest, ListWorkspacesRequest, RoleSpec, UpdateRoleRequest,
    UpdateUserRequest, UpdateWorkspaceRequest, UserSpec, WorkspaceSpec,
};

pub struct AdminClient {
    addr: String,
    ca_file: Option<PathBuf>,
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
}

impl AdminClient {
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
            ca_file: None,
            cert_file: None,
            key_file: None,
        }
    }

    async fn connect(&self) -> Result<GrpcAdminClient<Channel>> {
        let endpoint = if self.addr.starts_with("http://") || self.addr.starts_with("https://") {
            self.addr.clone()
        } else {
            format!("http://{}", self.addr)
        };

        let channel = if let (Some(ca_file), Some(cert_file), Some(key_file)) =
            (&self.ca_file, &self.cert_file, &self.key_file)
        {
            let ca_pem = std::fs::read_to_string(ca_file)
                .map_err(|e| anyhow!("failed to read CA file: {}", e))?;
            let cert_pem = std::fs::read_to_string(cert_file)
                .map_err(|e| anyhow!("failed to read cert file: {}", e))?;
            let key_pem = std::fs::read_to_string(key_file)
                .map_err(|e| anyhow!("failed to read key file: {}", e))?;

            let tls_config = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_pem))
                .identity(Identity::from_pem(cert_pem, key_pem));

            let https_endpoint = endpoint.replace("http://", "https://");
            Channel::from_shared(https_endpoint)
                .map_err(|e| anyhow!("invalid endpoint: {}", e))?
                .tls_config(tls_config)
                .map_err(|e| anyhow!("TLS config error: {}", e))?
                .connect()
                .await
                .map_err(|e| anyhow!("failed to connect: {}", e))?
        } else {
            Channel::from_shared(endpoint)
                .map_err(|e| anyhow!("invalid endpoint: {}", e))?
                .connect()
                .await
                .map_err(|e| anyhow!("failed to connect: {}", e))?
        };

        Ok(GrpcAdminClient::new(channel))
    }

    async fn connect_frontend(&self) -> Result<GrpcFrontendClient<Channel>> {
        let endpoint = if self.addr.starts_with("http://") || self.addr.starts_with("https://") {
            self.addr.clone()
        } else {
            format!("http://{}", self.addr)
        };

        let channel = if let (Some(ca_file), Some(cert_file), Some(key_file)) =
            (&self.ca_file, &self.cert_file, &self.key_file)
        {
            let ca_pem = std::fs::read_to_string(ca_file)
                .map_err(|e| anyhow!("failed to read CA file: {}", e))?;
            let cert_pem = std::fs::read_to_string(cert_file)
                .map_err(|e| anyhow!("failed to read cert file: {}", e))?;
            let key_pem = std::fs::read_to_string(key_file)
                .map_err(|e| anyhow!("failed to read key file: {}", e))?;

            let tls_config = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_pem))
                .identity(Identity::from_pem(cert_pem, key_pem));

            let https_endpoint = endpoint.replace("http://", "https://");
            Channel::from_shared(https_endpoint)
                .map_err(|e| anyhow!("invalid endpoint: {}", e))?
                .tls_config(tls_config)
                .map_err(|e| anyhow!("TLS config error: {}", e))?
                .connect()
                .await
                .map_err(|e| anyhow!("failed to connect: {}", e))?
        } else {
            Channel::from_shared(endpoint)
                .map_err(|e| anyhow!("invalid endpoint: {}", e))?
                .connect()
                .await
                .map_err(|e| anyhow!("failed to connect: {}", e))?
        };

        Ok(GrpcFrontendClient::new(channel))
    }

    pub fn create_user(
        &self,
        username: &str,
        display_name: &str,
        _cert_dir: Option<&PathBuf>,
    ) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(CreateUserRequest {
                name: username.to_string(),
                spec: Some(UserSpec {
                    display_name: display_name.to_string(),
                    email: String::new(),
                    certificate_cn: username.to_string(),
                    role_refs: vec![],
                }),
            });

            let response = client
                .create_user(request)
                .await
                .map_err(|e| anyhow!("create user failed: {}", e))?;

            let user = response.into_inner();
            let name = user
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            println!("Created user: {}", name);
            Ok(())
        })
    }

    pub fn list_users(&self, role_filter: Option<&str>) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(ListUsersRequest {
                role_filter: role_filter.map(String::from),
            });

            let response = client
                .list_users(request)
                .await
                .map_err(|e| anyhow!("list users failed: {}", e))?;

            let users = response.into_inner().users;
            if users.is_empty() {
                println!("No users found");
            } else {
                println!(
                    "{:<20} {:<30} {:<10} ROLES",
                    "NAME", "DISPLAY NAME", "ENABLED"
                );
                for user in users {
                    let name = user
                        .metadata
                        .as_ref()
                        .map(|m| m.name.as_str())
                        .unwrap_or("");
                    let display = user
                        .spec
                        .as_ref()
                        .map(|s| s.display_name.as_str())
                        .unwrap_or("");
                    let enabled = user.status.as_ref().map(|s| s.enabled).unwrap_or(false);
                    let roles = user
                        .spec
                        .as_ref()
                        .map(|s| s.role_refs.join(", "))
                        .unwrap_or_default();
                    println!("{:<20} {:<30} {:<10} {}", name, display, enabled, roles);
                }
            }
            Ok(())
        })
    }

    pub fn get_user(&self, username: &str) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(GetUserRequest {
                name: username.to_string(),
            });

            let response = client
                .get_user(request)
                .await
                .map_err(|e| anyhow!("get user failed: {}", e))?;

            let user = response.into_inner();
            let name = user
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            let enabled = user.status.as_ref().map(|s| s.enabled).unwrap_or(false);
            println!("Name: {}", name);
            println!("Enabled: {}", enabled);
            if let Some(spec) = user.spec {
                println!("Display Name: {}", spec.display_name);
                println!("Email: {}", spec.email);
                println!("Certificate CN: {}", spec.certificate_cn);
                println!("Roles: {}", spec.role_refs.join(", "));
            }
            Ok(())
        })
    }

    pub fn update_user(&self, username: &str, assign: &[String], revoke: &[String]) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(UpdateUserRequest {
                name: username.to_string(),
                spec: None,
                assign_roles: assign.to_vec(),
                revoke_roles: revoke.to_vec(),
            });

            let response = client
                .update_user(request)
                .await
                .map_err(|e| anyhow!("update user failed: {}", e))?;

            let user = response.into_inner();
            let name = user
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            println!("Updated user: {}", name);
            Ok(())
        })
    }

    pub fn delete_user(&self, username: &str, _force: bool) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(DeleteUserRequest {
                name: username.to_string(),
            });

            client
                .delete_user(request)
                .await
                .map_err(|e| anyhow!("delete user failed: {}", e))?;

            println!("Deleted user: {}", username);
            Ok(())
        })
    }

    pub fn enable_user(&self, username: &str) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let get_request = tonic::Request::new(GetUserRequest {
                name: username.to_string(),
            });
            let user = client
                .get_user(get_request)
                .await
                .map_err(|e| anyhow!("get user failed: {}", e))?
                .into_inner();

            let request = tonic::Request::new(UpdateUserRequest {
                name: username.to_string(),
                spec: user.spec,
                assign_roles: vec![],
                revoke_roles: vec![],
            });

            client
                .update_user(request)
                .await
                .map_err(|e| anyhow!("enable user failed: {}", e))?;

            println!("Enabled user: {}", username);
            Ok(())
        })
    }

    pub fn disable_user(&self, username: &str) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let get_request = tonic::Request::new(GetUserRequest {
                name: username.to_string(),
            });
            let user = client
                .get_user(get_request)
                .await
                .map_err(|e| anyhow!("get user failed: {}", e))?
                .into_inner();

            let request = tonic::Request::new(UpdateUserRequest {
                name: username.to_string(),
                spec: user.spec,
                assign_roles: vec![],
                revoke_roles: vec![],
            });

            client
                .update_user(request)
                .await
                .map_err(|e| anyhow!("disable user failed: {}", e))?;

            println!("Disabled user: {}", username);
            Ok(())
        })
    }

    pub fn create_role(
        &self,
        role: &str,
        description: &str,
        permissions: &[String],
        workspaces: &[String],
    ) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(CreateRoleRequest {
                name: role.to_string(),
                spec: Some(RoleSpec {
                    description: description.to_string(),
                    permissions: permissions.to_vec(),
                    workspaces: workspaces.to_vec(),
                }),
            });

            let response = client
                .create_role(request)
                .await
                .map_err(|e| anyhow!("create role failed: {}", e))?;

            let role = response.into_inner();
            let name = role
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            println!("Created role: {}", name);
            Ok(())
        })
    }

    pub fn list_roles(&self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(ListRolesRequest {
                workspace_filter: None,
            });

            let response = client
                .list_roles(request)
                .await
                .map_err(|e| anyhow!("list roles failed: {}", e))?;

            let roles = response.into_inner().roles;
            if roles.is_empty() {
                println!("No roles found");
            } else {
                println!("{:<20} {:<40} WORKSPACES", "NAME", "PERMISSIONS");
                for role in roles {
                    let name = role
                        .metadata
                        .as_ref()
                        .map(|m| m.name.as_str())
                        .unwrap_or("");
                    let perms = role
                        .spec
                        .as_ref()
                        .map(|s| s.permissions.join(", "))
                        .unwrap_or_default();
                    let ws = role
                        .spec
                        .as_ref()
                        .map(|s| s.workspaces.join(", "))
                        .unwrap_or_default();
                    println!("{:<20} {:<40} {}", name, perms, ws);
                }
            }
            Ok(())
        })
    }

    pub fn get_role(&self, role: &str) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(GetRoleRequest {
                name: role.to_string(),
            });

            let response = client
                .get_role(request)
                .await
                .map_err(|e| anyhow!("get role failed: {}", e))?;

            let role = response.into_inner();
            let name = role
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            println!("Name: {}", name);
            if let Some(spec) = role.spec {
                println!("Description: {}", spec.description);
                println!("Permissions: {}", spec.permissions.join(", "));
                println!("Workspaces: {}", spec.workspaces.join(", "));
            }
            Ok(())
        })
    }

    pub fn update_role(
        &self,
        role: &str,
        add_perm: &[String],
        remove_perm: &[String],
        add_ws: &[String],
        remove_ws: &[String],
    ) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let get_request = tonic::Request::new(GetRoleRequest {
                name: role.to_string(),
            });
            let existing = client
                .get_role(get_request)
                .await
                .map_err(|e| anyhow!("get role failed: {}", e))?
                .into_inner();

            let mut permissions = existing
                .spec
                .as_ref()
                .map(|s| s.permissions.clone())
                .unwrap_or_default();
            let mut workspaces = existing
                .spec
                .as_ref()
                .map(|s| s.workspaces.clone())
                .unwrap_or_default();

            for p in add_perm {
                if !permissions.contains(p) {
                    permissions.push(p.clone());
                }
            }
            permissions.retain(|p| !remove_perm.contains(p));

            for w in add_ws {
                if !workspaces.contains(w) {
                    workspaces.push(w.clone());
                }
            }
            workspaces.retain(|w| !remove_ws.contains(w));

            let request = tonic::Request::new(UpdateRoleRequest {
                name: role.to_string(),
                spec: Some(RoleSpec {
                    description: existing
                        .spec
                        .as_ref()
                        .map(|s| s.description.clone())
                        .unwrap_or_default(),
                    permissions,
                    workspaces,
                }),
            });

            let response = client
                .update_role(request)
                .await
                .map_err(|e| anyhow!("update role failed: {}", e))?;

            println!(
                "Updated role: {}",
                response
                    .into_inner()
                    .metadata
                    .as_ref()
                    .map(|m| m.name.as_str())
                    .unwrap_or("unknown")
            );
            Ok(())
        })
    }

    pub fn delete_role(&self, role: &str, _force: bool) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect().await?;

            let request = tonic::Request::new(DeleteRoleRequest {
                name: role.to_string(),
            });

            client
                .delete_role(request)
                .await
                .map_err(|e| anyhow!("delete role failed: {}", e))?;

            println!("Deleted role: {}", role);
            Ok(())
        })
    }

    pub fn create_workspace(
        &self,
        workspace: &str,
        description: &str,
        labels: &[(String, String)],
    ) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect_frontend().await?;

            let request = tonic::Request::new(CreateWorkspaceRequest {
                name: workspace.to_string(),
                spec: Some(WorkspaceSpec {
                    description: description.to_string(),
                    labels: labels.iter().cloned().collect(),
                }),
            });

            let response = client
                .create_workspace(request)
                .await
                .map_err(|e| anyhow!("create workspace failed: {}", e))?;

            let ws = response.into_inner();
            let name = ws
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            println!("Created workspace: {}", name);
            Ok(())
        })
    }

    pub fn list_workspaces(&self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect_frontend().await?;

            let request = tonic::Request::new(ListWorkspacesRequest {});

            let response = client
                .list_workspaces(request)
                .await
                .map_err(|e| anyhow!("list workspaces failed: {}", e))?;

            let workspaces = response.into_inner().workspaces;
            if workspaces.is_empty() {
                println!("No workspaces found");
            } else {
                println!("{:<20} DESCRIPTION", "NAME");
                for ws in workspaces {
                    let name = ws.metadata.as_ref().map(|m| m.name.as_str()).unwrap_or("");
                    let desc = ws
                        .spec
                        .as_ref()
                        .map(|s| s.description.as_str())
                        .unwrap_or("");
                    println!("{:<20} {}", name, desc);
                }
            }
            Ok(())
        })
    }

    pub fn get_workspace(&self, workspace: &str) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect_frontend().await?;

            let request = tonic::Request::new(GetWorkspaceRequest {
                name: workspace.to_string(),
            });

            let response = client
                .get_workspace(request)
                .await
                .map_err(|e| anyhow!("get workspace failed: {}", e))?;

            let ws = response.into_inner();
            let name = ws
                .metadata
                .as_ref()
                .map(|m| m.name.as_str())
                .unwrap_or("unknown");
            println!("Name: {}", name);
            if let Some(spec) = ws.spec {
                println!("Description: {}", spec.description);
                if !spec.labels.is_empty() {
                    println!("Labels:");
                    for (k, v) in spec.labels {
                        println!("  {}: {}", k, v);
                    }
                }
            }
            Ok(())
        })
    }

    pub fn update_workspace(
        &self,
        workspace: &str,
        description: &Option<String>,
        labels: &Vec<String>,
    ) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect_frontend().await?;

            let get_request = tonic::Request::new(GetWorkspaceRequest {
                name: workspace.to_string(),
            });
            let existing = client
                .get_workspace(get_request)
                .await
                .map_err(|e| anyhow!("get workspace failed: {}", e))?
                .into_inner();

            let desc = description.clone().unwrap_or_else(|| {
                existing
                    .spec
                    .as_ref()
                    .map(|s| s.description.clone())
                    .unwrap_or_default()
            });

            let mut label_map = existing
                .spec
                .as_ref()
                .map(|s| s.labels.clone())
                .unwrap_or_default();
            for label in labels {
                if let Some((k, v)) = label.split_once('=') {
                    label_map.insert(k.to_string(), v.to_string());
                }
            }

            let request = tonic::Request::new(UpdateWorkspaceRequest {
                name: workspace.to_string(),
                spec: Some(WorkspaceSpec {
                    description: desc,
                    labels: label_map,
                }),
            });

            let response = client
                .update_workspace(request)
                .await
                .map_err(|e| anyhow!("update workspace failed: {}", e))?;

            println!(
                "Updated workspace: {}",
                response
                    .into_inner()
                    .metadata
                    .as_ref()
                    .map(|m| m.name.as_str())
                    .unwrap_or("unknown")
            );
            Ok(())
        })
    }

    pub fn delete_workspace(&self, workspace: &str, force: bool) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let mut client = self.connect_frontend().await?;

            let request = tonic::Request::new(DeleteWorkspaceRequest {
                name: workspace.to_string(),
                force,
            });

            client
                .delete_workspace(request)
                .await
                .map_err(|e| anyhow!("delete workspace failed: {}", e))?;

            println!("Deleted workspace: {}", workspace);
            Ok(())
        })
    }
}

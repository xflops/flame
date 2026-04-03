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

use async_trait::async_trait;
use chrono::Utc;
use stdng::trace_fn;
use tonic::{Request, Response, Status};

use self::rpc::admin_server::Admin;
use rpc::flame as rpc;

use common::apis::{Role, User};

use crate::controller::ControllerPtr;

pub struct AdminService {
    controller: ControllerPtr,
}

impl AdminService {
    pub fn new(controller: ControllerPtr) -> Self {
        Self { controller }
    }
}

#[async_trait]
impl Admin for AdminService {
    async fn create_user(
        &self,
        req: Request<rpc::CreateUserRequest>,
    ) -> Result<Response<rpc::User>, Status> {
        trace_fn!("Admin::create_user");

        let req = req.into_inner();
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("user spec is required"))?;

        if req.name.is_empty() {
            return Err(Status::invalid_argument("user name is required"));
        }

        let certificate_cn = if spec.certificate_cn.is_empty() {
            req.name.clone()
        } else {
            spec.certificate_cn
        };

        let user = User {
            name: req.name,
            display_name: if spec.display_name.is_empty() {
                None
            } else {
                Some(spec.display_name)
            },
            email: if spec.email.is_empty() {
                None
            } else {
                Some(spec.email)
            },
            certificate_cn,
            enabled: true,
            creation_time: Utc::now(),
            last_login_time: None,
            roles: spec.role_refs,
        };

        let user = self
            .controller
            .create_user(&user)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::User::from(user)))
    }

    async fn get_user(
        &self,
        req: Request<rpc::GetUserRequest>,
    ) -> Result<Response<rpc::User>, Status> {
        trace_fn!("Admin::get_user");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("user name is required"));
        }

        let user = self
            .controller
            .get_user(&req.name)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("user '{}' not found", req.name)))?;

        Ok(Response::new(rpc::User::from(user)))
    }

    async fn update_user(
        &self,
        req: Request<rpc::UpdateUserRequest>,
    ) -> Result<Response<rpc::User>, Status> {
        trace_fn!("Admin::update_user");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("user name is required"));
        }

        let existing = self
            .controller
            .get_user(&req.name)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("user '{}' not found", req.name)))?;

        let spec = req.spec.unwrap_or_default();

        let user = User {
            name: existing.name,
            display_name: if spec.display_name.is_empty() {
                existing.display_name
            } else {
                Some(spec.display_name)
            },
            email: if spec.email.is_empty() {
                existing.email
            } else {
                Some(spec.email)
            },
            certificate_cn: if spec.certificate_cn.is_empty() {
                existing.certificate_cn
            } else {
                spec.certificate_cn
            },
            enabled: existing.enabled,
            creation_time: existing.creation_time,
            last_login_time: existing.last_login_time,
            roles: existing.roles,
        };

        let user = self
            .controller
            .update_user(&user, &req.assign_roles, &req.revoke_roles)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::User::from(user)))
    }

    async fn delete_user(
        &self,
        req: Request<rpc::DeleteUserRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Admin::delete_user");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("user name is required"));
        }

        self.controller
            .delete_user(&req.name)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Result {
            return_code: 0,
            message: None,
        }))
    }

    async fn list_users(
        &self,
        req: Request<rpc::ListUsersRequest>,
    ) -> Result<Response<rpc::UserList>, Status> {
        trace_fn!("Admin::list_users");

        let req = req.into_inner();
        let role_filter = req.role_filter.as_deref();

        let users = self
            .controller
            .list_users(role_filter)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::UserList {
            users: users.into_iter().map(rpc::User::from).collect(),
        }))
    }

    async fn create_role(
        &self,
        req: Request<rpc::CreateRoleRequest>,
    ) -> Result<Response<rpc::Role>, Status> {
        trace_fn!("Admin::create_role");

        let req = req.into_inner();
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("role spec is required"))?;

        if req.name.is_empty() {
            return Err(Status::invalid_argument("role name is required"));
        }

        if spec.permissions.is_empty() {
            return Err(Status::invalid_argument(
                "role must have at least one permission",
            ));
        }

        if spec.workspaces.is_empty() {
            return Err(Status::invalid_argument(
                "role must have at least one workspace",
            ));
        }

        let role = Role {
            name: req.name,
            description: if spec.description.is_empty() {
                None
            } else {
                Some(spec.description)
            },
            permissions: spec.permissions,
            workspaces: spec.workspaces,
            creation_time: Utc::now(),
        };

        let role = self
            .controller
            .create_role(&role)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Role::from(role)))
    }

    async fn get_role(
        &self,
        req: Request<rpc::GetRoleRequest>,
    ) -> Result<Response<rpc::Role>, Status> {
        trace_fn!("Admin::get_role");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("role name is required"));
        }

        let role = self
            .controller
            .get_role(&req.name)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("role '{}' not found", req.name)))?;

        Ok(Response::new(rpc::Role::from(role)))
    }

    async fn update_role(
        &self,
        req: Request<rpc::UpdateRoleRequest>,
    ) -> Result<Response<rpc::Role>, Status> {
        trace_fn!("Admin::update_role");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("role name is required"));
        }

        let existing = self
            .controller
            .get_role(&req.name)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::not_found(format!("role '{}' not found", req.name)))?;

        let spec = req.spec.unwrap_or_default();

        let role = Role {
            name: existing.name,
            description: if spec.description.is_empty() {
                existing.description
            } else {
                Some(spec.description)
            },
            permissions: if spec.permissions.is_empty() {
                existing.permissions
            } else {
                spec.permissions
            },
            workspaces: if spec.workspaces.is_empty() {
                existing.workspaces
            } else {
                spec.workspaces
            },
            creation_time: existing.creation_time,
        };

        let role = self
            .controller
            .update_role(&role)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Role::from(role)))
    }

    async fn delete_role(
        &self,
        req: Request<rpc::DeleteRoleRequest>,
    ) -> Result<Response<rpc::Result>, Status> {
        trace_fn!("Admin::delete_role");

        let req = req.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("role name is required"));
        }

        self.controller
            .delete_role(&req.name)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::Result {
            return_code: 0,
            message: None,
        }))
    }

    async fn list_roles(
        &self,
        req: Request<rpc::ListRolesRequest>,
    ) -> Result<Response<rpc::RoleList>, Status> {
        trace_fn!("Admin::list_roles");

        let req = req.into_inner();
        let workspace_filter = req.workspace_filter.as_deref();

        let roles = self
            .controller
            .list_roles(workspace_filter)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(rpc::RoleList {
            roles: roles.into_iter().map(rpc::Role::from).collect(),
        }))
    }
}

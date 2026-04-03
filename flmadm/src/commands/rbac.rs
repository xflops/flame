use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Clone, Args)]
pub struct CreateCmd {
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long = "display-name")]
    pub display_name: Option<String>,
    #[arg(long = "cert-dir")]
    pub cert_dir: Option<PathBuf>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, value_name = "PERM")]
    pub permission: Vec<String>,
    #[arg(long, value_name = "LABEL")]
    pub label: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ListCmd {
    #[arg(long)]
    pub user: bool,
    #[arg(long)]
    pub role: bool,
    #[arg(long)]
    pub workspace: bool,
    #[arg(long, value_name = "ROLE")]
    pub filter_role: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct GetCmd {
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct UpdateCmd {
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long = "assign-role")]
    pub assign_role: Vec<String>,
    #[arg(long = "revoke-role")]
    pub revoke_role: Vec<String>,
    #[arg(long = "add-permission")]
    pub add_permission: Vec<String>,
    #[arg(long = "remove-permission")]
    pub remove_permission: Vec<String>,
    #[arg(long = "add-workspace")]
    pub add_workspace: Vec<String>,
    #[arg(long = "remove-workspace")]
    pub remove_workspace: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct DeleteCmd {
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct EnableCmd {
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct DisableCmd {
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
}

use crate::managers::admin as admin_mgr;
use anyhow::anyhow;

pub fn handle_create(cmd: &CreateCmd) -> Result<()> {
    if let Some(user) = &cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051").create_user(
            user,
            cmd.display_name.as_deref().unwrap_or(""),
            cmd.cert_dir.as_ref(),
        )?;
        return Ok(());
    }
    if let Some(role) = &cmd.role {
        admin_mgr::AdminClient::new("http://localhost:50051").create_role(
            role,
            cmd.description.as_deref().unwrap_or(""),
            &cmd.permission,
            &[],
        )?;
        return Ok(());
    }
    if let Some(workspace) = &cmd.workspace {
        let labels: Vec<(String, String)> = cmd
            .label
            .iter()
            .filter_map(|l| {
                l.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        admin_mgr::AdminClient::new("http://localhost:50051").create_workspace(
            workspace,
            cmd.description.as_deref().unwrap_or(""),
            &labels,
        )?;
        return Ok(());
    }
    Err(anyhow!("No resource specified for create"))
}

pub fn handle_list(cmd: &ListCmd) -> Result<()> {
    if cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051")
            .list_users(cmd.filter_role.as_deref())?;
        println!("Listed users");
        return Ok(());
    }
    if cmd.role {
        admin_mgr::AdminClient::new("http://localhost:50051").list_roles()?;
        println!("Listed roles");
        return Ok(());
    }
    if cmd.workspace {
        admin_mgr::AdminClient::new("http://localhost:50051").list_workspaces()?;
        println!("Listed workspaces");
        return Ok(());
    }
    Err(anyhow!("No resource selected for list"))
}

pub fn handle_get(cmd: &GetCmd) -> Result<()> {
    if let Some(user) = &cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051").get_user(user)?;
        println!("Got user: {}", user);
        return Ok(());
    }
    if let Some(role) = &cmd.role {
        admin_mgr::AdminClient::new("http://localhost:50051").get_role(role)?;
        println!("Got role: {}", role);
        return Ok(());
    }
    if let Some(workspace) = &cmd.workspace {
        admin_mgr::AdminClient::new("http://localhost:50051").get_workspace(workspace)?;
        println!("Got workspace: {}", workspace);
        return Ok(());
    }
    Err(anyhow!("No resource specified for get"))
}

pub fn handle_update(cmd: &UpdateCmd) -> Result<()> {
    if let Some(user) = &cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051").update_user(
            user,
            &cmd.assign_role,
            &cmd.revoke_role,
        )?;
        println!("Updated user: {}", user);
        return Ok(());
    }
    if let Some(role) = &cmd.role {
        admin_mgr::AdminClient::new("http://localhost:50051").update_role(
            role,
            &cmd.add_permission,
            &cmd.remove_permission,
            &cmd.add_workspace,
            &cmd.remove_workspace,
        )?;
        println!("Updated role: {}", role);
        return Ok(());
    }
    if let Some(workspace) = &cmd.workspace {
        let desc: Option<String> = None;
        let labels: Vec<String> = Vec::new();
        admin_mgr::AdminClient::new("http://localhost:50051")
            .update_workspace(workspace, &desc, &labels)?;
        println!("Updated workspace: {}", workspace);
        return Ok(());
    }
    Err(anyhow!("No resource specified for update"))
}

pub fn handle_delete(cmd: &DeleteCmd) -> Result<()> {
    if let Some(user) = &cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051").delete_user(user, cmd.force)?;
        println!("Deleted user: {}", user);
        return Ok(());
    }
    if let Some(role) = &cmd.role {
        admin_mgr::AdminClient::new("http://localhost:50051").delete_role(role, cmd.force)?;
        println!("Deleted role: {}", role);
        return Ok(());
    }
    if let Some(workspace) = &cmd.workspace {
        admin_mgr::AdminClient::new("http://localhost:50051")
            .delete_workspace(workspace, cmd.force)?;
        println!("Deleted workspace: {}", workspace);
        return Ok(());
    }
    Err(anyhow!("No resource specified for delete"))
}

pub fn handle_enable(cmd: &EnableCmd) -> Result<()> {
    if let Some(user) = &cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051").enable_user(user)?;
        println!("Enabled user: {}", user);
        return Ok(());
    }
    Err(anyhow!("Only user enable is supported in this MVP"))
}

pub fn handle_disable(cmd: &DisableCmd) -> Result<()> {
    if let Some(user) = &cmd.user {
        admin_mgr::AdminClient::new("http://localhost:50051").disable_user(user)?;
        println!("Disabled user: {}", user);
        return Ok(());
    }
    Err(anyhow!("Only user disable is supported in this MVP"))
}

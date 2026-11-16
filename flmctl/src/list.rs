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

use std::error::Error;

use clap::{ArgGroup, Args};
use comfy_table::presets::NOTHING;
use comfy_table::Table;
use flame_rs as flame;
use flame_rs::apis::{FlameContext, FlameError};
use flame_rs::client::{
    Application, Connection, Executor, Node, NodeState, Paging, Session, SessionFilter,
};
use serde::Serialize;

use crate::utils::{format_memory, format_resreq};

/// Existing selectors retain their output; pagination and filters are opt-in.
#[derive(Args, Clone, Debug)]
#[command(group(ArgGroup::new("resource").required(true).multiple(false).args(["application", "session", "executor", "node"])))]
pub struct ListOptions {
    /// List applications.
    #[arg(short, long)]
    application: bool,
    /// List sessions.
    #[arg(short, long)]
    session: bool,
    /// List executors.
    #[arg(short, long)]
    executor: bool,
    /// List nodes.
    #[arg(short, long)]
    node: bool,
    /// Output format.
    #[arg(short, long, default_value="table", value_parser=["table", "json"])]
    output_format: String,
    /// Page size. Zero selects the server default.
    #[arg(long, default_value_t = 0)]
    limit: i32,
    /// One-based page number. Zero selects the first page.
    #[arg(long, default_value_t = 0)]
    number: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resource {
    Applications,
    Sessions,
    Executors,
    Nodes,
}

impl ListOptions {
    /// Identify the selected resource, including programmatically created arguments.
    pub(crate) fn resource(&self) -> Result<Resource, FlameError> {
        let selected = [
            (self.application, Resource::Applications),
            (self.session, Resource::Sessions),
            (self.executor, Resource::Executors),
            (self.node, Resource::Nodes),
        ];
        let mut selected = selected
            .into_iter()
            .filter_map(|(yes, resource)| yes.then_some(resource));
        let resource = selected
            .next()
            .ok_or_else(|| invalid("select exactly one resource"))?;
        if selected.next().is_some() {
            return Err(invalid("select exactly one resource"));
        }
        Ok(resource)
    }

    pub(crate) fn page(&self) -> Paging {
        Paging {
            limit: self.limit,
            number: self.number,
            total: None,
        }
    }

    /// Keep invalid resource-specific filters from silently selecting a different set.
    pub(crate) fn validate(&self) -> Result<Resource, FlameError> {
        self.resource()
    }
}

/// Report invalid arguments without opening a connection.
fn invalid(message: &str) -> FlameError {
    FlameError::InvalidConfig(message.into())
}

/// List one resource. The server returns at most its default list limitation.
pub async fn run(ctx: &FlameContext, options: &ListOptions) -> Result<(), Box<dyn Error>> {
    let resource = options.validate()?;
    let current = ctx.get_current_context()?;
    let connection =
        flame::client::connect_with_tls(&current.cluster.endpoint, current.cluster.tls.as_ref())
            .await?;
    match resource {
        Resource::Applications => {
            list_applications(connection, &options.output_format, options.page()).await
        }
        Resource::Sessions => {
            list_sessions(connection, &options.output_format, options.page()).await
        }
        Resource::Executors => {
            list_executors(connection, &options.output_format, options.page()).await
        }
        Resource::Nodes => list_nodes(connection, &options.output_format, options.page()).await,
    }
}

pub(crate) fn format_json<T: Serialize + ?Sized>(items: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(items)
}

async fn list_applications(
    conn: Connection,
    format: &str,
    page: Paging,
) -> Result<(), Box<dyn Error>> {
    let items = conn
        .list_applications(Some(&flame::client::ApplicationFilter {
            paging: Some(page),
            ..Default::default()
        }))
        .await?;
    if format == "json" {
        println!("{}", format_json(&items)?)
    } else {
        println!("{}", application_table(&items))
    }
    Ok(())
}

fn application_table(items: &[Application]) -> Table {
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_header(["Name", "State", "Shim", "Tags", "Created", "Command"]);
    for app in items {
        table.add_row([
            app.name.clone(),
            app.state.to_string(),
            app.attributes
                .shim
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".into()),
            app.attributes.labels.join(", "),
            app.creation_time.format("%T").to_string(),
            app.attributes.command.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    table
}

async fn list_sessions(conn: Connection, format: &str, page: Paging) -> Result<(), Box<dyn Error>> {
    let items = conn
        .list_sessions(Some(&SessionFilter {
            paging: Some(page),
            ..SessionFilter::default()
        }))
        .await?;
    if format == "json" {
        println!("{}", format_json(&items)?)
    } else {
        println!("{}", session_table(&items));
    }
    Ok(())
}

/// Render in the order returned by the server.
fn session_table(items: &[Session]) -> Table {
    let mut table = Table::new();
    table.load_style(NOTHING).set_header([
        "ID",
        "State",
        "App",
        "Resources",
        "Priority",
        "Pending",
        "Running",
        "Succeed",
        "Failed",
        "Created",
    ]);
    for session in items {
        table.add_row([
            session.id.clone(),
            session.state.to_string(),
            session.application.clone(),
            format_resreq(&session.resreq),
            session.priority.to_string(),
            session.pending.to_string(),
            session.running.to_string(),
            session.succeed.to_string(),
            session.failed.to_string(),
            session.creation_time.format("%T").to_string(),
        ]);
    }
    table
}

async fn list_executors(
    conn: Connection,
    format: &str,
    page: Paging,
) -> Result<(), Box<dyn Error>> {
    let items = conn
        .list_executors(Some(&flame::client::ExecutorFilter {
            paging: Some(page),
            ..Default::default()
        }))
        .await?;
    if format == "json" {
        println!("{}", format_json(&items)?)
    } else {
        println!("{}", executor_table(&items))
    }
    Ok(())
}

pub(crate) fn executor_table(executors: &[Executor]) -> Table {
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_header(["ID", "State", "App", "Session", "Node"]);
    for executor in executors {
        table.add_row([
            executor.id.clone(),
            executor.state.to_string(),
            executor.application.clone(),
            executor.session_id.clone().unwrap_or_else(|| "-".into()),
            executor.node.clone(),
        ]);
    }
    table
}

async fn list_nodes(conn: Connection, format: &str, page: Paging) -> Result<(), Box<dyn Error>> {
    let items = conn
        .list_nodes(Some(&flame::client::NodeFilter {
            paging: Some(page),
            ..Default::default()
        }))
        .await?;
    if format == "json" {
        println!("{}", format_json(&items)?)
    } else {
        println!("{}", node_table(&items))
    }
    Ok(())
}

fn node_table(items: &[Node]) -> Table {
    let mut table = Table::new();
    table.load_style(NOTHING).set_header([
        "NAME", "HOSTNAME", "STATUS", "CPU", "MEMORY", "GPU", "ARCH", "OS",
    ]);
    for node in items {
        table.add_row([
            node.name.clone(),
            node.hostname.clone(),
            match node.state {
                NodeState::Ready => "Ready",
                NodeState::NotReady => "NotReady",
                NodeState::Unknown => "Unknown",
            }
            .into(),
            node.cpu.to_string(),
            format_memory(node.memory),
            node.gpu.to_string(),
            node.arch.clone(),
            node.os.clone(),
        ]);
    }
    table
}

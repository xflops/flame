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
use std::future::Future;

use clap::{ArgGroup, Args};
use comfy_table::presets::NOTHING;
use comfy_table::Table;
use flame_rs as flame;
use flame_rs::apis::{
    ApplicationState, ExecutorState, FlameContext, FlameError, SessionState, TaskState,
};
use flame_rs::client::{
    Application, ApplicationFilter, Connection, Executor, ExecutorFilter, Node, NodeFilter,
    NodeState, Page, PageRequest, Session, SessionFilter, Task, TaskFilter,
};
use serde::Serialize;

use crate::utils::{format_memory, format_resreq};

/// Existing selectors retain their output; pagination and filters are opt-in.
#[derive(Args, Clone, Debug)]
#[command(group(ArgGroup::new("resource").required(true).multiple(false).args(["application", "session", "executor", "node", "task"])))]
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
    /// List a session's tasks using bounded pages.
    #[arg(short, long, requires = "session_id")]
    task: bool,
    /// Output format. Paged JSON contains items and paging metadata.
    #[arg(short, long, default_value="table", value_parser=["table", "json"])]
    output_format: String,
    /// Page size, from 0 (server default of 100) to 1000.
    #[arg(long, value_parser=clap::value_parser!(i32).range(0..=1000))]
    page_size: Option<i32>,
    /// Page number, starting at one (zero selects the first page).
    #[arg(long, conflicts_with = "all", value_parser = clap::value_parser!(i64).range(0..))]
    page_number: Option<i64>,
    /// Retrieve every matching page through bounded server requests.
    #[arg(long)]
    all: bool,
    /// Server order, e.g. id, name or creation_time desc where supported.
    #[arg(long)]
    order_by: Option<String>,
    /// Resource state (case-insensitive).
    #[arg(long)]
    state: Option<String>,
    /// Exact application or node names, separated by commas.
    #[arg(long, value_delimiter=',', num_args=1..)]
    names: Vec<String>,
    /// Exact session, executor or numeric task IDs, separated by commas.
    #[arg(long, value_delimiter=',', num_args=1..)]
    ids: Vec<String>,
    /// Restrict sessions or executors to this application.
    #[arg(long)]
    application_name: Option<String>,
    /// Restrict executors to this node.
    #[arg(long)]
    node_name: Option<String>,
    /// Required task session, or an executor's current session filter.
    #[arg(long)]
    session_id: Option<String>,
    /// Include input, output and affinity in paged task JSON.
    #[arg(long, requires = "task")]
    include_payloads: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Resource {
    Applications,
    Sessions,
    Executors,
    Nodes,
    Tasks,
}

impl ListOptions {
    /// Identify the selected resource, including programmatically created arguments.
    fn resource(&self) -> Result<Resource, FlameError> {
        let selected = [
            (self.application, Resource::Applications),
            (self.session, Resource::Sessions),
            (self.executor, Resource::Executors),
            (self.node, Resource::Nodes),
            (self.task, Resource::Tasks),
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

    /// Keep invalid resource-specific filters from silently selecting a different set.
    fn validate(&self) -> Result<Resource, FlameError> {
        let resource = self.resource()?;
        if !self.names.is_empty() && !matches!(resource, Resource::Applications | Resource::Nodes) {
            return Err(invalid("--names is supported for applications and nodes"));
        }
        if !self.ids.is_empty()
            && !matches!(
                resource,
                Resource::Sessions | Resource::Executors | Resource::Tasks
            )
        {
            return Err(invalid(
                "--ids is supported for sessions, executors and tasks",
            ));
        }
        if self.application_name.is_some()
            && !matches!(resource, Resource::Sessions | Resource::Executors)
        {
            return Err(invalid(
                "--application-name is supported for sessions and executors",
            ));
        }
        if self.node_name.is_some() && resource != Resource::Executors {
            return Err(invalid("--node-name is supported for executors"));
        }
        if self.session_id.is_some() && !matches!(resource, Resource::Executors | Resource::Tasks) {
            return Err(invalid("--session-id is supported for executors and tasks"));
        }
        if resource == Resource::Tasks && self.session_id.is_none() {
            return Err(invalid("--task requires --session-id"));
        }
        if self.include_payloads && resource != Resource::Tasks {
            return Err(invalid("--include-payloads is supported for tasks"));
        }
        Ok(resource)
    }

    /// No new flags means the original complete list and JSON-array behavior.
    fn paged(&self) -> bool {
        self.task
            || self.page_size.is_some()
            || self.page_number.is_some()
            || self.all
            || self.order_by.is_some()
            || self.state.is_some()
            || !self.names.is_empty()
            || !self.ids.is_empty()
            || self.application_name.is_some()
            || self.node_name.is_some()
            || self.session_id.is_some()
            || self.include_payloads
    }

    /// Preserve ordering while requesting a numbered page.
    fn page_request(&self) -> PageRequest {
        PageRequest {
            page_size: self.page_size.unwrap_or_default(),
            page_number: self.page_number.unwrap_or_default(),
            order_by: self.order_by.clone().unwrap_or_default(),
        }
    }
}

/// Dispatch legacy complete-list commands or opt-in bounded page commands.
pub async fn run(ctx: &FlameContext, options: &ListOptions) -> Result<(), Box<dyn Error>> {
    let resource = options.validate()?;
    let current = ctx.get_current_context()?;
    let connection =
        flame::client::connect_with_tls(&current.cluster.endpoint, current.cluster.tls.as_ref())
            .await?;
    if options.paged() {
        return list_page(connection, resource, options).await;
    }
    match resource {
        Resource::Applications => list_applications(connection, &options.output_format).await,
        Resource::Sessions => list_sessions(connection, &options.output_format).await,
        Resource::Executors => list_executors(connection, &options.output_format).await,
        Resource::Nodes => list_nodes(connection, &options.output_format).await,
        Resource::Tasks => unreachable!("task listing always uses pagination"),
    }
}

/// Report invalid arguments without opening a connection.
fn invalid(message: &str) -> FlameError {
    FlameError::InvalidConfig(message.into())
}

/// Interpret state names using the resource's existing public enum values.
fn state<T: Copy>(value: &Option<String>, choices: &[(&str, T)]) -> Result<Option<T>, FlameError> {
    value
        .as_ref()
        .map(|value| {
            choices
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(value.trim()))
                .map(|(_, value)| *value)
                .ok_or_else(|| {
                    invalid(&format!(
                        "invalid state <{value}>; expected {}",
                        choices
                            .iter()
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })
        })
        .transpose()
}

/// Fetch all pages only for an explicit --all request; ordinary page commands do one RPC.
async fn fetch_pages<T, F, Fut>(options: &ListOptions, mut fetch: F) -> Result<Page<T>, FlameError>
where
    F: FnMut(PageRequest) -> Fut,
    Fut: Future<Output = Result<Page<T>, FlameError>>,
{
    let mut request = options.page_request();
    let mut result = fetch(request.clone()).await?;
    while options.all && (result.paging.page_number as u64) < result.paging.total_pages {
        request.page_number = result
            .paging
            .page_number
            .checked_add(1)
            .ok_or_else(|| invalid("page number overflow"))?;
        let mut next = fetch(request.clone()).await?;
        result.items.append(&mut next.items);
        result.paging = next.paging;
    }
    Ok(result)
}

/// Send resource-specific filters to the corresponding RPC without fetching other lists.
async fn list_page(
    connection: Connection,
    resource: Resource,
    options: &ListOptions,
) -> Result<(), Box<dyn Error>> {
    match resource {
        Resource::Applications => {
            let filter = ApplicationFilter {
                state: state(
                    &options.state,
                    &[
                        ("Enabled", ApplicationState::Enabled),
                        ("Disabled", ApplicationState::Disabled),
                    ],
                )?,
                names: options.names.clone(),
            };
            let page = fetch_pages(options, |paging| {
                let connection = connection.clone();
                let filter = filter.clone();
                async move { connection.list_applications_page(&filter, &paging).await }
            })
            .await?;
            print_page(&page, &options.output_format, application_table)?;
        }
        Resource::Sessions => {
            let filter = SessionFilter {
                state: state(
                    &options.state,
                    &[
                        ("Open", SessionState::Open),
                        ("Closed", SessionState::Closed),
                        ("Suspending", SessionState::Suspending),
                        ("Suspended", SessionState::Suspended),
                    ],
                )?,
                application: options.application_name.clone(),
                ids: options.ids.clone(),
            };
            let page = fetch_pages(options, |paging| {
                let connection = connection.clone();
                let filter = filter.clone();
                async move { connection.list_sessions_page(&filter, &paging).await }
            })
            .await?;
            print_page(&page, &options.output_format, session_table)?;
        }
        Resource::Nodes => {
            let filter = NodeFilter {
                state: state(
                    &options.state,
                    &[
                        ("Unknown", NodeState::Unknown),
                        ("Ready", NodeState::Ready),
                        ("NotReady", NodeState::NotReady),
                    ],
                )?,
                names: options.names.clone(),
            };
            let page = fetch_pages(options, |paging| {
                let connection = connection.clone();
                let filter = filter.clone();
                async move { connection.list_nodes_page(&filter, &paging).await }
            })
            .await?;
            print_page(&page, &options.output_format, node_table)?;
        }
        Resource::Executors => {
            let filter = ExecutorFilter {
                state: state(
                    &options.state,
                    &[
                        ("Unknown", ExecutorState::Unknown),
                        ("Void", ExecutorState::Void),
                        ("Idle", ExecutorState::Idle),
                        ("Binding", ExecutorState::Binding),
                        ("Bound", ExecutorState::Bound),
                        ("Unbinding", ExecutorState::Unbinding),
                        ("Releasing", ExecutorState::Releasing),
                        ("Released", ExecutorState::Released),
                    ],
                )?,
                application: options.application_name.clone(),
                node: options.node_name.clone(),
                session_id: options.session_id.clone(),
                ids: options.ids.clone(),
            };
            let page = fetch_pages(options, |paging| {
                let connection = connection.clone();
                let filter = filter.clone();
                async move { connection.list_executors_page(&filter, &paging).await }
            })
            .await?;
            print_page(&page, &options.output_format, executor_table)?;
        }
        Resource::Tasks => {
            let filter = TaskFilter {
                state: state(
                    &options.state,
                    &[
                        ("Pending", TaskState::Pending),
                        ("Running", TaskState::Running),
                        ("Succeed", TaskState::Succeed),
                        ("Failed", TaskState::Failed),
                        ("Cancelled", TaskState::Cancelled),
                    ],
                )?,
                ids: options
                    .ids
                    .iter()
                    .map(|id| {
                        id.parse::<i64>()
                            .map_err(|_| invalid("task IDs must be integers"))
                    })
                    .collect::<Result<_, _>>()?,
                include_payloads: options.include_payloads,
            };
            let session = options
                .session_id
                .as_ref()
                .ok_or_else(|| invalid("--task requires --session-id"))?;
            let page = fetch_pages(options, |paging| {
                let connection = connection.clone();
                let filter = filter.clone();
                let session = session.clone();
                async move { connection.list_tasks_page(&session, &filter, &paging).await }
            })
            .await?;
            print_page(&page, &options.output_format, task_table)?;
        }
    }
    Ok(())
}

/// Paged JSON is an envelope; table output shows the page position and totals.
fn print_page<T: Serialize>(
    page: &Page<T>,
    format: &str,
    table: impl FnOnce(&[T]) -> Table,
) -> Result<(), serde_json::Error> {
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(page)?)
    } else {
        println!("{}", table(&page.items));
        println!(
            "Total: {} (resource: {})",
            page.paging.total_count, page.paging.resource_count
        );
        println!(
            "Page: {}/{} (size: {})",
            page.paging.page_number, page.paging.total_pages, page.paging.page_size
        );
    }
    Ok(())
}

fn format_json<T: Serialize>(items: &[T]) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(items)
}

async fn list_applications(conn: Connection, format: &str) -> Result<(), Box<dyn Error>> {
    let items = conn.list_applications().await?;
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

async fn list_sessions(conn: Connection, format: &str) -> Result<(), Box<dyn Error>> {
    let mut items = conn.list_sessions().await?;
    if format == "json" {
        println!("{}", format_json(&items)?)
    } else {
        items.sort_by_key(|session| {
            (
                if session.state == SessionState::Open { 0 } else { 1 },
                session.state as i32,
                session.id.trim().parse::<u32>().unwrap_or(0),
            )
        });
        println!("{}", session_table(&items));
    }
    Ok(())
}

/// Render in supplied order; paged results must retain the server's global ordering.
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

async fn list_executors(conn: Connection, format: &str) -> Result<(), Box<dyn Error>> {
    let items = conn.list_executors().await?;
    if format == "json" {
        println!("{}", format_json(&items)?)
    } else {
        println!("{}", executor_table(&items))
    }
    Ok(())
}

fn executor_table(items: &[Executor]) -> Table {
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_header(["ID", "State", "App", "Session", "Node"]);
    for executor in items {
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

async fn list_nodes(conn: Connection, format: &str) -> Result<(), Box<dyn Error>> {
    let items = conn.list_nodes().await?;
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

fn task_table(items: &[Task]) -> Table {
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_header(["ID", "State", "Session"]);
    for task in items {
        table.add_row([task.id.clone(), task.state.to_string(), task.ssn_id.clone()]);
    }
    table
}

#[cfg(test)]
mod tests;

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

use std::cmp::Ordering;
use std::error::Error;

use comfy_table::presets::NOTHING;
use comfy_table::Table;
use flame_rs as flame;
use flame_rs::apis::{FlameContext, FlameError, SessionState};
use flame_rs::client::{Connection, Executor, NodeState};

use crate::utils::{format_memory, format_resreq};

pub async fn run(
    ctx: &FlameContext,
    application: bool,
    session: bool,
    executor: bool,
    node: bool,
) -> Result<(), Box<dyn Error>> {
    let current_ctx = ctx.get_current_context()?;
    let conn = flame::client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;
    match (application, session, executor, node) {
        (true, _, _, _) => list_applications(conn).await,
        (_, true, _, _) => list_sessions(conn).await,
        (_, _, true, _) => list_executors(conn).await,
        (_, _, _, true) => list_nodes(conn).await,
        _ => Err(Box::new(FlameError::InvalidConfig(
            "unsupported parameters".to_string(),
        ))),
    }
}

async fn list_applications(conn: Connection) -> Result<(), Box<dyn Error>> {
    let app_list = conn.list_applications().await?;

    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_header(vec!["Name", "State", "Shim", "Tags", "Created", "Command"]);

    for app in &app_list {
        table.add_row(vec![
            app.name.to_string(),
            app.state.to_string(),
            app.attributes
                .shim
                .map(|s| s.to_string())
                .unwrap_or("-".to_string()),
            app.attributes.labels.join(", "),
            app.creation_time.format("%T").to_string(),
            app.attributes.command.clone().unwrap_or("-".to_string()),
        ]);
    }

    println!("{table}");

    Ok(())
}

async fn list_sessions(conn: Connection) -> Result<(), Box<dyn Error>> {
    let mut ssn_list = conn.list_sessions().await?;
    let mut table = Table::new();
    table.load_preset(NOTHING).set_header(vec![
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

    ssn_list.sort_by(|l, r| {
        if l.state == r.state {
            let lid: u32 = l.id.trim().parse().unwrap_or(0);
            let rid: u32 = r.id.trim().parse().unwrap_or(0);
            lid.cmp(&rid)
        } else if l.state == SessionState::Open {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });

    for ssn in &ssn_list {
        table.add_row(vec![
            ssn.id.to_string(),
            ssn.state.to_string(),
            ssn.application.to_string(),
            format_resreq(&ssn.resreq),
            ssn.priority.to_string(),
            ssn.pending.to_string(),
            ssn.running.to_string(),
            ssn.succeed.to_string(),
            ssn.failed.to_string(),
            ssn.creation_time.format("%T").to_string(),
        ]);
    }

    println!("{table}");

    Ok(())
}

async fn list_executors(conn: Connection) -> Result<(), Box<dyn Error>> {
    let executor_list = conn.list_executors().await?;
    let table = executor_table(&executor_list);

    println!("{table}");

    Ok(())
}

fn executor_table(executors: &[Executor]) -> Table {
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_header(vec!["ID", "State", "App", "Session", "Node"]);

    for executor in executors {
        table.add_row(vec![
            executor.id.to_string(),
            executor.state.to_string(),
            executor.application.to_string(),
            executor.session_id.clone().unwrap_or("-".to_string()),
            executor.node.to_string(),
        ]);
    }

    table
}

async fn list_nodes(conn: Connection) -> Result<(), Box<dyn Error>> {
    let node_list = conn.list_nodes().await?;
    let mut table = Table::new();
    table.load_preset(NOTHING).set_header(vec![
        "NAME", "HOSTNAME", "STATUS", "CPU", "MEMORY", "ARCH", "OS",
    ]);

    for node in &node_list {
        let status = match node.state {
            NodeState::Ready => "Ready",
            NodeState::NotReady => "NotReady",
            NodeState::Unknown => "Unknown",
        };
        table.add_row(vec![
            node.name.to_string(),
            node.hostname.to_string(),
            status.to_string(),
            node.cpu.to_string(),
            format_memory(node.memory),
            node.arch.to_string(),
            node.os.to_string(),
        ]);
    }

    println!("{table}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use flame_rs::apis::ExecutorState;

    use super::*;

    #[test]
    fn executor_table_shows_application() {
        let table = executor_table(&[Executor {
            id: "executor-1".to_string(),
            application: "app-1".to_string(),
            state: ExecutorState::Idle,
            session_id: None,
            node: "node-1".to_string(),
        }])
        .to_string();

        assert!(table.contains("App"));
        assert!(table.contains("app-1"));
    }
}

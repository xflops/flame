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

use comfy_table::presets::NOTHING;
use comfy_table::Table;
use serde_json::Value;

use flame_rs::apis::{FlameContext, FlameError};
use flame_rs::client::{self, NodeState};

use crate::utils::{format_memory, format_optional_duration, format_resreq};

pub async fn run(
    ctx: &FlameContext,
    output_format: &Option<String>,
    application: &Option<String>,
    session: &Option<String>,
    task: &Option<String>,
    node: &Option<String>,
) -> Result<(), Box<dyn Error>> {
    let current_ctx = ctx.get_current_context()?;
    let conn = client::connect_with_tls(
        &current_ctx.cluster.endpoint,
        current_ctx.cluster.tls.as_ref(),
    )
    .await?;
    match (application, session, task, node) {
        (Some(application), None, None, None) => {
            view_application(conn, output_format, application).await
        }
        (None, Some(session), None, None) => view_session(conn, output_format, session).await,
        (None, Some(session), Some(task), None) => {
            view_task(conn, output_format, session, task).await
        }
        (None, None, None, Some(node)) => view_node(conn, output_format, node).await,
        _ => Err(Box::new(FlameError::InvalidConfig(
            "unsupported parameters".to_string(),
        ))),
    }
}

async fn view_task(
    conn: client::Connection,
    output_format: &Option<String>,
    ssn_id: &String,
    task_id: &String,
) -> Result<(), Box<dyn Error>> {
    let session = conn.get_session(ssn_id).await?;
    let task = session.get_task(task_id).await?;
    if output_format.as_deref() == Some("json") {
        println!("{}", serde_json::to_string_pretty(&task)?);
        return Ok(());
    }

    println!("{:<15}{}", "Task:", task.id);
    println!("{:<15}{}", "Session:", session.id);
    println!("{:<15}{}", "Application:", session.application);
    println!("{:<15}{}", "State:", task.state);
    println!("{:<15}", "Events:");
    print!("{}", format_events(&task.events));

    Ok(())
}

async fn view_session(
    conn: client::Connection,
    output_format: &Option<String>,
    ssn_id: &String,
) -> Result<(), Box<dyn Error>> {
    let mut session = conn.get_session(ssn_id).await?;

    match output_format {
        Some(format) if format.as_str() == "json" => {
            let tasks = session.list_tasks().await?;
            session.tasks = Some(tasks);
            view_session_json(&session)
        }
        _ => view_session_table(&session),
    }
}

fn format_task_summary(pending: i32, running: i32, succeed: i32, failed: i32) -> String {
    format!("{pending} pending, {running} running, {succeed} succeed, {failed} failed")
}

fn view_session_table(session: &client::Session) -> Result<(), Box<dyn Error>> {
    let mut table = Table::new();
    table.load_style(NOTHING);

    table.add_row(vec!["Session:", &session.id.to_string()]);
    table.add_row(vec!["Application:", &session.application.to_string()]);
    table.add_row(vec!["State:", &session.state.to_string()]);
    table.add_row(vec!["Resources:", &format_resreq(&session.resreq)]);
    table.add_row(vec![
        "Creation Time:",
        &session.creation_time.format("%T").to_string(),
    ]);
    table.add_row(vec![
        "Tasks:",
        &format_task_summary(
            session.pending,
            session.running,
            session.succeed,
            session.failed,
        ),
    ]);

    println!("{table}");

    println!("{:<15}", "Events:");
    print!("{}", format_events(&session.events));

    Ok(())
}

fn format_events(events: &[client::Event]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "  {}: {} ({})\n",
                event.creation_time.format("%H:%M:%S%.3f"),
                event.message.clone().unwrap_or_default(),
                event.code
            )
        })
        .collect()
}

fn view_session_json(session: &client::Session) -> Result<(), Box<dyn Error>> {
    let json = serde_json::to_string_pretty(session)?;
    println!("{json}");
    Ok(())
}

async fn view_application(
    conn: client::Connection,
    output_format: &Option<String>,
    application: &str,
) -> Result<(), Box<dyn Error>> {
    let application = conn.get_application(application).await?;
    if output_format.as_deref() == Some("json") {
        println!("{}", serde_json::to_string_pretty(&application)?);
        return Ok(());
    }
    println!("{:<15}{}", "Name:", application.name);
    println!(
        "{:<15}{}",
        "Description:",
        application.attributes.description.unwrap_or_default()
    );
    println!(
        "{:<15}{}",
        "Shim:",
        application
            .attributes
            .shim
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Host".to_string())
    );
    println!(
        "{:<15}{}",
        "Image:",
        application.attributes.image.unwrap_or_default()
    );
    println!(
        "{:<15}{}",
        "URL:",
        application.attributes.url.unwrap_or_default()
    );
    println!(
        "{:<15}{}",
        "Installer:",
        application.attributes.installer.unwrap_or_default()
    );
    println!("{:<15}", "Labels:");
    for label in application.attributes.labels {
        println!("\t{label}");
    }
    println!(
        "{:<15}{}",
        "Command:",
        application.attributes.command.unwrap_or_default()
    );
    println!("{:<15}", "Arguments:");
    for argument in application.attributes.arguments {
        println!("\t{argument}");
    }
    println!("{:<15}", "Environments:");
    for (key, value) in application.attributes.environments {
        println!("\t{key}: {value}");
    }
    println!(
        "{:<15}{}",
        "WorkingDir:",
        application.attributes.working_directory.unwrap_or_default()
    );
    println!(
        "{:<15}{}",
        "Max Instances:",
        application.attributes.max_instances.unwrap_or_default()
    );
    println!(
        "{:<15}{}",
        "Delay Release:",
        format_optional_duration(&application.attributes.delay_release)
    );

    println!("{:<15}", "Schema:");

    if let Some(schema) = application.attributes.schema {
        let input_type = get_type(schema.input)?;
        let output_type = get_type(schema.output)?;
        let common_data_type = get_type(schema.common_data)?;

        println!("  Input: {input_type}");
        println!("  Output: {output_type}");
        println!("  Common Data: {common_data_type}");
    }
    Ok(())
}

async fn view_node(
    conn: client::Connection,
    output_format: &Option<String>,
    node_name: &str,
) -> Result<(), Box<dyn Error>> {
    let node = conn.get_node(node_name).await?;
    if output_format.as_deref() == Some("json") {
        println!("{}", serde_json::to_string_pretty(&node)?);
        return Ok(());
    }

    print!("{}", format_node(&node));
    Ok(())
}

/// Render reported node inventory, using a dash for unavailable information.
fn format_node(node: &client::Node) -> String {
    let status = match node.state {
        NodeState::Ready => "Ready",
        NodeState::NotReady => "NotReady",
        NodeState::Unknown => "Unknown",
    };

    let mut output = [
        format!("{:<15}{}", "Name:", value_or_dash(&node.name)),
        format!("{:<15}{}", "Hostname:", value_or_dash(&node.hostname)),
        format!("{:<15}{}", "Status:", status),
        "Capacity:".into(),
        format!("  {:<13}{}", "CPU:", node.cpu),
        format!("  {:<13}{}", "Memory:", format_memory(node.memory)),
        format!("  {:<13}{}", "GPU:", node.gpu),
        "Info:".into(),
        format!("  {:<13}{}", "Arch:", value_or_dash(&node.arch)),
        format!("  {:<13}{}", "OS:", value_or_dash(&node.os)),
        format!("  {:<13}{}", "OS Image:", value_or_dash(&node.os_image)),
        format!("  {:<13}{}", "Kernel:", value_or_dash(&node.kernel_version)),
        "Addresses:".into(),
    ]
    .join("\n");
    output.push('\n');

    let mut addresses = Table::new();
    addresses.load_style(NOTHING);
    for address in &node.addresses {
        if address.address_type == "Hostname" {
            continue;
        }
        let interface = node.network_interfaces.iter().find(|interface| {
            interface
                .addresses
                .iter()
                .any(|value| value.split('/').next() == Some(address.address.as_str()))
        });
        addresses.add_row([
            interface
                .map(|interface| value_or_dash(&interface.name))
                .unwrap_or("-"),
            value_or_dash(&address.address),
        ]);
    }
    if addresses.is_empty() {
        output.push_str("  -\n");
    } else {
        for line in addresses.to_string().lines() {
            output.push_str(&format!("  {}\n", line.trim()));
        }
    }
    output.push_str("PCI Devices:\n");
    let mut devices = Table::new();
    devices.load_style(NOTHING);
    if node.pci_devices.is_empty() {
        devices.add_row(["-"; 6]);
    } else {
        for device in &node.pci_devices {
            devices.add_row([
                value_or_dash(&device.address).to_string(),
                value_or_dash(&device.vendor_id).to_string(),
                value_or_dash(&device.device_id).to_string(),
                value_or_dash(&device.class_id).to_string(),
                value_or_dash(&device.driver).to_string(),
                device
                    .numa_node
                    .map(|node| node.to_string())
                    .unwrap_or_else(|| "-".into()),
            ]);
        }
    }
    for line in devices.to_string().lines() {
        output.push_str(&format!("  {}\n", line.trim()));
    }
    output
}

/// Keep missing inventory readable in both scalar fields and table cells.
fn value_or_dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

fn get_type(schema: Option<String>) -> Result<String, FlameError> {
    match schema {
        Some(schema) => {
            let value = serde_json::from_str::<Value>(&schema)
                .map_err(|e| FlameError::InvalidConfig(e.to_string()))?;
            let schema_type = value.get("type").ok_or(FlameError::InvalidConfig(
                "schema type is missed".to_string(),
            ))?;
            Ok(schema_type.to_string().trim_matches('\"').to_string())
        }
        None => Ok("-".to_string()),
    }
}

#[cfg(test)]
#[path = "view_node_tests.rs"]
mod node_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use flame_rs::client::Event;

    #[test]
    fn format_events_includes_session_event_details() {
        let event = Event {
            code: 1001,
            message: Some("bind failed".to_string()),
            creation_time: chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 1, 2).unwrap(),
        };

        let formatted = format_events(&[event]);

        assert!(formatted.contains("10:01:02.000"));
        assert!(formatted.contains("bind failed"));
        assert!(formatted.contains("(1001)"));
    }

    #[test]
    fn format_task_summary_includes_all_states() {
        let summary = format_task_summary(3, 1, 2, 1);

        assert_eq!(summary, "3 pending, 1 running, 2 succeed, 1 failed");
        assert!(!summary.contains("4 failed"));
    }
}

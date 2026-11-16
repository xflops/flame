/*
Copyright 2025 The Flame Authors.
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

use clap::Parser;
use flame_rs::apis::ExecutorState;
use flame_rs::client::Executor;

use super::*;
use crate::list::{executor_table, format_json, ListOptions, Resource};

/// Parse real command-line arguments without loading user configuration.
fn options(args: &[&str]) -> Result<ListOptions, clap::Error> {
    let args = std::iter::once("flmctl")
        .chain(std::iter::once("list"))
        .chain(args.iter().copied());
    match Cli::try_parse_from(args)?.command {
        Some(Commands::List(options)) => Ok(options),
        _ => unreachable!(),
    }
}

#[test]
fn selectors_accept_json_output() {
    for selector in ["-a", "-s", "-e", "-n"] {
        let options = options(&[selector, "-o", "json"]).unwrap();
        assert!(options.validate().is_ok());
    }
}

#[test]
fn resource_selection_is_exclusive() {
    assert!(options(&[]).is_err());
    assert!(options(&["-a", "-s"]).is_err());
    assert!(options(&["-t"]).is_err());
    let sessions = options(&["-s"]).unwrap();
    assert_eq!(sessions.resource().unwrap(), Resource::Sessions);
    let page = options(&["-s", "--limit", "2", "--number", "3"])
        .unwrap()
        .page();
    assert_eq!(page.limit, 2);
    assert_eq!(page.number, 3);
    assert!(options(&["-s", "--search", "name"]).is_err());
}

#[test]
fn executor_table_shows_application() {
    let table = executor_table(&[Executor {
        id: "executor-1".into(),
        application: "app-1".into(),
        state: ExecutorState::Idle,
        session_id: None,
        node: "node-1".into(),
    }])
    .to_string();
    assert!(table.contains("App"));
    assert!(table.contains("app-1"));
}

#[test]
fn executor_json_preserves_page_envelope_and_state() {
    let json = format_json(&flame_rs::client::Page {
        items: vec![Executor {
            id: "executor-1".into(),
            application: "app-1".into(),
            state: ExecutorState::Idle,
            session_id: None,
            node: "node-1".into(),
        }],
        paging: flame_rs::client::Paging {
            limit: 20,
            number: 3,
            total: Some(51),
        },
    })
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["paging"]["total"], 51);
    assert_eq!(value["paging"]["number"], 3);
    assert_eq!(value["items"][0]["id"], "executor-1");
    assert_eq!(value["items"][0]["state"], "Idle");
    assert_eq!(value["items"][0]["application"], "app-1");
}

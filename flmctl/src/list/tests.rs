/*
Copyright 2026 The Flame Authors.
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

use super::*;
use crate::{Cli, Commands};
use clap::Parser;

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
fn legacy_selectors_do_not_enable_pagination() {
    for selector in ["-a", "-s", "-e", "-n"] {
        let options = options(&[selector, "-o", "json"]).unwrap();
        assert!(!options.paged());
        assert!(options.validate().is_ok());
    }
}

#[test]
fn resource_selection_is_exclusive_and_tasks_require_session() {
    assert!(options(&[]).is_err());
    assert!(options(&["-a", "-s"]).is_err());
    assert!(options(&["-t"]).is_err());
    let tasks = options(&["-t", "--session-id", "s", "--ids", "1,2"]).unwrap();
    assert!(tasks.paged());
    assert_eq!(tasks.resource().unwrap(), Resource::Tasks);
}

#[test]
fn page_sizes_and_numbers_are_validated() {
    for size in ["0", "1", "1000"] {
        assert!(options(&["-s", "--page-size", size]).is_ok());
    }
    assert!(options(&["-s", "--page-size", "1001"]).is_err());
    assert!(options(&["-s", "--search", "name"]).is_err());
    assert!(options(&["-s", "--all", "--page-number", "2"]).is_err());
    assert!(options(&["-s", "--page-number=-1"]).is_err());
    for number in ["0", "1", "9223372036854775807"] {
        assert!(options(&["-s", "--page-number", number]).is_ok());
    }
    let options = options(&["-s", "--page-size", "2", "--order-by", "creation_time desc"]).unwrap();
    assert_eq!(
        options.page_request(),
        PageRequest {
            page_size: 2,
            order_by: "creation_time desc".into(),
            ..Default::default()
        }
    );
}

#[test]
fn unrelated_resource_filters_are_rejected() {
    for args in [
        vec!["-a", "--ids", "1"],
        vec!["-s", "--names", "node"],
        vec!["-n", "--application-name", "a"],
        vec!["-s", "--node-name", "n"],
        vec!["-n", "--session-id", "s"],
    ] {
        assert!(options(&args).unwrap().validate().is_err());
    }
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
fn executor_json_is_an_array_with_state() {
    let json = format_json(&[Executor {
        id: "executor-1".into(),
        application: "app-1".into(),
        state: ExecutorState::Idle,
        session_id: None,
        node: "node-1".into(),
    }])
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value.is_array());
    assert_eq!(value[0]["state"], "Idle");
    assert_eq!(value[0]["application"], "app-1");
}

#[test]
fn paged_json_keeps_metadata_and_server_row_order() {
    let page = Page {
        items: vec![
            Node {
                name: "z".into(),
                ..Default::default()
            },
            Node {
                name: "a".into(),
                ..Default::default()
            },
        ],
        paging: flame_rs::client::PageInfo {
            total_count: 9,
            page_number: 1,
            page_size: 2,
            total_pages: 2,
            ..Default::default()
        },
    };
    let value = serde_json::to_value(page).unwrap();
    assert_eq!(value["items"][0]["name"], "z");
    assert_eq!(value["paging"]["total_count"], 9);
    assert_eq!(value["paging"]["page_number"], 1);
    assert_eq!(value["paging"]["total_pages"], 2);
}

#[tokio::test]
async fn all_fetches_bounded_pages() {
    use std::sync::{Arc, Mutex};
    let options = options(&["-n", "--all", "--page-size", "1"]).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let page = fetch_pages(&options, move |request| {
        let record = record.clone();
        async move {
            let first = request.page_number == 0;
            record.lock().unwrap().push(request);
            Ok(Page {
                items: vec![if first { "first" } else { "last" }],
                paging: flame_rs::client::PageInfo {
                    page_number: if first { 1 } else { 2 },
                    page_size: 1,
                    total_pages: 2,
                    total_count: 2,
                    ..Default::default()
                },
            })
        }
    })
    .await
    .unwrap();
    assert_eq!(page.items, ["first", "last"]);
    assert_eq!(page.paging.page_number, 2);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].page_size, 1);
    assert_eq!(seen[1].page_number, 2);
}

#[tokio::test]
async fn single_page_keeps_position_without_fetching_the_next_page() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = AtomicUsize::new(0);
    let options = options(&["-n", "--page-size", "2"]).unwrap();
    let result = fetch_pages(&options, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async {
            Ok(Page::<()> {
                items: Vec::new(),
                paging: flame_rs::PageInfo {
                    page_number: 1,
                    page_size: 2,
                    total_pages: 2,
                    ..Default::default()
                },
            })
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.paging.page_number, 1);
    assert_eq!(result.paging.total_pages, 2);
}

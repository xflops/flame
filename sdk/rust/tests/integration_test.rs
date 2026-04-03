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

//! Integration tests for Flame SDK.
//!
//! These tests require a running Flame server.

use std::collections::HashMap;

use futures::future::try_join_all;
use serde_json::json;
use stdng::{lock_ptr, new_ptr};

use flame_rs as flame;

use flame::{
    apis::{FlameClientTls, FlameError, SessionState, TaskState},
    client::{ApplicationAttributes, ApplicationSchema, SessionAttributes, Task, TaskInformer},
};

const FLAME_DEFAULT_ADDR: &str = "https://127.0.0.1:8080";

const FLAME_DEFAULT_APP: &str = "flmping";

fn get_ca_cert_path() -> String {
    let root = std::env::var("FLAME_ROOT").unwrap_or_else(|_| {
        // Fallback: use CARGO_MANIFEST_DIR and navigate up
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        format!("{}/../..", manifest_dir)
    });
    format!("{}/ci/certs/ca.crt", root)
}

/// Helper function to get a TLS-enabled connection
async fn get_connection() -> Result<flame::client::Connection, FlameError> {
    let tls_config = FlameClientTls {
        ca_file: Some(get_ca_cert_path()),
        cert_file: None,
        key_file: None,
    };
    flame::client::connect_with_tls(FLAME_DEFAULT_ADDR, Some(&tls_config)).await
}

pub struct DefaultTaskInformer {
    pub succeed: i32,
    pub failed: i32,
    pub error: i32,
}

impl TaskInformer for DefaultTaskInformer {
    fn on_update(&mut self, task: Task) {
        tracing::info!("task: {:?}", task.state);
        match task.state {
            TaskState::Succeed => self.succeed += 1,
            TaskState::Failed => self.failed += 1,
            _ => {}
        }
    }

    fn on_error(&mut self, _: FlameError) {
        self.error += 1;
        tracing::info!("error: {}", self.error);
    }
}

#[tokio::test]
async fn test_create_session() -> Result<(), FlameError> {
    let conn = get_connection().await?;

    let ssn_attr = SessionAttributes {
        id: String::from("ssn-1-test-create-session"),
        application: FLAME_DEFAULT_APP.to_string(),
        slots: 1,
        common_data: None,
        min_instances: 0,
        max_instances: None,
    };
    let ssn = conn.create_session(&ssn_attr).await?;

    assert_eq!(ssn.state, SessionState::Open);

    ssn.close().await?;

    Ok(())
}

#[tokio::test]
async fn test_create_multiple_sessions() -> Result<(), FlameError> {
    let conn = get_connection().await?;

    let ssn_num = 10;

    for i in 0..ssn_num {
        let ssn_attr = SessionAttributes {
            id: format!("ssn-1-test-create-multiple-sessions-{}", i),
            application: FLAME_DEFAULT_APP.to_string(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
        };
        let ssn = conn.create_session(&ssn_attr).await?;

        assert_eq!(ssn.state, SessionState::Open);

        ssn.close().await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_create_session_with_tasks() -> Result<(), FlameError> {
    let conn = get_connection().await?;

    let ssn_attr = SessionAttributes {
        id: String::from("ssn-1-test-create-session-with-tasks"),
        application: FLAME_DEFAULT_APP.to_string(),
        slots: 1,
        common_data: None,
        min_instances: 0,
        max_instances: None,
    };
    let ssn = conn.create_session(&ssn_attr).await?;

    assert_eq!(ssn.state, SessionState::Open);

    let informer = new_ptr(DefaultTaskInformer {
        succeed: 0,
        failed: 0,
        error: 0,
    });

    let task_num = 100;
    let mut tasks = vec![];
    for _ in 0..task_num {
        let task = ssn.run_task(None, informer.clone());
        tasks.push(task);
    }

    try_join_all(tasks).await?;

    {
        let informer = lock_ptr!(informer)?;
        assert_eq!(informer.succeed, task_num);
    }

    // Also check the events of the task.
    let task = ssn.get_task(&String::from("1")).await?;
    assert_eq!(task.state, TaskState::Succeed);
    assert_ne!(task.events.len(), 0);
    for event in task.events {
        if event.code > 15 {
            // The first 16 codes are reserved as Flame system code.
            continue;
        }

        assert!(
            event.code == TaskState::Succeed as i32
                || event.code == TaskState::Pending as i32
                || event.code == TaskState::Running as i32,
            "event code <{}> is not valid",
            event.code
        );
    }

    ssn.close().await?;

    Ok(())
}

#[tokio::test]
async fn test_create_multiple_sessions_with_tasks() -> Result<(), FlameError> {
    let conn = get_connection().await?;

    let ssn_1_attr = SessionAttributes {
        id: String::from("ssn-1-test-create-multiple-sessions-with-tasks"),
        application: FLAME_DEFAULT_APP.to_string(),
        slots: 1,
        common_data: None,
        min_instances: 0,
        max_instances: None,
    };
    let ssn_1 = conn.create_session(&ssn_1_attr).await?;
    assert_eq!(ssn_1.state, SessionState::Open);

    let ssn_2_attr = SessionAttributes {
        id: String::from("ssn-2-test-create-multiple-sessions-with-tasks"),
        application: FLAME_DEFAULT_APP.to_string(),
        slots: 1,
        common_data: None,
        min_instances: 0,
        max_instances: None,
    };
    let ssn_2 = conn.create_session(&ssn_2_attr).await?;
    assert_eq!(ssn_2.state, SessionState::Open);

    let informer = new_ptr(DefaultTaskInformer {
        succeed: 0,
        failed: 0,
        error: 0,
    });

    let task_num = 100;
    let mut tasks = vec![];

    for _ in 0..task_num {
        let task = ssn_1.run_task(None, informer.clone());
        tasks.push(task);
    }

    for _ in 0..task_num {
        let task = ssn_2.run_task(None, informer.clone());
        tasks.push(task);
    }

    try_join_all(tasks).await?;

    {
        let informer = lock_ptr!(informer)?;
        assert_eq!(informer.succeed, task_num * 2);
    }

    ssn_1.close().await?;
    ssn_2.close().await?;

    Ok(())
}

#[tokio::test]
async fn test_application_lifecycle() -> Result<(), FlameError> {
    let conn = get_connection().await?;

    let string_schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "string",
        "description": "The string for testing."
    });

    let apps = vec![
        (
            "my-test-agent-1".to_string(),
            ApplicationAttributes {
                shim: None,
                image: Some("may-agent".to_string()),
                description: Some("This is my agent for testing.".to_string()),
                labels: vec!["test".to_string(), "agent".to_string()],
                command: Some("my-agent".to_string()),
                arguments: vec!["--test".to_string(), "--agent".to_string()],
                environments: HashMap::from([("TEST".to_string(), "true".to_string())]),
                working_directory: Some("/tmp".to_string()),
                max_instances: Some(10),
                delay_release: None,
                schema: Some(ApplicationSchema {
                    input: Some(string_schema.to_string()),
                    output: Some(string_schema.to_string()),
                    common_data: None,
                }),
                url: None,
            },
        ),
        (
            "empty-app".to_string(),
            ApplicationAttributes {
                shim: None,
                image: None,
                description: None,
                labels: vec![],
                command: None,
                arguments: vec![],
                environments: HashMap::new(),
                working_directory: None,
                max_instances: None,
                delay_release: None,
                schema: None,
                url: None,
            },
        ),
    ];

    for (name, app_attr) in apps {
        conn.register_application(name.clone(), app_attr)
            .await
            .map_err(|e| {
                FlameError::Internal(format!("failed to register application <{name}>: {e}"))
            })?;
    }

    Ok(())
}

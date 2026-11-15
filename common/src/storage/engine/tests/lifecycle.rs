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

//! Application and session lifecycle contracts for persistent storage.
use super::*;
use crate::apis::{ApplicationSchema, Shim};
use chrono::{Duration, Utc};
use serde_json::json;
use std::collections::HashMap;

fn test_applications() -> Vec<(String, ApplicationAttributes)> {
    ["flmexec", "flmping", "flmrun"]
        .into_iter()
        .map(|name| (name.to_string(), ApplicationAttributes::default()))
        .collect()
}

#[test]
fn test_create_session_stores_tokens() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let ssn_id = format!("ssn-tokens-{}", Utc::now().timestamp());
    tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: [("service".into(), "credential".into())].into(),
        id: ssn_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 2,
        max_instances: Some(4),
        priority: 0,
        resreq: None,
    }))?;

    let ssn = tokio_test::block_on(storage.get_session(ssn_id))?;
    assert_eq!(
        ssn.tokens.get("service").map(String::as_str),
        Some("credential")
    );

    Ok(())
}

#[test]
fn test_get_task_with_events() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;
    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_1.id, 1);
    let tasks =
        tokio_test::block_on(storage.find_tasks(&TaskFilter::by_session(ssn_1.id.clone())))?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, 1);
    assert_eq!(tasks[0].ssn_id, ssn_1.id.clone());
    assert_eq!(tasks[0].state, TaskState::Pending);
    assert_eq!(tasks[0].input, None);
    assert_eq!(tasks[0].output, None);

    let task_1_1 = tokio_test::block_on(storage.update_task_state(
        task_1_1.clone(),
        TaskState::Succeed,
        Some("Task succeeded".to_string()),
    ))?;
    assert_eq!(task_1_1.state, TaskState::Succeed);
    let tasks =
        tokio_test::block_on(storage.find_tasks(&TaskFilter::by_session(ssn_1.id.clone())))?;

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, 1);
    assert_eq!(tasks[0].ssn_id, ssn_1.id.clone());
    assert_eq!(tasks[0].state, TaskState::Succeed);

    Ok(())
}

#[test]
fn test_update_application() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let app_1 = tokio_test::block_on(storage.get_application("flmexec".to_string()))?;
    assert_eq!(app_1.name, "flmexec");
    assert_eq!(app_1.state, ApplicationState::Enabled);

    let app_2 = tokio_test::block_on(storage.update_application(
        "flmexec".to_string(),
        ApplicationAttributes {
            shim: Shim::Cri,
            description: Some("This is my agent for testing.".to_string()),
            labels: vec!["test".to_string(), "agent".to_string()],
            image: Some("may-agent".to_string()),
            command: Some("run-agent".to_string()),
            arguments: vec!["--test".to_string(), "--agent".to_string()],
            environments: HashMap::from([("TEST".to_string(), "true".to_string())]),
            working_directory: Some("/tmp".to_string()),
            max_instances: 10,
            delay_release: Duration::seconds(0),
            schema: None,
            url: None,
            installer: None,
        },
    ))?;
    assert_eq!(app_2.name, "flmexec");
    assert_eq!(app_2.shim, Shim::Cri);
    assert_eq!(app_2.image.as_deref(), Some("may-agent"));
    assert_eq!(
        app_2.description,
        Some("This is my agent for testing.".to_string())
    );
    assert_eq!(app_2.labels, vec!["test".to_string(), "agent".to_string()]);
    assert_eq!(app_2.command, Some("run-agent".to_string()));
    assert_eq!(
        app_2.arguments,
        vec!["--test".to_string(), "--agent".to_string()]
    );
    assert_eq!(
        app_2.environments,
        HashMap::from([("TEST".to_string(), "true".to_string())])
    );
    assert_eq!(app_2.working_directory, Some("/tmp".to_string()));
    assert_eq!(app_2.max_instances, 10);
    assert_eq!(app_2.delay_release, Duration::seconds(0));
    assert!(app_2.schema.is_none());

    Ok(())
}

#[test]
fn test_application_state_update_and_filter() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    tokio_test::block_on(
        storage.register_application("enabled-app".to_string(), ApplicationAttributes::default()),
    )?;
    tokio_test::block_on(
        storage.register_application("disabled-app".to_string(), ApplicationAttributes::default()),
    )?;

    let disabled = tokio_test::block_on(
        storage.update_application_state("disabled-app".to_string(), ApplicationState::Disabled),
    )?;
    assert_eq!(disabled.version, 2);
    let unchanged = tokio_test::block_on(
        storage.update_application_state("disabled-app".to_string(), ApplicationState::Disabled),
    )?;
    assert_eq!(unchanged.version, disabled.version);

    let result = tokio_test::block_on(storage.update_application(
        "disabled-app".to_string(),
        ApplicationAttributes {
            image: Some("must-not-be-written".to_string()),
            ..Default::default()
        },
    ));
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    let unchanged = tokio_test::block_on(storage.get_application("disabled-app".to_string()))?;
    assert_eq!(unchanged.version, disabled.version);
    assert_eq!(unchanged.image, disabled.image);

    let filter = ApplicationFilter::by_state(ApplicationState::Disabled);
    let apps = tokio_test::block_on(storage.find_applications(Some(&filter)))?;
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "disabled-app");

    let result = tokio_test::block_on(storage.delete_application("enabled-app".to_string()));
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    Ok(())
}

#[test]
fn test_unregister_application() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());

    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;
    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id, None, None))?;
    assert_eq!(task_1_1.id, 1);
    let res = tokio_test::block_on(storage.delete_application("flmexec".to_string()));
    assert!(res.is_err());

    let task_1_1 = tokio_test::block_on(storage.get_task(task_1_1.gid()))?;
    assert_eq!(task_1_1.state, TaskState::Pending);

    let task_1_1 = tokio_test::block_on(storage.update_task_state(
        task_1_1.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_1.state, TaskState::Succeed);

    let res = tokio_test::block_on(storage.delete_application("flmexec".to_string()));
    assert!(res.is_err());

    let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);

    tokio_test::block_on(
        storage.update_application_state("flmexec".to_string(), ApplicationState::Disabled),
    )?;
    let res = tokio_test::block_on(storage.delete_application("flmexec".to_string()));
    assert!(matches!(res, Err(FlameError::InvalidState(_))));

    let list_ssn = tokio_test::block_on(storage.find_sessions())?;
    assert_eq!(list_ssn.len(), 1);
    assert_eq!(list_ssn[0].id, ssn_1_id);

    tokio_test::block_on(storage.delete_session(ssn_1_id))?;
    tokio_test::block_on(storage.delete_application("flmexec".to_string()))?;

    let app_1 = tokio_test::block_on(storage.get_application("flmexec".to_string()));
    assert!(app_1.is_err());

    let list_ssn = tokio_test::block_on(storage.find_sessions())?;
    assert_eq!(list_ssn.len(), 0);

    Ok(())
}

#[test]
fn test_register_application() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    let string_schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "string",
        "description": "The string for testing."
    });

    let apps = vec![
        (
            "my-test-agent-1".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                image: Some("may-agent".to_string()),
                description: Some("This is my agent for testing.".to_string()),
                labels: vec!["test".to_string(), "agent".to_string()],
                command: Some("my-agent".to_string()),
                arguments: vec!["--test".to_string(), "--agent".to_string()],
                environments: HashMap::from([("TEST".to_string(), "true".to_string())]),
                working_directory: Some("/tmp".to_string()),
                max_instances: 10,
                delay_release: Duration::seconds(0),
                schema: Some(ApplicationSchema {
                    input: Some(string_schema.to_string()),
                    output: Some(string_schema.to_string()),
                    common_data: None,
                }),
                url: None,
                installer: None,
            },
        ),
        (
            "empty-app".to_string(),
            ApplicationAttributes {
                shim: Shim::Host,
                image: None,
                description: None,
                labels: vec![],
                command: None,
                arguments: vec![],
                environments: HashMap::new(),
                working_directory: Some("/tmp".to_string()),
                max_instances: 10,
                delay_release: Duration::seconds(0),
                schema: None,
                url: None,
                installer: None,
            },
        ),
    ];
    for (name, attr) in apps {
        let expected_shim = attr.shim;
        let expected_image = attr.image.clone();
        tokio_test::block_on(storage.register_application(name.clone(), attr)).map_err(|e| {
            FlameError::Storage(format!("failed to register application <{name}>: {e}"))
        })?;
        let app_1 = tokio_test::block_on(storage.get_application(name.clone()))
            .map_err(|e| FlameError::Storage(format!("failed to get application <{name}>: {e}")))?;

        assert_eq!(app_1.name, name);
        assert_eq!(app_1.state, ApplicationState::Enabled);
        assert_eq!(app_1.shim, expected_shim);
        assert_eq!(app_1.image, expected_image);
    }

    Ok(())
}

#[test]
fn test_register_duplicate_application_returns_already_exists() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    tokio_test::block_on(
        storage.register_application("duplicate".to_string(), ApplicationAttributes::default()),
    )?;
    let error = tokio_test::block_on(
        storage.register_application("duplicate".to_string(), ApplicationAttributes::default()),
    )
    .unwrap_err();

    assert!(matches!(error, FlameError::AlreadyExist(_)));
    Ok(())
}

#[test]
fn test_get_application() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let app_1 = tokio_test::block_on(storage.get_application("flmexec".to_string()))?;

    assert_eq!(app_1.name, "flmexec");
    assert_eq!(app_1.state, ApplicationState::Enabled);

    Ok(())
}

#[test]
fn test_register_application_with_url() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    let test_url = "file:///opt/test-package.whl".to_string();

    // Register application with URL
    let app = tokio_test::block_on(storage.register_application(
        "flmtestapp-url".to_string(),
        ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: Some("Test application with URL".to_string()),
            labels: vec!["test".to_string()],
            command: Some("/usr/bin/uv".to_string()),
            arguments: vec![
                "run".to_string(),
                "-n".to_string(),
                "flamepy.app.runpy".to_string(),
            ],
            environments: HashMap::new(),
            working_directory: Some("/tmp".to_string()),
            max_instances: 5,
            delay_release: Duration::seconds(10),
            schema: None,
            url: Some(test_url.clone()),
            installer: None,
        },
    ))?;

    // Verify application was registered with URL
    assert_eq!(app.name, "flmtestapp-url");
    assert_eq!(app.url, Some(test_url.clone()));
    assert_eq!(
        app.description,
        Some("Test application with URL".to_string())
    );
    assert_eq!(app.state, ApplicationState::Enabled);

    // Retrieve and verify URL persisted
    let retrieved_app =
        tokio_test::block_on(storage.get_application("flmtestapp-url".to_string()))?;
    assert_eq!(retrieved_app.name, "flmtestapp-url");
    assert_eq!(retrieved_app.url, Some(test_url));
    assert_eq!(
        retrieved_app.description,
        Some("Test application with URL".to_string())
    );
    assert_eq!(retrieved_app.state, ApplicationState::Enabled);

    Ok(())
}

#[test]
fn test_register_application_without_url() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    // Register application without URL (backward compatibility test)
    let app = tokio_test::block_on(storage.register_application(
        "flmtestapp-no-url".to_string(),
        ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: Some("Test application without URL".to_string()),
            labels: vec!["test".to_string()],
            command: Some("/usr/bin/test".to_string()),
            arguments: vec![],
            environments: HashMap::new(),
            working_directory: Some("/tmp".to_string()),
            max_instances: 5,
            delay_release: Duration::seconds(10),
            schema: None,
            url: None,
            installer: None,
        },
    ))?;

    // Verify application was registered without URL
    assert_eq!(app.name, "flmtestapp-no-url");
    assert_eq!(app.url, None);
    assert_eq!(
        app.description,
        Some("Test application without URL".to_string())
    );
    assert_eq!(app.state, ApplicationState::Enabled);

    // Retrieve and verify URL is None
    let retrieved_app =
        tokio_test::block_on(storage.get_application("flmtestapp-no-url".to_string()))?;
    assert_eq!(retrieved_app.name, "flmtestapp-no-url");
    assert_eq!(retrieved_app.url, None);
    assert_eq!(retrieved_app.state, ApplicationState::Enabled);

    Ok(())
}

#[test]
fn test_update_application_with_url() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;

    // Register initial application without URL
    tokio_test::block_on(storage.register_application(
        "flmtestapp-update".to_string(),
        ApplicationAttributes {
            shim: Shim::Host,
            image: None,
            description: Some("Initial description".to_string()),
            labels: vec![],
            command: Some("/usr/bin/test".to_string()),
            arguments: vec![],
            environments: HashMap::new(),
            working_directory: Some("/tmp".to_string()),
            max_instances: 5,
            delay_release: Duration::seconds(10),
            schema: None,
            url: None,
            installer: None,
        },
    ))?;

    let app_before =
        tokio_test::block_on(storage.get_application("flmtestapp-update".to_string()))?;
    assert_eq!(app_before.url, None);

    // Update application with URL
    let test_url = "file:///opt/updated-package.whl".to_string();
    let updated_app = tokio_test::block_on(storage.update_application(
        "flmtestapp-update".to_string(),
        ApplicationAttributes {
            shim: Shim::Host,
            image: Some("updated-image".to_string()),
            description: Some("Updated description".to_string()),
            labels: vec!["updated".to_string()],
            command: Some("/usr/bin/uv".to_string()),
            arguments: vec!["run".to_string()],
            environments: HashMap::from([("ENV".to_string(), "test".to_string())]),
            working_directory: Some("/opt".to_string()),
            max_instances: 10,
            delay_release: Duration::seconds(20),
            schema: None,
            url: Some(test_url.clone()),
            installer: None,
        },
    ))?;

    // Verify update including URL
    assert_eq!(updated_app.name, "flmtestapp-update");
    assert_eq!(updated_app.url, Some(test_url.clone()));
    assert_eq!(
        updated_app.description,
        Some("Updated description".to_string())
    );
    // Note: image field is not updated by update_application method
    assert_eq!(updated_app.working_directory, Some("/opt".to_string()));
    assert_eq!(updated_app.max_instances, 10);

    // Retrieve and verify URL persisted after update
    let retrieved_app =
        tokio_test::block_on(storage.get_application("flmtestapp-update".to_string()))?;
    assert_eq!(retrieved_app.url, Some(test_url));
    assert_eq!(
        retrieved_app.description,
        Some("Updated description".to_string())
    );

    Ok(())
}

#[test]
fn test_single_session() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_1.id, 1);

    let task_1_2 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_2.id, 2);

    let task_list = tokio_test::block_on(storage.find_tasks(&TaskFilter::by_session(ssn_1.id)))?;
    assert_eq!(task_list.len(), 2);

    let task_1_1 = tokio_test::block_on(storage.update_task_state(
        task_1_1.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_1.state, TaskState::Succeed);

    let task_1_2 = tokio_test::block_on(storage.update_task_state(
        task_1_2.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_2.state, TaskState::Succeed);

    let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);

    Ok(())
}

#[test]
fn test_multiple_session() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }

    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_1.id, 1);

    let task_1_2 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_2.id, 2);

    let task_1_1 = tokio_test::block_on(storage.update_task_state(
        task_1_1.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_1.state, TaskState::Succeed);

    let task_1_2 = tokio_test::block_on(storage.update_task_state(
        task_1_2.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_2.state, TaskState::Succeed);

    let ssn_2_id = format!("ssn-2-{}", Utc::now().timestamp());
    let ssn_2 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_2_id.clone(),
        application: "flmping".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_2.id, ssn_2_id);
    assert_eq!(ssn_2.application, "flmping");
    assert_eq!(ssn_2.status.state, SessionState::Open);

    let task_2_1 = tokio_test::block_on(storage.create_task(ssn_2.id.clone(), None, None))?;
    assert_eq!(task_2_1.id, 1);

    let task_2_2 = tokio_test::block_on(storage.create_task(ssn_2.id.clone(), None, None))?;
    assert_eq!(task_2_2.id, 2);

    let task_2_1 = tokio_test::block_on(storage.update_task_state(
        task_2_1.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_2_1.state, TaskState::Succeed);

    let task_2_2 = tokio_test::block_on(storage.update_task_state(
        task_2_2.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_2_2.state, TaskState::Succeed);

    let ssn_list = tokio_test::block_on(storage.find_sessions())?;
    assert_eq!(ssn_list.len(), 2);

    let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);
    let ssn_2 = tokio_test::block_on(storage.close_session(ssn_2_id.clone()))?;
    assert_eq!(ssn_2.status.state, SessionState::Closed);

    Ok(())
}

#[test]
fn test_close_session_with_open_tasks() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }
    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_1.id, 1);

    let task_1_2 = tokio_test::block_on(storage.create_task(ssn_1.id, None, None))?;
    assert_eq!(task_1_2.id, 2);

    let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);

    let task_1_1 = tokio_test::block_on(storage.get_task(task_1_1.gid()))?;
    assert_eq!(task_1_1.state, TaskState::Cancelled);

    let task_1_2 = tokio_test::block_on(storage.get_task(task_1_2.gid()))?;
    assert_eq!(task_1_2.state, TaskState::Cancelled);

    Ok(())
}

#[test]
fn test_close_session_with_running_tasks() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }
    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_1.state, TaskState::Pending);

    tokio_test::block_on(storage.update_task_state(task_1_1.clone(), TaskState::Running, None))?;

    let res = tokio_test::block_on(storage.close_session(ssn_1_id.clone()));
    assert!(res.is_err());

    Ok(())
}

#[test]
fn test_create_task_for_close_session() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());

    let storage = tokio_test::block_on(connect(&url))?;
    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }
    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id, None, None))?;
    assert_eq!(task_1_1.id, 1);

    let task_1_1 = tokio_test::block_on(storage.update_task_state(
        task_1_1.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_1.state, TaskState::Succeed);

    let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);

    let res = tokio_test::block_on(storage.create_task(ssn_1.id, None, None));
    assert!(res.is_err());

    Ok(())
}

#[test]
fn test_delete_session_with_open_tasks() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    for (name, attr) in test_applications() {
        tokio_test::block_on(storage.register_application(name.clone(), attr))?;
    }
    let ssn_1_id = format!("ssn-1-{}", Utc::now().timestamp());
    let ssn_1 = tokio_test::block_on(storage.create_session(SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: ssn_1_id.clone(),
        application: "flmexec".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    }))?;

    assert_eq!(ssn_1.id, ssn_1_id);
    assert_eq!(ssn_1.application, "flmexec");
    assert_eq!(ssn_1.status.state, SessionState::Open);

    let task_1_1 = tokio_test::block_on(storage.create_task(ssn_1.id.clone(), None, None))?;
    assert_eq!(task_1_1.id, 1);

    // It should be failed because the session is open and there are open tasks
    let res = tokio_test::block_on(storage.delete_session(ssn_1_id.clone()));
    assert!(res.is_err());

    let task_1_1 = tokio_test::block_on(storage.get_task(task_1_1.gid()))?;
    assert_eq!(task_1_1.state, TaskState::Pending);

    let task_1_1 = tokio_test::block_on(storage.update_task_state(
        task_1_1.clone(),
        TaskState::Succeed,
        None,
    ))?;
    assert_eq!(task_1_1.state, TaskState::Succeed);

    // It should be failed because the session is open
    let res = tokio_test::block_on(storage.delete_session(ssn_1_id.clone()));
    assert!(res.is_err());

    let ssn_1 = tokio_test::block_on(storage.close_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);

    let ssn_1 = tokio_test::block_on(storage.delete_session(ssn_1_id.clone()))?;
    assert_eq!(ssn_1.status.state, SessionState::Closed);

    Ok(())
}

#[test]
fn test_delete_session_with_cancelled_tasks() -> Result<(), FlameError> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let storage = tokio_test::block_on(connect(&url))?;
    tokio_test::block_on(
        storage.register_application("flmexec".to_string(), ApplicationAttributes::default()),
    )?;
    let session_id = format!(
        "cancelled-tasks-{}",
        Utc::now().timestamp_nanos_opt().unwrap()
    );
    tokio_test::block_on(storage.create_session(SessionAttributes {
        id: session_id.clone(),
        application: "flmexec".to_string(),
        ..Default::default()
    }))?;
    let task = tokio_test::block_on(storage.create_task(session_id.clone(), None, None))?;

    tokio_test::block_on(storage.close_session(session_id.clone()))?;
    assert_eq!(
        tokio_test::block_on(storage.get_task(task.gid()))?.state,
        TaskState::Cancelled
    );
    let deleted = tokio_test::block_on(storage.delete_session(session_id))?;
    assert_eq!(deleted.status.state, SessionState::Closed);

    Ok(())
}

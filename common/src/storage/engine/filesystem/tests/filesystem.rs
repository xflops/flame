/*
Copyright 2025 The xflops Authors.
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
use tempfile::TempDir;

async fn create_test_engine() -> (FilesystemEngine, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let url = format!("filesystem://{}", temp_dir.path().display());
    let _engine_ptr = FilesystemEngine::new_ptr(&url).await.unwrap();

    let engine = FilesystemEngine {
        base_path: temp_dir.path().to_path_buf(),
        record_size: task_record_size(),
        sessions: RwLock::new(HashMap::new()),
        applications: RwLock::new(HashMap::new()),
        nodes: RwLock::new(HashMap::new()),
        executors: RwLock::new(HashMap::new()),
    };

    (engine, temp_dir)
}

#[tokio::test]
async fn history_and_recovery_reads_reject_corrupt_metadata() {
    let (engine, _dir) = create_test_engine().await;
    engine
        .register_application("corrupt".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    engine
        .create_session(SessionAttributes {
            id: "corrupt".into(),
            application: "corrupt".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    engine
        .create_task("corrupt".into(), None, None)
        .await
        .unwrap();
    let path = engine.session_path("corrupt").join("tasks.bin");
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    fs::write(path, bytes).unwrap();
    assert!(engine
        .find_tasks(&TaskFilter::by_session("corrupt"))
        .await
        .is_err());
    assert!(engine
        .find_tasks(&TaskFilter::terminal("corrupt"))
        .await
        .is_err());
}

#[tokio::test]
async fn finished_reads_skip_active_payloads_and_release_task_gates() {
    let (engine, _dir) = create_test_engine().await;
    engine
        .register_application("scan".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    engine
        .create_session(SessionAttributes {
            id: "scan".into(),
            application: "scan".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for _ in 0..16 {
        let task = engine.create_task("scan".into(), None, None).await.unwrap();
        engine
            .update_task_result(
                task.clone(),
                TaskResult {
                    state: TaskState::Succeed,
                    output: Some(Bytes::from("history")),
                    message: None,
                },
            )
            .await
            .unwrap();
    }
    engine
        .create_task("scan".into(), Some(Bytes::from("active input")), None)
        .await
        .unwrap();
    // Finished reads filter metadata before loading active task payloads.
    fs::write(engine.session_path("scan").join("inputs.bin"), []).unwrap();
    assert_eq!(
        engine
            .find_tasks(&TaskFilter::terminal("scan"))
            .await
            .unwrap()
            .len(),
        16
    );
    assert!(engine
        .find_tasks(&TaskFilter::by_session("scan"))
        .await
        .is_err());
    // Corrupt finished payloads must fail both recovery and history reads.
    fs::write(engine.session_path("scan").join("outputs.bin"), []).unwrap();
    assert!(engine
        .find_tasks(&TaskFilter::terminal("scan"))
        .await
        .is_err());
    assert!(engine
        .find_tasks(&TaskFilter::by_session("scan"))
        .await
        .is_err());
    // Closing counts metadata without loading corrupted task payloads.
    let closed = engine.close_session("scan".into()).await.unwrap();
    assert_eq!(closed.task_counts.succeed, 16);
    assert_eq!(closed.task_counts.cancelled, 1);
    let repeated = engine.close_session("scan".into()).await.unwrap();
    assert_eq!(repeated.task_counts, closed.task_counts);
    let shared = engine.session_entry("scan").unwrap();
    let entry = read_async!(shared);
    assert!(entry.tasks.lock().is_empty());
    {
        let gate = entry.task_lock(1);
        entry.with_task_read(1, || ());
        assert!(Arc::ptr_eq(&gate, &entry.task_lock(1)));
    }
    entry.with_task_read(1, || ());
    assert!(entry.tasks.lock().is_empty());
}

#[tokio::test]
async fn test_record_size_is_constant() {
    let size1 = task_record_size();
    let size2 = task_record_size();
    assert_eq!(size1, size2);

    // Verify different metadata values produce same size
    let meta1 = TaskMetadata::default();
    let meta2 = TaskMetadata {
        id: u64::MAX,
        version: u32::MAX,
        checksum: u32::MAX,
        state: 255,
        creation_time: i64::MAX,
        completion_time: i64::MAX,
        input_offset: u64::MAX,
        input_len: u64::MAX,
        output_offset: u64::MAX,
        output_len: u64::MAX,
        affinity_offset: u64::MAX,
        affinity_len: u64::MAX,
    };

    let buf1 = postcard::to_allocvec(&meta1).unwrap();
    let buf2 = postcard::to_allocvec(&meta2).unwrap();

    assert_eq!(buf1.len(), buf2.len());
}

#[tokio::test]
async fn test_checksum_calculation() {
    let meta = TaskMetadata {
        id: 1,
        version: 1,
        checksum: 0,
        state: TaskState::Pending as u8,
        creation_time: 1234567890,
        completion_time: 0,
        input_offset: 0,
        input_len: 100,
        output_offset: 0,
        output_len: 0,
        affinity_offset: 0,
        affinity_len: 0,
    };

    let checksum1 = calculate_checksum(&meta);
    let checksum2 = calculate_checksum(&meta);

    assert_eq!(checksum1, checksum2);

    // Different metadata should produce different checksum
    let meta2 = TaskMetadata { id: 2, ..meta };

    let checksum3 = calculate_checksum(&meta2);
    assert_ne!(checksum1, checksum3);
}

#[tokio::test]
async fn test_url_parsing() {
    std::env::set_var("FLAME_HOME", "/opt/flame");

    let path1 = FilesystemEngine::parse_url("filesystem:///var/lib/flame").unwrap();
    assert_eq!(path1, PathBuf::from("/var/lib/flame"));

    let path2 = FilesystemEngine::parse_url("file:///tmp/flame").unwrap();
    assert_eq!(path2, PathBuf::from("/tmp/flame"));

    let path3 = FilesystemEngine::parse_url("fs:///data").unwrap();
    assert_eq!(path3, PathBuf::from("/data"));

    let path4 = FilesystemEngine::parse_url("fs://data").unwrap();
    assert_eq!(path4, PathBuf::from("/opt/flame/data"));

    let path5 = FilesystemEngine::parse_url("filesystem://data/sessions").unwrap();
    assert_eq!(path5, PathBuf::from("/opt/flame/data/sessions"));

    let path6 = FilesystemEngine::parse_url("file://storage").unwrap();
    assert_eq!(path6, PathBuf::from("/opt/flame/storage"));

    std::env::remove_var("FLAME_HOME");

    let err = FilesystemEngine::parse_url("sqlite:///tmp/flame.db");
    assert!(err.is_err());
}

#[tokio::test]
async fn test_application_lifecycle() {
    let (engine, _temp_dir) = create_test_engine().await;

    // Register application
    let attr = ApplicationAttributes {
        shim: Shim::Host,
        image: Some("test-image".to_string()),
        description: Some("Test application".to_string()),
        labels: vec!["test".to_string()],
        command: Some("/bin/test".to_string()),
        arguments: vec!["--arg1".to_string()],
        environments: std::collections::HashMap::new(),
        working_directory: Some("/tmp".to_string()),
        max_instances: 10,
        delay_release: Duration::seconds(60),
        schema: None,
        url: None,
        installer: None,
    };

    let app = engine
        .register_application("test-app".to_string(), attr.clone())
        .await
        .unwrap();
    assert_eq!(app.name, "test-app");
    assert_eq!(app.state, ApplicationState::Enabled);

    // Get application
    let app2 = engine
        .get_application("test-app".to_string())
        .await
        .unwrap();
    assert_eq!(app2.name, "test-app");

    // Find applications
    let apps = engine.find_applications(None).await.unwrap();
    assert_eq!(apps.len(), 1);

    // Update application
    let updated_attr = ApplicationAttributes {
        shim: Shim::Cri,
        image: Some("updated-image".to_string()),
        description: Some("Updated description".to_string()),
        ..attr
    };
    let app3 = engine
        .update_application("test-app".to_string(), updated_attr)
        .await
        .unwrap();
    assert_eq!(app3.description, Some("Updated description".to_string()));
    assert_eq!(app3.shim, Shim::Cri);
    assert_eq!(app3.image.as_deref(), Some("updated-image"));
    assert_eq!(app3.version, 2);

    let result = engine.delete_application("test-app".to_string()).await;
    assert!(matches!(result, Err(FlameError::InvalidState(_))));

    let disabled = engine
        .update_application_state("test-app".to_string(), ApplicationState::Disabled)
        .await
        .unwrap();
    assert_eq!(disabled.version, 3);
    let unchanged = engine
        .update_application_state("test-app".to_string(), ApplicationState::Disabled)
        .await
        .unwrap();
    assert_eq!(unchanged.version, disabled.version);

    let result = engine
        .update_application(
            "test-app".to_string(),
            ApplicationAttributes {
                image: Some("must-not-be-written".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    let unchanged = engine
        .get_application("test-app".to_string())
        .await
        .unwrap();
    assert_eq!(unchanged.version, disabled.version);
    assert_eq!(unchanged.image, disabled.image);

    let filter = ApplicationFilter::by_state(ApplicationState::Disabled);
    let disabled_apps = engine.find_applications(Some(&filter)).await.unwrap();
    assert_eq!(disabled_apps.len(), 1);
    assert_eq!(disabled_apps[0].name, "test-app");

    engine
        .delete_application("test-app".to_string())
        .await
        .unwrap();

    // Verify it's gone
    let result = engine.get_application("test-app".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_application_deletion_waits_for_open_sessions() {
    let (engine, _temp_dir) = create_test_engine().await;
    engine
        .register_application("test-app".to_string(), ApplicationAttributes::default())
        .await
        .unwrap();
    engine
        .create_session(SessionAttributes {
            id: "test-session".to_string(),
            application: "test-app".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    engine
        .update_application_state("test-app".to_string(), ApplicationState::Disabled)
        .await
        .unwrap();

    let open_sessions = SessionFilter::by_application_state("test-app", SessionState::Open);
    assert_eq!(
        engine
            ._list_sessions_metadata(Some(&open_sessions))
            .unwrap()
            .len(),
        1
    );
    let result = engine.delete_application("test-app".to_string()).await;
    assert!(matches!(result, Err(FlameError::InvalidState(_))));

    engine
        .close_session("test-session".to_string())
        .await
        .unwrap();
    let result = engine.delete_application("test-app".to_string()).await;
    assert!(matches!(result, Err(FlameError::InvalidState(_))));
    assert!(engine.get_session("test-session".to_string()).await.is_ok());

    engine
        .delete_session("test-session".to_string())
        .await
        .unwrap();
    engine
        .delete_application("test-app".to_string())
        .await
        .unwrap();
    assert!(matches!(
        engine.get_session("test-session".to_string()).await,
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn test_application_deletion_fails_closed_on_unreadable_session() {
    let (engine, _temp_dir) = create_test_engine().await;
    engine
        .register_application("test-app".to_string(), ApplicationAttributes::default())
        .await
        .unwrap();
    engine
        .update_application_state("test-app".to_string(), ApplicationState::Disabled)
        .await
        .unwrap();
    fs::create_dir_all(engine.session_path("incomplete-session")).unwrap();

    let sessions = SessionFilter {
        application: Some("test-app".to_string()),
        ..SessionFilter::default()
    };
    assert!(engine._list_sessions_metadata(Some(&sessions)).is_err());
    assert!(engine
        .delete_application("test-app".to_string())
        .await
        .is_err());
    assert!(engine.get_application("test-app".to_string()).await.is_ok());
}

#[tokio::test]
async fn test_session_lifecycle() {
    let (engine, _temp_dir) = create_test_engine().await;

    // First register an application
    let app_attr = ApplicationAttributes {
        shim: Shim::Host,
        image: None,
        description: None,
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };
    engine
        .register_application("test-app".to_string(), app_attr)
        .await
        .unwrap();

    // Create session
    let ssn_attr = SessionAttributes {
        batch_size: 1,
        tokens: [("service".into(), "credential".into())].into(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: Some(Bytes::from("test data")),
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    };

    let session = engine.create_session(ssn_attr).await.unwrap();
    assert_eq!(session.id, "test-session");
    assert_eq!(session.status.state, SessionState::Open);

    // Get session
    let session2 = engine
        .get_session("test-session".to_string())
        .await
        .unwrap();
    assert_eq!(session2.id, "test-session");
    assert_eq!(
        session2.tokens.get("service").map(String::as_str),
        Some("credential")
    );

    // Find sessions
    let sessions = engine.find_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);

    // Close session (should work since no tasks)
    let closed = engine
        .close_session("test-session".to_string())
        .await
        .unwrap();
    assert_eq!(closed.status.state, SessionState::Closed);

    // Delete session
    let deleted = engine
        .delete_session("test-session".to_string())
        .await
        .unwrap();
    assert_eq!(deleted.id, "test-session");
}

#[tokio::test]
async fn test_task_lifecycle() {
    let (engine, _temp_dir) = create_test_engine().await;

    // Setup: register app and create session
    let app_attr = ApplicationAttributes {
        shim: Shim::Host,
        image: None,
        description: None,
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };
    engine
        .register_application("test-app".to_string(), app_attr)
        .await
        .unwrap();

    let ssn_attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(ssn_attr).await.unwrap();

    // Create task with input
    let input = Bytes::from("test input data");
    let task = engine
        .create_task("test-session".to_string(), Some(input.clone()), None)
        .await
        .unwrap();
    assert_eq!(task.id, 1);
    assert_eq!(task.state, TaskState::Pending);
    assert_eq!(task.input, Some(input));

    // Get task
    let gid = TaskGID {
        ssn_id: "test-session".to_string(),
        task_id: 1,
    };
    let task2 = engine.get_task(gid.clone()).await.unwrap();
    assert_eq!(task2.id, 1);

    // Update task state
    let task3 = engine
        .update_task_state(task_snapshot(gid.clone()), TaskState::Running, None)
        .await
        .unwrap();
    assert_eq!(task3.state, TaskState::Running);

    // Update task result
    let output = Bytes::from("test output data");
    let result = TaskResult {
        state: TaskState::Succeed,
        output: Some(output.clone()),
        message: None,
    };
    let task4 = engine
        .update_task_result(task_snapshot(gid.clone()), result)
        .await
        .unwrap();
    assert_eq!(task4.state, TaskState::Succeed);
    assert_eq!(task4.output, Some(output));

    // Find tasks
    let tasks = engine
        .find_tasks(&TaskFilter::by_session("test-session"))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);

    // Create another task
    let task5 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task5.id, 2);

    // Complete second task
    let gid2 = TaskGID {
        ssn_id: "test-session".to_string(),
        task_id: 2,
    };
    engine
        .update_task_state(task_snapshot(gid2), TaskState::Succeed, None)
        .await
        .unwrap();

    // Now we can close the session
    let closed = engine
        .close_session("test-session".to_string())
        .await
        .unwrap();
    assert_eq!(closed.status.state, SessionState::Closed);
}

#[tokio::test]
async fn test_register_application_already_exists() {
    let (engine, _temp_dir) = create_test_engine().await;

    let attr = ApplicationAttributes {
        shim: Shim::Host,
        image: Some("test-image".to_string()),
        description: Some("Test application".to_string()),
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };

    engine
        .register_application("test-app".to_string(), attr.clone())
        .await
        .unwrap();

    let result = engine
        .register_application("test-app".to_string(), attr)
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, FlameError::AlreadyExist(_)));
}

#[tokio::test]
async fn test_create_session_already_exists() {
    let (engine, _temp_dir) = create_test_engine().await;

    let app_attr = ApplicationAttributes {
        shim: Shim::Host,
        image: None,
        description: None,
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };
    engine
        .register_application("test-app".to_string(), app_attr)
        .await
        .unwrap();

    let ssn_attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    };

    engine.create_session(ssn_attr.clone()).await.unwrap();

    let result = engine.create_session(ssn_attr).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, FlameError::AlreadyExist(_)));
}

#[tokio::test]
async fn test_close_session_with_pending_tasks() {
    let (engine, _temp_dir) = create_test_engine().await;

    let app_attr = ApplicationAttributes {
        shim: Shim::Host,
        image: None,
        description: None,
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };
    engine
        .register_application("test-app".to_string(), app_attr)
        .await
        .unwrap();

    let ssn_attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(ssn_attr).await.unwrap();

    let task1 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task1.state, TaskState::Pending);

    let task2 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task2.state, TaskState::Pending);

    let closed = engine
        .close_session("test-session".to_string())
        .await
        .unwrap();
    assert_eq!(closed.status.state, SessionState::Closed);

    let task1_after = engine.get_task(task1.gid()).await.unwrap();
    assert_eq!(task1_after.state, TaskState::Cancelled);

    let task2_after = engine.get_task(task2.gid()).await.unwrap();
    assert_eq!(task2_after.state, TaskState::Cancelled);

    engine
        .delete_session("test-session".to_string())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_close_session_with_running_tasks() {
    let (engine, _temp_dir) = create_test_engine().await;

    let app_attr = ApplicationAttributes {
        shim: Shim::Host,
        image: None,
        description: None,
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };
    engine
        .register_application("test-app".to_string(), app_attr)
        .await
        .unwrap();

    let ssn_attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(ssn_attr).await.unwrap();

    let task = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();

    engine
        .update_task_state(task.clone(), TaskState::Running, None)
        .await
        .unwrap();

    let result = engine.close_session("test-session".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_node_crud() {
    let (engine, _temp_dir) = create_test_engine().await;

    let node = Node {
        name: "test-node".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 6,
            memory: 12288,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
    };

    let created = engine.create_node(&node).await.unwrap();
    assert_eq!(created.name, "test-node");

    let found = engine.get_node("test-node").await.unwrap();
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.name, "test-node");
    assert_eq!(found.capacity.cpu, 8);

    let mut updated_node = node.clone();
    updated_node.state = NodeState::NotReady;
    let updated = engine.update_node(&updated_node).await.unwrap();
    assert_eq!(updated.state, NodeState::NotReady);

    let nodes = engine.find_nodes().await.unwrap();
    assert_eq!(nodes.len(), 1);

    engine.delete_node("test-node").await.unwrap();
    let deleted = engine.get_node("test-node").await.unwrap();
    assert!(deleted.is_none());
}

#[tokio::test]
async fn test_executor_crud() {
    let (engine, _temp_dir) = create_test_engine().await;

    let node = Node {
        name: "exec-test-node".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 4,
            memory: 8192,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 4,
            memory: 8192,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
    };
    engine.create_node(&node).await.unwrap();

    let executor = crate::apis::Executor {
        id: "exec-1".to_string(),
        node: "exec-test-node".to_string(),
        resreq: ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        },
        shim: Shim::Host,
        application: "test-app".to_string(),
        task_id: None,
        ssn_id: None,
        signal: None,
        attributes: Default::default(),
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state: ExecutorState::Void,
    };

    let created = engine.create_executor(&executor).await.unwrap();
    assert_eq!(created.id, "exec-1");
    assert_eq!(created.application, "test-app");

    let found = engine.get_executor(&"exec-1".to_string()).await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().application, "test-app");

    let updated = engine
        .update_executor_state(&"exec-1".to_string(), ExecutorState::Idle)
        .await
        .unwrap();
    assert_eq!(updated.state, ExecutorState::Idle);
    assert_eq!(updated.application, "test-app");

    let executors = engine.find_executors(None).await.unwrap();
    assert_eq!(executors.len(), 1);
    assert_eq!(executors[0].application, "test-app");

    let by_node = engine.find_executors(Some("exec-test-node")).await.unwrap();
    assert_eq!(by_node.len(), 1);

    let by_other = engine.find_executors(Some("other-node")).await.unwrap();
    assert_eq!(by_other.len(), 0);

    engine.delete_executor(&"exec-1".to_string()).await.unwrap();
    let deleted = engine.get_executor(&"exec-1".to_string()).await.unwrap();
    assert!(deleted.is_none());
}

#[tokio::test]
async fn test_node_delete_cascades_to_executors() {
    let (engine, _temp_dir) = create_test_engine().await;

    let node = Node {
        name: "cascade-node".to_string(),
        state: NodeState::Ready,
        capacity: ResourceRequirement {
            cpu: 4,
            memory: 8192,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 4,
            memory: 8192,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
    };
    engine.create_node(&node).await.unwrap();

    for i in 1..=3 {
        let executor = crate::apis::Executor {
            id: format!("cascade-exec-{i}"),
            node: "cascade-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            },
            shim: Shim::Host,
            application: "test-app".to_string(),
            task_id: None,
            ssn_id: None,
            signal: None,
            attributes: Default::default(),
            creation_time: Utc::now(),
            latest_updated_timestamp: Utc::now(),
            state: ExecutorState::Void,
        };
        engine.create_executor(&executor).await.unwrap();
    }

    let executors = engine.find_executors(Some("cascade-node")).await.unwrap();
    assert_eq!(executors.len(), 3);

    engine.delete_node("cascade-node").await.unwrap();

    let executors = engine.find_executors(None).await.unwrap();
    assert_eq!(executors.len(), 0);
}

async fn engine_with_session(id: &str) -> (Arc<FilesystemEngine>, TempDir) {
    let (engine, dir) = create_test_engine().await;
    let app_attr = ApplicationAttributes {
        shim: Shim::Host,
        image: None,
        description: None,
        labels: vec![],
        command: Some("/bin/test".to_string()),
        arguments: vec![],
        environments: std::collections::HashMap::new(),
        working_directory: None,
        max_instances: 10,
        delay_release: Duration::seconds(0),
        schema: None,
        url: None,
        installer: None,
    };
    engine
        .register_application("test-app".to_string(), app_attr)
        .await
        .unwrap();
    engine
        .create_session(SessionAttributes {
            id: id.to_string(),
            application: "test-app".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    (Arc::new(engine), dir)
}

fn gid(ssn: &str, task_id: TaskID) -> TaskGID {
    TaskGID {
        ssn_id: ssn.to_string(),
        task_id,
    }
}

fn test_node_named(name: &str) -> Node {
    let resreq = ResourceRequirement {
        cpu: 4,
        memory: 8192,
        gpu: 0,
    };
    Node {
        name: name.to_string(),
        state: NodeState::Ready,
        capacity: resreq.clone(),
        allocatable: resreq,
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: Vec::new(),
    }
}

fn test_executor_on(id: &str, node: &str) -> Executor {
    Executor {
        id: id.to_string(),
        node: node.to_string(),
        resreq: ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        },
        shim: Shim::Host,
        application: "test-app".to_string(),
        task_id: None,
        ssn_id: None,
        signal: None,
        attributes: Default::default(),
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state: ExecutorState::Void,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_executor_moves_and_node_deletes_do_not_deadlock() {
    let (engine, _dir) = create_test_engine().await;
    let engine = Arc::new(engine);
    let shared_engine = engine.clone();
    for node in ["node-a", "node-b"] {
        engine.create_node(&test_node_named(node)).await.unwrap();
    }
    engine
        .create_executor(&test_executor_on("mover", "node-a"))
        .await
        .unwrap();

    let workload = async {
        let mut tasks = vec![];
        for round in 0..20 {
            let target = if round % 2 == 0 { "node-b" } else { "node-a" };
            let engine = Arc::clone(&shared_engine);
            tasks.push(tokio::spawn(async move {
                let _ = engine
                    .update_executor(&test_executor_on("mover", target))
                    .await;
            }));
            let engine = Arc::clone(&shared_engine);
            tasks.push(tokio::spawn(async move {
                let _ = engine
                    .update_executor_state(&"mover".to_string(), ExecutorState::Idle)
                    .await;
                let _ = engine.get_executor(&"mover".to_string()).await;
                let _ = engine.find_executors(None).await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), workload)
        .await
        .expect("executor moves deadlocked");

    // Exactly one copy of the executor remains, on the node it last moved to.
    let all = engine.find_executors(None).await.unwrap();
    assert_eq!(all.len(), 1);
    let stored = engine
        .get_executor(&"mover".to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.node, all[0].node);

    let deleting = engine.clone();
    let node = stored.node.clone();
    within_a_second(async move { deleting.delete_node(&node).await })
        .await
        .expect("node delete deadlocked")
        .unwrap();
    assert!(engine
        .get_executor(&"mover".to_string())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_application_create_admits_one() {
    let (engine, _dir) = create_test_engine().await;
    let engine = Arc::new(engine);
    let attempts = (0..8).map(|_| {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .register_application("race-app".to_string(), ApplicationAttributes::default())
                .await
        })
    });
    let mut results = vec![];
    for attempt in attempts.collect::<Vec<_>>() {
        results.push(attempt.await);
    }
    let admitted = results
        .iter()
        .filter(|r| r.as_ref().unwrap().is_ok())
        .count();
    assert_eq!(admitted, 1, "exactly one concurrent create must win");
    assert!(engine.get_application("race-app".to_string()).await.is_ok());
}

#[tokio::test]
async fn test_create_session_rejects_empty_application() {
    let (engine, dir) = create_test_engine().await;
    let error = engine
        .create_session(SessionAttributes {
            id: "empty-app".to_string(),
            application: String::new(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, FlameError::InvalidConfig(_)));
    assert!(!dir.path().join("sessions/empty-app").exists());
}

#[tokio::test]
async fn test_missing_entities_are_not_cached() {
    let (engine, _dir) = create_test_engine().await;
    assert!(engine.get_node("ghost").await.unwrap().is_none());
    assert!(engine
        .get_executor(&"ghost".to_string())
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        engine.get_application("ghost".to_string()).await,
        Err(FlameError::NotFound(_))
    ));
    assert!(engine.nodes.read().unwrap().is_empty());
    assert!(engine.executors.read().unwrap().is_empty());
    assert!(engine.applications.read().unwrap().is_empty());
}

#[tokio::test]
async fn test_executor_requires_existing_node_and_failed_create_leaves_no_entry() {
    let (engine, _dir) = create_test_engine().await;
    let error = engine
        .create_executor(&test_executor_on("orphan", "no-such-node"))
        .await
        .unwrap_err();
    assert!(matches!(error, FlameError::NotFound(_)));
    assert!(engine.executors.read().unwrap().is_empty());
    assert!(engine
        .get_executor(&"orphan".to_string())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_deleted_application_can_be_registered_again() {
    let (engine, _dir) = create_test_engine().await;
    let name = "cycle-app".to_string();
    engine
        .register_application(name.clone(), ApplicationAttributes::default())
        .await
        .unwrap();
    engine
        .update_application_state(name.clone(), ApplicationState::Disabled)
        .await
        .unwrap();
    engine.delete_application(name.clone()).await.unwrap();
    assert!(engine.applications.read().unwrap().is_empty());
    assert!(matches!(
        engine.get_application(name.clone()).await,
        Err(FlameError::NotFound(_))
    ));

    let again = engine
        .register_application(name.clone(), ApplicationAttributes::default())
        .await
        .unwrap();
    assert_eq!(again.version, 1);
    assert!(matches!(
        engine
            .register_application(name, ApplicationAttributes::default())
            .await,
        Err(FlameError::AlreadyExist(_))
    ));
}

#[tokio::test]
async fn test_restart_loads_entities_once() {
    let dir = TempDir::new().unwrap();
    let url = format!("filesystem://{}", dir.path().display());
    {
        let engine = FilesystemEngine::new_ptr(&url).await.unwrap();
        engine
            .register_application("restart-app".to_string(), ApplicationAttributes::default())
            .await
            .unwrap();
        engine
            .create_node(&test_node_named("restart-node"))
            .await
            .unwrap();
        engine
            .create_executor(&test_executor_on("restart-exec", "restart-node"))
            .await
            .unwrap();
        engine
            .create_session(SessionAttributes {
                id: "restart-ssn".to_string(),
                application: "restart-app".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    let engine = FilesystemEngine::new_ptr(&url).await.unwrap();
    assert!(engine
        .get_application("restart-app".to_string())
        .await
        .is_ok());
    assert!(engine.get_node("restart-node").await.unwrap().is_some());
    let executor = engine
        .get_executor(&"restart-exec".to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(executor.node, "restart-node");
    assert_eq!(
        engine
            .find_executors(Some("restart-node"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(engine.find_sessions().await.unwrap().len(), 1);
    assert!(engine.get_session("restart-ssn".to_string()).await.is_ok());
    assert!(matches!(
        engine.get_session("missing".to_string()).await,
        Err(FlameError::NotFound(_))
    ));
    assert!(matches!(
        engine
            .register_application("restart-app".to_string(), ApplicationAttributes::default())
            .await,
        Err(FlameError::AlreadyExist(_))
    ));
}

fn within_a_second<F: std::future::IntoFuture>(future: F) -> tokio::time::Timeout<F::IntoFuture> {
    tokio::time::timeout(std::time::Duration::from_secs(1), future)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_creates_get_dense_ids_and_distinct_payloads() {
    let (engine, dir) = engine_with_session("dense").await;

    let mut handles = vec![];
    for i in 0..32u8 {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            engine
                .create_task("dense".to_string(), Some(Bytes::from(vec![i; 16])), None)
                .await
                .unwrap()
        }));
    }
    let mut tasks = vec![];
    for handle in handles {
        tasks.push(handle.await.unwrap());
    }

    let mut ids: Vec<_> = tasks.iter().map(|task| task.id).collect();
    ids.sort();
    assert_eq!(ids, (1..=32).collect::<Vec<_>>());

    let url = format!("filesystem://{}", dir.path().display());
    let reopened = FilesystemEngine::new_ptr(&url).await.unwrap();
    reopened.find_sessions().await.unwrap();
    for task in &tasks {
        let stored = reopened.get_task(gid("dense", task.id)).await.unwrap();
        assert_eq!(stored.input, task.input, "task {} payload", task.id);
        assert_eq!(stored.state, TaskState::Pending);
    }
}

#[tokio::test]
async fn test_gates_do_not_block_unrelated_operations() {
    let (engine, _dir) = engine_with_session("gates").await;
    for _ in 0..2 {
        engine
            .create_task("gates".to_string(), None, None)
            .await
            .unwrap();
    }
    let shared = engine.session_entry("gates").unwrap();
    let entry = read_async!(shared);

    // Creation commit gate held: updates to existing tasks proceed.
    {
        let _commit = entry.commit.lock().await;
        within_a_second(engine.update_task_state(
            task_snapshot(gid("gates", 1)),
            TaskState::Running,
            None,
        ))
        .await
        .expect("update waited for the creation commit gate")
        .unwrap();
    }

    // Another task's lock is held: creation must still commit, and an unrelated
    // task must still update and be read.
    {
        let _held = entry.task_lock(2).write_arc();
        within_a_second(engine.update_task_state(
            task_snapshot(gid("gates", 1)),
            TaskState::Succeed,
            None,
        ))
        .await
        .expect("unrelated task waited for a held task lock")
        .unwrap();
        within_a_second(engine.get_task(gid("gates", 1)))
            .await
            .expect("unrelated read waited for a held task lock")
            .unwrap();
        let created = within_a_second(engine.create_task("gates".to_string(), None, None))
            .await
            .expect("creation waited for another task's lock")
            .unwrap();
        assert_eq!(created.id, 3);
    }
}

#[tokio::test]
async fn test_persistent_checksum_mismatch_is_corruption() {
    let (engine, dir) = engine_with_session("corrupt").await;
    engine
        .create_task("corrupt".to_string(), None, None)
        .await
        .unwrap();

    let tasks = dir.path().join("sessions/corrupt/tasks.bin");
    let file = fs::OpenOptions::new().write(true).open(&tasks).unwrap();
    // id (8 bytes) + version (4 bytes) precede the checksum.
    file.write_all_at(&[0xFF; 4], 12).unwrap();

    let err = engine.get_task(gid("corrupt", 1)).await.unwrap_err();
    assert!(err.to_string().contains("checksum mismatch"), "{err}");
}

#[tokio::test]
async fn test_incomplete_tail_is_discarded_before_next_id() {
    let (engine, dir) = engine_with_session("tail").await;
    engine
        .create_task("tail".to_string(), None, None)
        .await
        .unwrap();

    let tasks = dir.path().join("sessions/tail/tasks.bin");
    let file = fs::OpenOptions::new().write(true).open(&tasks).unwrap();
    let record_size = engine.record_size as u64;
    file.write_all_at(&[0xAB; 5], record_size).unwrap();

    let second = engine
        .create_task("tail".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(second.id, 2);
    assert_eq!(fs::metadata(&tasks).unwrap().len(), 2 * record_size);
    engine.get_task(gid("tail", 2)).await.unwrap();
}

#[tokio::test]
async fn test_failed_commit_reports_error_and_retry_succeeds() {
    let (engine, dir) = engine_with_session("retry-commit").await;
    let shared = engine.session_entry("retry-commit").unwrap();
    {
        let entry = read_async!(shared);
        let path = dir.path().join("sessions/retry-commit/tasks.bin");
        // A read-only descriptor forces a task-record write failure.
        *entry.files[Part::Tasks as usize].lock().unwrap() = Some(Arc::new(
            super::super::payload::PayloadFile::open(&path, false).unwrap(),
        ));
    }
    let error = engine
        .create_task("retry-commit".into(), None, None)
        .await
        .unwrap_err();
    assert!(matches!(error, FlameError::Storage(_)));
    assert!(error.to_string().contains("Failed to commit task record"));
    // Restore write access as a transient storage failure would recover.
    *read_async!(shared).files[Part::Tasks as usize]
        .lock()
        .unwrap() = None;
    let task = engine
        .create_task("retry-commit".into(), None, None)
        .await
        .unwrap();
    assert_eq!(task.id, 1);
    assert_eq!(
        engine
            .get_task(gid("retry-commit", task.id))
            .await
            .unwrap()
            .state,
        TaskState::Pending
    );
}

#[tokio::test]
async fn test_closed_session_retains_no_descriptors_and_stays_readable() {
    let (engine, _dir) = engine_with_session("closing").await;
    let task = engine
        .create_task("closing".to_string(), Some(Bytes::from("data")), None)
        .await
        .unwrap();
    let shared = engine.session_entry("closing").unwrap();
    assert!(read_async!(shared).files[Part::Tasks as usize]
        .lock()
        .unwrap()
        .is_some());

    engine.close_session("closing".to_string()).await.unwrap();
    let retained = |entry: &SessionEntry| {
        entry
            .files
            .iter()
            .filter(|slot| slot.lock().unwrap().is_some())
            .count()
    };
    assert_eq!(retained(&*read_async!(shared)), 0);

    let stored = engine.get_task(gid("closing", task.id)).await.unwrap();
    assert_eq!(stored.input, Some(Bytes::from("data")));
    assert_eq!(retained(&*read_async!(shared)), 0);
}

#[tokio::test]
async fn test_empty_affinity_writes_no_payload() {
    let (engine, dir) = engine_with_session("affinity").await;
    let task = engine
        .create_task("affinity".to_string(), None, None)
        .await
        .unwrap();
    assert!(task.affinity.is_empty());
    assert!(!dir.path().join("sessions/affinity/affinity.bin").exists());

    let stored = engine.get_task(gid("affinity", task.id)).await.unwrap();
    assert!(stored.affinity.is_empty());
}

#[tokio::test]
async fn test_operations_after_delete_observe_the_tombstone() {
    let (engine, _dir) = engine_with_session("gone").await;
    engine.close_session("gone".to_string()).await.unwrap();
    engine.delete_session("gone".to_string()).await.unwrap();

    let err = engine
        .create_task("gone".to_string(), None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, FlameError::NotFound(_)), "{err}");
}
#[tokio::test]
async fn test_closed_session_rejects_every_task_mutation_before_payload_write() {
    let (engine, dir) = engine_with_session("closed-writes").await;
    let task = engine
        .create_task("closed-writes".into(), Some(Bytes::from("input")), None)
        .await
        .unwrap();
    engine.close_session("closed-writes".into()).await.unwrap();
    let tasks_path = dir.path().join("sessions/closed-writes/tasks.bin");
    let records = fs::read(&tasks_path).unwrap();
    let output_path = dir.path().join("sessions/closed-writes/outputs.bin");
    let outputs = fs::read(&output_path).unwrap();
    let results = [
        engine
            .create_task("closed-writes".into(), Some(Bytes::from("late")), None)
            .await
            .map(|_| ()),
        engine
            .retry_task(gid("closed-writes", task.id))
            .await
            .map(|_| ()),
        engine
            .update_task_state(
                task_snapshot(gid("closed-writes", task.id)),
                TaskState::Running,
                None,
            )
            .await
            .map(|_| ()),
        engine
            .update_task_result(
                task_snapshot(gid("closed-writes", task.id)),
                TaskResult {
                    state: TaskState::Succeed,
                    output: Some(Bytes::from("late output")),
                    message: None,
                },
            )
            .await
            .map(|_| ()),
    ];
    for result in results {
        assert!(matches!(result, Err(FlameError::InvalidState(_))));
    }
    assert_eq!(fs::read(&tasks_path).unwrap(), records);
    assert_eq!(fs::read(&output_path).unwrap(), outputs);
    let stored = engine
        .get_task(gid("closed-writes", task.id))
        .await
        .unwrap();
    assert_eq!(stored.state, TaskState::Cancelled);
    assert_eq!(stored.input, Some(Bytes::from("input")));
}

#[tokio::test]
async fn test_session_entry_json_and_runtime_recovery() {
    let (_engine, dir) = engine_with_session("entry-json").await;
    let path = dir.path().join("sessions/entry-json/metadata");
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 15);
    for field in ["dir", "commit", "tasks", "files"] {
        assert!(
            json.get(field).is_none(),
            "runtime field persisted: {field}"
        );
    }
    let reopened = FilesystemEngine::new_ptr(&format!("filesystem://{}", dir.path().display()))
        .await
        .unwrap();
    reopened.find_sessions().await.unwrap();
    let task = reopened
        .create_task("entry-json".into(), Some(Bytes::from("recovered")), None)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_task(gid("entry-json", task.id))
            .await
            .unwrap()
            .input,
        task.input
    );
    reopened.close_session("entry-json".into()).await.unwrap();
    let closed = FilesystemEngine::new_ptr(&format!("filesystem://{}", dir.path().display()))
        .await
        .unwrap();
    closed.find_sessions().await.unwrap();
    assert!(matches!(
        closed.retry_task(gid("entry-json", task.id)).await,
        Err(FlameError::InvalidState(_))
    ));
    assert_eq!(
        closed
            .get_task(gid("entry-json", task.id))
            .await
            .unwrap()
            .state,
        TaskState::Cancelled
    );
}

#[tokio::test]
async fn test_waiting_reader_rejects_deleted_entry_after_session_id_reuse() {
    let (engine, _dir) = engine_with_session("reuse").await;
    let task = engine
        .create_task("reuse".into(), None, None)
        .await
        .unwrap();
    engine.close_session("reuse".into()).await.unwrap();
    let old = engine.session_entry("reuse").unwrap();
    let mut deleting = Box::pin(engine.delete_session("reuse".into()));
    let mut reading = Box::pin(engine.get_task(gid("reuse", task.id)));
    {
        let _held = write_async!(old);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                deleting.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                reading.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
    }
    deleting.await.unwrap();
    engine
        .create_session(SessionAttributes {
            id: "reuse".into(),
            application: "test-app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    engine
        .create_task("reuse".into(), None, None)
        .await
        .unwrap();
    assert!(matches!(reading.await, Err(FlameError::NotFound(_))));
    assert_eq!(
        engine.get_task(gid("reuse", 1)).await.unwrap().state,
        TaskState::Pending
    );
}

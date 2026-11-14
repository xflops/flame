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

fn test_engine() -> NoneEngine {
    NoneEngine {
        applications: stdng::new_ptr(HashMap::new()),
        sessions: stdng::new_ptr(HashMap::new()),
    }
}

#[test]
fn locked_session_does_not_block_other_session_history_or_mutations() {
    let engine = test_engine();
    for id in ["held", "free"] {
        tokio_test::block_on(engine.create_session(SessionAttributes {
            id: id.into(),
            application: "app".into(),
            ..Default::default()
        }))
        .unwrap();
    }
    let shared = engine.session_entry("held").unwrap();
    let result = std::thread::scope(|scope| {
        let result = {
            let _held = lock_ptr!(shared).unwrap();
            let (sent, received) = std::sync::mpsc::channel();
            let engine = &engine;
            scope.spawn(move || {
                let result = tokio_test::block_on(async {
                    let task = engine.create_task("free".into(), None, None).await?;
                    let running = engine
                        .update_task_state(task, TaskState::Running, None)
                        .await?;
                    let finished = engine
                        .update_task_result(
                            running,
                            TaskResult {
                                state: TaskState::Succeed,
                                output: Some(bytes::Bytes::from_static(b"result")),
                                message: None,
                            },
                        )
                        .await?;
                    let tasks = engine.find_tasks(&TaskFilter::terminal("free")).await?;
                    assert_eq!(tasks.len(), 1);
                    assert_eq!(tasks[0].output, finished.output);
                    let closed = engine.close_session("free".into()).await?;
                    assert_eq!(closed.task_counts.succeed, 1);
                    Ok::<_, FlameError>(())
                });
                sent.send(result).unwrap();
            });
            received.recv_timeout(std::time::Duration::from_secs(5))
        };
        // Releasing the held entry before joining prevents a failing lock
        // regression from leaving a blocked test thread behind.
        result
    });
    result
        .expect("another session must make progress while an entry is locked")
        .unwrap();
}

#[test]
fn stale_session_handles_are_rejected_after_delete_and_recreate() {
    let engine = test_engine();
    let attr = SessionAttributes {
        id: "same".into(),
        application: "app".into(),
        ..Default::default()
    };
    tokio_test::block_on(engine.create_session(attr.clone())).unwrap();
    let stale = engine.session_entry("same").unwrap();
    let old_task = tokio_test::block_on(engine.create_task(
        "same".into(),
        Some(bytes::Bytes::from_static(b"old")),
        None,
    ))
    .unwrap();
    tokio_test::block_on(engine.delete_session("same".into())).unwrap();
    {
        let _old_entry = lock_ptr!(stale).unwrap();
        assert!(matches!(
            engine.validate_session("same", &stale),
            Err(FlameError::NotFound(_))
        ));
    }
    tokio_test::block_on(engine.create_session(attr)).unwrap();
    let replacement = engine.session_entry("same").unwrap();
    assert!(!Arc::ptr_eq(&stale, &replacement));
    {
        let _old_entry = lock_ptr!(stale).unwrap();
        assert!(matches!(
            engine.validate_session("same", &stale),
            Err(FlameError::NotFound(_))
        ));
    }
    assert!(matches!(
        tokio_test::block_on(engine.get_task(old_task.gid())),
        Err(FlameError::NotFound(_))
    ));
    let task = tokio_test::block_on(engine.create_task(
        "same".into(),
        Some(bytes::Bytes::from_static(b"new")),
        None,
    ))
    .unwrap();
    assert_eq!(task.id, 1);
    assert_eq!(task.input, Some(bytes::Bytes::from_static(b"new")));
    assert_eq!(
        tokio_test::block_on(engine.get_session("same".into()))
            .unwrap()
            .task_counts
            .pending,
        1
    );
}

#[tokio::test]
async fn test_none_engine_create_session() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();

    let attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 1,
        max_instances: None,
        priority: 0,
        resreq: None,
    };

    let session = engine.create_session(attr).await.unwrap();
    assert_eq!(session.id, "test-session");
    assert_eq!(session.application, "test-app");
    assert_eq!(session.status.state, SessionState::Open);
}

#[tokio::test]
async fn test_none_engine_get_session_returns_not_found() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();

    let result = engine.get_session("test-session".to_string()).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), FlameError::NotFound(_)));
}

#[tokio::test]
async fn test_none_engine_find_sessions_returns_empty() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();

    let sessions = engine.find_sessions().await.unwrap();
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn session_metadata_and_retry_keep_history_until_delete_or_restart() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();
    let attr = SessionAttributes {
        id: "history".into(),
        application: "app".into(),
        ..Default::default()
    };
    engine.create_session(attr.clone()).await.unwrap();
    let created = engine
        .create_task("history".into(), None, None)
        .await
        .unwrap();
    let running = engine
        .update_task_state(created.clone(), TaskState::Running, None)
        .await
        .unwrap();
    assert!(matches!(
        engine.create_session(attr.clone()).await,
        Err(FlameError::AlreadyExist(_))
    ));
    let opened = engine
        .open_session("history".into(), Some(attr))
        .await
        .unwrap();
    assert_eq!(opened.task_counts.running, 1);
    assert!(opened.tasks.is_empty());
    assert!(opened.tasks_index.is_empty());
    let retried = engine.retry_task(running.gid()).await.unwrap();
    assert_eq!(retried.state, TaskState::Pending);
    assert_eq!(retried.version, running.version + 1);
    assert_eq!(
        engine.get_task(created.gid()).await.unwrap().version,
        retried.version
    );
    let metadata = engine.find_sessions().await.unwrap();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].task_counts.pending, 1);
    assert_eq!(metadata[0].task_counts.running, 0);
    assert!(metadata[0].tasks.is_empty());
    assert!(matches!(
        engine
            .retry_task(TaskGID {
                ssn_id: "history".into(),
                task_id: 99
            })
            .await,
        Err(FlameError::NotFound(_))
    ));
    let unknown = Task {
        id: 99,
        ..created.clone()
    };
    assert!(matches!(
        engine
            .update_task_state(unknown.clone(), TaskState::Failed, None)
            .await,
        Err(FlameError::NotFound(_))
    ));
    assert!(matches!(
        engine
            .update_task_result(unknown, TaskResult::default())
            .await,
        Err(FlameError::NotFound(_))
    ));
    assert_eq!(
        engine
            .get_session("history".into())
            .await
            .unwrap()
            .task_counts
            .pending,
        1
    );
    let closed = engine.close_session("history".into()).await.unwrap();
    assert_eq!(closed.task_counts.cancelled, 1);
    assert_eq!(
        engine.get_task(created.gid()).await.unwrap().state,
        TaskState::Cancelled
    );
    assert!(matches!(
        engine.retry_task(created.gid()).await,
        Err(FlameError::InvalidState(_))
    ));
    let fresh = NoneEngine::new_ptr("none").await.unwrap();
    assert!(fresh.find_sessions().await.unwrap().is_empty());
    assert!(matches!(
        fresh.get_task(created.gid()).await,
        Err(FlameError::NotFound(_))
    ));
}

#[tokio::test]
async fn test_none_engine_task_id_allocation() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();

    let attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 1,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(attr).await.unwrap();

    let task1 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task1.id, 1);

    let task2 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task2.id, 2);

    let task3 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task3.id, 3);
}

#[tokio::test]
async fn test_none_engine_task_id_per_session() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();

    let attr1 = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "session-1".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 1,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(attr1).await.unwrap();

    let attr2 = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "session-2".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 1,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(attr2).await.unwrap();

    let task1_s1 = engine
        .create_task("session-1".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task1_s1.id, 1);

    let task1_s2 = engine
        .create_task("session-2".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task1_s2.id, 1);

    let task2_s1 = engine
        .create_task("session-1".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task2_s1.id, 2);
}

#[tokio::test]
async fn test_none_engine_delete_session_cleans_counter() {
    let engine = NoneEngine::new_ptr("none").await.unwrap();

    let attr = SessionAttributes {
        batch_size: 1,
        tokens: Default::default(),
        id: "test-session".to_string(),
        application: "test-app".to_string(),
        common_data: None,
        min_instances: 1,
        max_instances: None,
        priority: 0,
        resreq: None,
    };
    engine.create_session(attr.clone()).await.unwrap();

    let task1 = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task1.id, 1);

    let result = engine.delete_session("test-session".to_string()).await;
    assert_eq!(result.unwrap().id, "test-session");
    assert!(matches!(
        engine.get_task(task1.gid()).await,
        Err(FlameError::NotFound(_))
    ));
    assert!(matches!(
        engine
            .find_tasks(&TaskFilter::by_session("test-session"))
            .await,
        Err(FlameError::NotFound(_))
    ));

    engine.create_session(attr).await.unwrap();

    let task_new = engine
        .create_task("test-session".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(task_new.id, 1);
}

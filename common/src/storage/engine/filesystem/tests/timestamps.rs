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
use rpc::flame::v1 as rpc;

const CREATED: i64 = 1_790_000_000_123;
const COMPLETED: i64 = 1_790_000_000_987;

async fn engine() -> (FilesystemEngine, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let engine = FilesystemEngine {
        base_path: directory.path().to_path_buf(),
        record_size: task_record_size(),
        sessions: RwLock::new(HashMap::new()),
        applications: RwLock::new(HashMap::new()),
        nodes: RwLock::new(HashMap::new()),
        executors: RwLock::new(HashMap::new()),
    };
    engine
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    engine
        .create_session(SessionAttributes {
            id: "times".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    (engine, directory)
}

async fn seed(
    engine: &FilesystemEngine,
    state: TaskState,
    created: i64,
    completed: i64,
) -> TaskGID {
    let task = engine
        .create_task("times".into(), None, None)
        .await
        .unwrap();
    let shared = engine.session_entry("times").unwrap();
    let entry = read_async!(shared);
    entry.with_task(task.id, || {
        let mut header = engine.read_task_record(&entry, task.id).unwrap();
        header.state = state as u8;
        header.creation_time = created;
        header.completion_time = completed;
        header.checksum = calculate_checksum(&header);
        engine.write_task_metadata(&entry, &header).unwrap();
    });
    task.gid()
}

fn assert_wire(task: &Task, created: i64, completed: Option<i64>) {
    let wire = rpc::Task::from(task).status.unwrap();
    assert_eq!(wire.creation_time, created);
    assert_eq!(wire.completion_time, completed);
}

#[tokio::test]
async fn completed_task_retains_exact_milliseconds_through_get_list_rpc_and_reopen() {
    let (engine, directory) = engine().await;
    let gid = seed(&engine, TaskState::Succeed, CREATED, COMPLETED).await;
    assert_wire(
        &engine.get_task(gid.clone()).await.unwrap(),
        CREATED,
        Some(COMPLETED),
    );
    let tasks = engine
        .find_task_metadata(&TaskFilter::terminal("times"))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_wire(&tasks[0], CREATED, Some(COMPLETED));
    let reopened =
        FilesystemEngine::new_ptr(&format!("filesystem://{}", directory.path().display()))
            .await
            .unwrap();
    assert_wire(
        &reopened.get_task(gid).await.unwrap(),
        CREATED,
        Some(COMPLETED),
    );
    assert_eq!(task_record_size(), 81);
    assert_eq!(
        fs::metadata(engine.session_path("times").join("tasks.bin"))
            .unwrap()
            .len(),
        81
    );
}

#[tokio::test]
async fn retry_clears_completion_and_preserves_millisecond_creation() {
    let (engine, directory) = engine().await;
    let gid = seed(&engine, TaskState::Succeed, CREATED, COMPLETED).await;
    let completed = engine.get_task(gid.clone()).await.unwrap();
    assert_wire(&completed, CREATED, Some(COMPLETED));
    let task = engine.retry_task(gid.clone()).await.unwrap();
    assert_eq!(task.state, TaskState::Pending);
    assert_wire(&task, CREATED, None);
    let task = engine
        .update_task_state(task, TaskState::Running, None)
        .await
        .unwrap();
    assert_wire(&task, CREATED, None);
    let task = engine
        .update_task_result(
            task,
            TaskResult {
                state: TaskState::Succeed,
                output: None,
                message: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.creation_time.timestamp_millis(), CREATED);
    assert!(task.completion_time.is_some());
    let completed = task.completion_time.unwrap().timestamp_millis();
    {
        let shared = engine.session_entry("times").unwrap();
        let entry = read_async!(shared);
        let header = engine.read_task_metadata(&entry, gid.task_id).unwrap();
        assert_eq!(header.creation_time, CREATED);
        assert_eq!(header.completion_time, completed);
    }
    let reopened =
        FilesystemEngine::new_ptr(&format!("filesystem://{}", directory.path().display()))
            .await
            .unwrap();
    assert_wire(
        &reopened.get_task(gid).await.unwrap(),
        CREATED,
        Some(completed),
    );
}

#[tokio::test]
async fn new_task_and_completion_writers_store_milliseconds_instead_of_seconds() {
    let (engine, _directory) = engine().await;
    // Bracketing the production writer catches second-based storage without relying on a
    // particular scheduler delay or requiring a sleep across a clock boundary.
    let before = Utc::now().timestamp_millis();
    let pending = engine
        .create_task("times".into(), None, None)
        .await
        .unwrap();
    let after = Utc::now().timestamp_millis();
    assert!((before..=after).contains(&pending.creation_time.timestamp_millis()));
    assert!(pending.completion_time.is_none());
    let running = engine
        .update_task_state(pending, TaskState::Running, None)
        .await
        .unwrap();
    assert!(running.completion_time.is_none());
    let before = Utc::now().timestamp_millis();
    let finished = engine
        .update_task_result(
            running,
            TaskResult {
                state: TaskState::Succeed,
                output: None,
                message: None,
            },
        )
        .await
        .unwrap();
    let after = Utc::now().timestamp_millis();
    assert!((before..=after).contains(&finished.completion_time.unwrap().timestamp_millis()));
    let shared = engine.session_entry("times").unwrap();
    let entry = read_async!(shared);
    let header = engine.read_task_metadata(&entry, finished.id).unwrap();
    assert_eq!(
        header.creation_time,
        finished.creation_time.timestamp_millis()
    );
    assert_eq!(
        header.completion_time,
        finished.completion_time.unwrap().timestamp_millis()
    );
}

#[tokio::test]
async fn task_filters_counts_and_session_closure_preserve_timestamps() {
    let (engine, _directory) = engine().await;
    seed(&engine, TaskState::Succeed, CREATED, COMPLETED).await;
    seed(&engine, TaskState::Succeed, CREATED, COMPLETED).await;
    let pending = seed(&engine, TaskState::Pending, CREATED, 0).await;
    let running = seed(&engine, TaskState::Running, CREATED, 0).await;
    for gid in [pending, running.clone()] {
        assert!(engine
            .get_task(gid)
            .await
            .unwrap()
            .completion_time
            .is_none());
    }
    assert_eq!(
        engine
            .find_task_metadata(&TaskFilter::terminal("times"))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        engine
            .find_task_metadata(&TaskFilter::by_session_states(
                "times",
                vec![TaskState::Running]
            ))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        engine.close_session("times".into()).await.is_err(),
        "running tasks must still prevent session closure"
    );
    let task = engine.get_task(running).await.unwrap();
    engine
        .update_task_state(task, TaskState::Pending, None)
        .await
        .unwrap();
    let closed = engine.close_session("times".into()).await.unwrap();
    assert_eq!(closed.task_counts.succeed, 2);
    assert_eq!(closed.task_counts.cancelled, 2);
    let tasks = engine
        .find_task_metadata(&TaskFilter::terminal("times"))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 4);
    assert!(tasks.iter().all(|task| task.completion_time.is_some()));
    assert_eq!(
        fs::metadata(engine.session_path("times").join("tasks.bin"))
            .unwrap()
            .len(),
        4 * 81
    );
}

#[tokio::test]
async fn state_completion_writer_uses_milliseconds_and_nonterminal_updates_clear_completion() {
    let (engine, _directory) = engine().await;
    let gid = seed(&engine, TaskState::Succeed, CREATED, COMPLETED).await;
    let task = engine.get_task(gid.clone()).await.unwrap();
    let pending = engine
        .update_task_result(
            task,
            TaskResult {
                state: TaskState::Pending,
                output: None,
                message: None,
            },
        )
        .await
        .unwrap();
    assert_wire(&pending, CREATED, None);
    let before = Utc::now().timestamp_millis();
    let failed = engine
        .update_task_state(pending, TaskState::Failed, None)
        .await
        .unwrap();
    let after = Utc::now().timestamp_millis();
    assert!((before..=after).contains(&failed.completion_time.unwrap().timestamp_millis()));
    let running = engine
        .update_task_state(failed, TaskState::Running, None)
        .await
        .unwrap();
    assert_wire(&running, CREATED, None);
    let shared = engine.session_entry("times").unwrap();
    let entry = read_async!(shared);
    let header = engine.read_task_metadata(&entry, gid.task_id).unwrap();
    assert_eq!(header.creation_time, CREATED);
    assert_eq!(header.completion_time, 0);
}

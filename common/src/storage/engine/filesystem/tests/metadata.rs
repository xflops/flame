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

async fn engine() -> (FilesystemEngine, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let storage = FilesystemEngine {
        base_path: directory.path().to_owned(),
        record_size: task_record_size(),
        sessions: RwLock::new(HashMap::new()),
        applications: RwLock::new(HashMap::new()),
        nodes: RwLock::new(HashMap::new()),
        executors: RwLock::new(HashMap::new()),
    };
    storage
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    storage
        .create_session(SessionAttributes {
            id: "session".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    (storage, directory)
}

/// Header ordering and filtering remain independent of unreadable payload files.
#[tokio::test]
async fn task_metadata_orders_by_descending_id_without_reading_payloads() {
    let (storage, _directory) = engine().await;
    for _ in 0..3 {
        storage
            .create_task("session".into(), Some(Bytes::from_static(b"payload")), None)
            .await
            .unwrap();
    }
    let shared = storage.session_entry("session").unwrap();
    {
        let entry = read_async!(shared);
        for (id, timestamp, state) in [
            (1, 10, TaskState::Running),
            (2, 20, TaskState::Succeed),
            (3, 20, TaskState::Pending),
        ] {
            entry.with_task(id, || {
                let mut meta = storage.read_task_record(&entry, id).unwrap();
                meta.creation_time = timestamp;
                meta.state = state as u8;
                meta.checksum = calculate_checksum(&meta);
                storage.write_task_metadata(&entry, &meta).unwrap();
            });
        }
    }
    fs::write(storage.session_path("session").join("inputs.bin"), b"").unwrap();
    let tasks = storage
        .find_task_metadata(&TaskFilter::by_session("session"))
        .await
        .unwrap();
    assert_eq!(
        tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert!(tasks.iter().all(|task| task.input.is_none()));
    assert!(storage.get_task(tasks[0].gid()).await.is_err());
    let tasks = storage
        .find_task_metadata(&TaskFilter::by_session_states(
            "session",
            vec![TaskState::Pending],
        ))
        .await
        .unwrap();
    assert_eq!(tasks.iter().map(|task| task.id).collect::<Vec<_>>(), [3]);
}

/// Pages seek past unreadable history and read only their selected fixed records.
#[tokio::test]
async fn hundred_thousand_headers_have_payload_independent_metadata_reads() {
    let (storage, _directory) = engine().await;
    let mut bytes = Vec::with_capacity(100_000 * storage.record_size);
    for id in 1..=100_000 {
        let mut header = TaskMetadata {
            id,
            creation_time: 10,
            state: TaskState::Succeed as u8,
            input_len: 1024,
            ..Default::default()
        };
        header.checksum = calculate_checksum(&header);
        let record = postcard::to_allocvec(&header).unwrap();
        bytes.extend_from_slice(&record);
        bytes.resize(id as usize * storage.record_size, 0);
    }
    // Every header outside the last two pages is corrupt. Listing either page
    // must succeed without opening payloads or decoding any of those headers.
    bytes[..99_990 * storage.record_size].fill(0);
    fs::write(storage.session_path("session").join("tasks.bin"), bytes).unwrap();
    for number in [1, 2] {
        let filter = TaskFilter::by_session("session").with_paging(
            crate::apis::Paging::new()
                .with_limit(5)
                .unwrap()
                .with_number(number)
                .unwrap(),
        );
        let tasks = storage.find_task_metadata(&filter).await.unwrap();
        assert_eq!(tasks.len(), 5);
        assert_eq!(tasks.paging.total(), Some(100_000));
        assert_eq!(tasks[0].id, 100_000 - (number - 1) * 5);
        assert_eq!(tasks[4].id, 99_996 - (number - 1) * 5);
        assert!(tasks
            .iter()
            .all(|task| task.input.is_none() && task.output.is_none() && task.affinity.is_empty()));
    }
    let beyond = TaskFilter::by_session("session").with_paging(
        crate::apis::Paging::new()
            .with_limit(5)
            .unwrap()
            .with_number(20_001)
            .unwrap(),
    );
    let empty = storage.find_task_metadata(&beyond).await.unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.paging.total(), Some(100_000));
    // A selected corrupt header still fails: skipped records are the only reads avoided.
    let corrupt = TaskFilter::by_session("session").with_paging(
        crate::apis::Paging::new()
            .with_limit(5)
            .unwrap()
            .with_number(3)
            .unwrap(),
    );
    assert!(storage.find_task_metadata(&corrupt).await.is_err());
}

#[tokio::test]
async fn node_creation_time_survives_update_and_restart() {
    let (storage, directory) = engine().await;
    let mut node = Node {
        name: "node".into(),
        ..Default::default()
    };
    storage.create_node(&node).await.unwrap();
    let path = directory.path().join("nodes/node/metadata");
    let mut metadata: NodeMetadata = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    metadata.creation_time = 1234;
    fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let recovered = FilesystemEngine::new_ptr(&format!("fs://{}", directory.path().display()))
        .await
        .unwrap();
    assert_eq!(
        recovered
            .get_node("node")
            .await
            .unwrap()
            .unwrap()
            .creation_time
            .timestamp(),
        1234
    );
    node.capacity.cpu = 12;
    recovered.update_node(&node).await.unwrap();
    assert_eq!(
        recovered
            .get_node("node")
            .await
            .unwrap()
            .unwrap()
            .creation_time
            .timestamp(),
        1234
    );
}

/// Closing and startup discard cached credentials; explicit reads remain full.
#[tokio::test]
async fn closed_registry_drops_tokens_and_details_read_them_on_demand() {
    let (storage, directory) = engine().await;
    storage
        .create_session(SessionAttributes {
            id: "credentials".into(),
            application: "app".into(),
            tokens: [("service".into(), "secret".into())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage.close_session("credentials".into()).await.unwrap();
    {
        let shared = storage.session_entry("credentials").unwrap();
        let entry = read_async!(shared);
        assert!(entry.metadata.tokens.is_empty());
    }
    assert_eq!(
        storage
            .get_session("credentials".into())
            .await
            .unwrap()
            .tokens["service"],
        "secret"
    );
    let reloaded = FilesystemEngine {
        base_path: directory.path().to_owned(),
        record_size: task_record_size(),
        sessions: RwLock::new(HashMap::new()),
        applications: RwLock::new(HashMap::new()),
        nodes: RwLock::new(HashMap::new()),
        executors: RwLock::new(HashMap::new()),
    };
    reloaded.load_entities().unwrap();
    {
        let shared = reloaded.session_entry("credentials").unwrap();
        let entry = read_async!(shared);
        assert!(entry.metadata.tokens.is_empty());
    }
    assert_eq!(
        reloaded
            .get_session("credentials".into())
            .await
            .unwrap()
            .tokens["service"],
        "secret"
    );
}

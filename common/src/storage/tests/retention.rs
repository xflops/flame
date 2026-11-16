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

//! Retained sessions and atomic admission are independent of list paging.
use super::*;
use crate::apis::{Paging, TaskCounts};
use bytes::Bytes;

async fn storage(url: &str, max: Option<usize>) -> Storage {
    let context = FlameClusterContext {
        cluster: crate::ctx::FlameCluster {
            storage: "none".into(),
            limits: crate::ctx::FlameLimits {
                max_sessions: max,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let mut storage = (*new_ptr(&context).await.unwrap()).clone();
    storage.context.cluster.storage = url.into();
    storage.engine = engine::connect(url).await.unwrap();
    storage
}
fn attributes(id: &str) -> SessionAttributes {
    SessionAttributes {
        id: id.into(),
        application: "app".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn close_retains_the_same_session_pointer_without_task_indexes() {
    let storage = storage("none", Some(1)).await;
    storage
        .create_session(SessionAttributes {
            common_data: Some(Bytes::from_static(b"common")),
            tokens: HashMap::from([("credential".into(), "secret".into())]),
            ..attributes("s")
        })
        .await
        .unwrap();
    storage
        .create_task("s".into(), Some(Bytes::from_static(b"input")), None)
        .await
        .unwrap();
    let weak = {
        let live = storage.get_session_ptr("s".into()).unwrap();
        storage.close_session("s".into()).await.unwrap();
        {
            let session = lock_ptr!(live).unwrap();
            assert!(session.is_closed());
            assert!(session.tasks.is_empty());
            assert_eq!(session.common_data, Some(Bytes::from_static(b"common")));
            assert!(!session.tokens.is_empty());
        }
        Arc::downgrade(&live)
    };
    assert!(weak.upgrade().is_some());
    assert!(Arc::ptr_eq(
        &weak.upgrade().unwrap(),
        &storage.get_session_ptr("s".into()).unwrap()
    ));
    {
        let directory = lock_ptr!(storage.sessions).unwrap();
        assert_eq!(directory.len(), 1);
    }
    let sessions = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].task_counts.cancelled, 1);
    assert!(sessions[0].common_data.is_none());
    let detailed = storage.get_session("s".into()).unwrap();
    assert_eq!(detailed.common_data, Some(Bytes::from_static(b"common")));
    assert!(lock_ptr!(storage.get_session_ptr("s".into()).unwrap())
        .unwrap()
        .tasks_index
        .is_empty());
    storage.delete_session("s".into()).await.unwrap();
    assert!(storage.list_sessions(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn open_admission_is_atomic_for_creation_resume_and_close() {
    let storage = Arc::new(storage("none", Some(1)).await);
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut jobs = Vec::new();
    for id in ["first", "second"] {
        let storage = storage.clone();
        let barrier = barrier.clone();
        jobs.push(tokio::spawn(async move {
            barrier.wait().await;
            storage.create_session(attributes(id)).await
        }));
    }
    barrier.wait().await;
    let mut admitted = Vec::new();
    for job in jobs {
        if let Ok(session) = job.await.unwrap() {
            admitted.push(session.id);
        }
    }
    assert_eq!(admitted.len(), 1);
    storage.close_session(admitted.remove(0)).await.unwrap();
    for id in ["resume-a", "resume-b"] {
        storage.create_session(attributes(id)).await.unwrap();
        storage
            .update_session_state(id.into(), SessionState::Suspended)
            .await
            .unwrap();
    }
    let (first, second) = tokio::join!(
        storage.update_session_state("resume-a".into(), SessionState::Open),
        storage.update_session_state("resume-b".into(), SessionState::Open)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let (open, suspended) = if first.is_ok() {
        ("resume-a", "resume-b")
    } else {
        ("resume-b", "resume-a")
    };
    storage.close_session(open.into()).await.unwrap();
    storage
        .update_session_state(suspended.into(), SessionState::Open)
        .await
        .unwrap();
    let sessions = storage.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(
        sessions.iter().filter(|session| session.is_open()).count(),
        1
    );
    assert_eq!(
        sessions
            .iter()
            .filter(|session| session.is_closed())
            .count(),
        2
    );
}

#[tokio::test]
async fn recovery_keeps_all_sessions_and_skips_closed_task_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let original = storage(&url, None).await;
    original.create_session(attributes("closed")).await.unwrap();
    original
        .create_task(
            "closed".into(),
            Some(Bytes::from_static(b"broken later")),
            None,
        )
        .await
        .unwrap();
    original.close_session("closed".into()).await.unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("sessions/closed/inputs.bin"))
        .unwrap()
        .set_len(0)
        .unwrap();
    for id in ["open-a", "open-b"] {
        original.create_session(attributes(id)).await.unwrap();
    }
    let recovered = storage(&url, Some(1)).await;
    recovered.load_data().await.unwrap();
    let sessions = recovered.list_sessions(None).await.unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(
        sessions.iter().filter(|session| session.is_open()).count(),
        2
    );
    let closed = recovered.get_session("closed".into()).unwrap();
    assert_eq!(
        closed.task_counts,
        TaskCounts {
            cancelled: 1,
            ..Default::default()
        }
    );
    assert!(closed.tasks.is_empty());
    assert!(closed.tasks_index.is_empty());
    assert!(recovered.get_session_ptr("closed".into()).is_ok());
    assert!(recovered
        .create_session(attributes("blocked"))
        .await
        .is_err());
    recovered.close_session("open-a".into()).await.unwrap();
    assert!(recovered
        .create_session(attributes("still-blocked"))
        .await
        .is_err());
    recovered.close_session("open-b".into()).await.unwrap();
    recovered
        .create_session(attributes("admitted"))
        .await
        .unwrap();
    assert_eq!(recovered.list_sessions(None).await.unwrap().len(), 4);
    let tasks = recovered
        .list_tasks(&TaskFilter::by_session("closed"))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].input.is_none());
}

#[tokio::test]
async fn lists_project_metadata_and_internal_queries_are_complete() {
    let storage = storage("none", None).await;
    for i in 0..105 {
        storage
            .create_session(attributes(&format!("s-{i:03}")))
            .await
            .unwrap();
    }
    let all = storage.list_sessions(None).await.unwrap();
    assert_eq!(all.len(), 105);
    let page = storage
        .list_sessions(Some(
            &SessionFilter::default().with_paging(
                Paging::new()
                    .with_limit(10)
                    .unwrap()
                    .with_number(2)
                    .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(page.len(), 10);
    assert_eq!(page.paging.total(), Some(105));
    assert!(page.iter().all(|session| session.common_data.is_none()
        && session.tokens.is_empty()
        && session.tasks.is_empty()
        && session.events.is_empty()));
    storage
        .create_task(
            "s-000".into(),
            Some(Bytes::from_static(b"payload")),
            Some(TaskOptions {
                affinity: [Bytes::from_static(b"key")].into(),
            }),
        )
        .await
        .unwrap();
    let tasks = storage
        .list_tasks(&TaskFilter::by_session("s-000"))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(
        tasks[0].input.is_none()
            && tasks[0].output.is_none()
            && tasks[0].affinity.is_empty()
            && tasks[0].events.is_empty()
    );
}

#[tokio::test]
async fn resource_pages_preserve_state_priority_and_creation_order() {
    let storage = storage("none", None).await;
    let old = chrono::DateTime::from_timestamp(10, 0).unwrap();
    let new = chrono::DateTime::from_timestamp(20, 0).unwrap();
    for name in ["old-enabled", "new-disabled"] {
        storage
            .register_application(
                name.into(),
                ApplicationAttributes {
                    environments: HashMap::from([("SECRET".into(), "hidden".into())]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    storage
        .update_application_state("new-disabled".into(), ApplicationState::Disabled)
        .await
        .unwrap();
    {
        let applications = lock_ptr!(storage.applications).unwrap();
        lock_ptr!(applications["old-enabled"])
            .unwrap()
            .creation_time = old;
        lock_ptr!(applications["new-disabled"])
            .unwrap()
            .creation_time = new;
    }
    let applications = storage.list_applications(None).await.unwrap();
    assert_eq!(
        applications
            .iter()
            .map(|app| app.name.as_str())
            .collect::<Vec<_>>(),
        vec!["old-enabled", "new-disabled"]
    );
    assert!(applications.iter().all(|app| app.environments.is_empty()));
    for id in ["old-open", "new-suspended"] {
        storage.create_session(attributes(id)).await.unwrap();
    }
    {
        let old_session = storage.get_session_ptr("old-open".into()).unwrap();
        lock_ptr!(old_session).unwrap().creation_time = old;
        let new_session = storage.get_session_ptr("new-suspended".into()).unwrap();
        lock_ptr!(new_session).unwrap().creation_time = new;
    }
    storage
        .update_session_state("new-suspended".into(), SessionState::Suspended)
        .await
        .unwrap();
    let sessions = storage.list_sessions(None).await.unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["old-open", "new-suspended"]
    );
    for (name, state) in [
        ("old-ready", NodeState::Ready),
        ("new-notready", NodeState::NotReady),
    ] {
        storage
            .register_node(&Node {
                name: name.into(),
                state,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    {
        lock_ptr!(storage.get_node_ptr("old-ready").unwrap())
            .unwrap()
            .creation_time = old;
        lock_ptr!(storage.get_node_ptr("new-notready").unwrap())
            .unwrap()
            .creation_time = new;
    }
    let nodes = storage.list_nodes(None).unwrap();
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        vec!["old-ready", "new-notready"]
    );
    storage
        .update_node_state("old-ready", NodeState::Unknown)
        .await
        .unwrap();
    let nodes = storage.list_nodes(None).unwrap();
    assert_eq!(nodes[0].name, "new-notready");
    assert_eq!(
        storage
            .get_node("old-ready")
            .unwrap()
            .unwrap()
            .creation_time,
        old
    );
    {
        let mut executors = lock_ptr!(storage.executors).unwrap();
        for (id, node, state) in [
            ("z", "a", ExecutorState::Idle),
            ("a", "b", ExecutorState::Unknown),
            ("b", "a", ExecutorState::Bound),
            ("y", "a", ExecutorState::Idle),
        ] {
            executors.insert(
                id.into(),
                stdng::new_ptr(Executor {
                    id: id.into(),
                    node: node.into(),
                    state,
                    ..Default::default()
                }),
            );
        }
    }
    let executors = storage.list_executors(None).await.unwrap();
    assert_eq!(
        executors
            .iter()
            .map(|executor| executor.id.as_str())
            .collect::<Vec<_>>(),
        vec!["y", "z", "b", "a"]
    );
}

#[tokio::test]
async fn node_first_registration_time_survives_reregistration() {
    let dir = tempfile::tempdir().unwrap();
    for url in ["none".to_string(), format!("fs://{}", dir.path().display())] {
        let storage = storage(&url, None).await;
        let mut node = Node {
            name: "registration-time".into(),
            ..Default::default()
        };
        let before = Utc::now().timestamp();
        storage.register_node(&node).await.unwrap();
        let created = storage.get_node(&node.name).unwrap().unwrap().creation_time;
        assert!(created.timestamp() >= before);
        node.creation_time = created - chrono::Duration::days(1);
        node.capacity.cpu = 16;
        storage.register_node(&node).await.unwrap();
        let updated = storage.get_node(&node.name).unwrap().unwrap();
        assert_eq!(updated.creation_time, created);
        assert_eq!(updated.capacity.cpu, 16);
    }
}

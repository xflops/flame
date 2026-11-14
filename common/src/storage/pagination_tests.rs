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

//! The same offset paging contract applies to memory, SQLite, and filesystem records.
use super::*;
use crate::apis::*;
use bytes::Bytes;

async fn test_storage(url: &str) -> Storage {
    let context = FlameClusterContext {
        cluster: crate::ctx::FlameCluster {
            storage: "none".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut storage = (*new_ptr(&context).await.unwrap()).clone();
    storage.engine = engine::connect(url).await.unwrap();
    storage.context.cluster.storage = url.into();
    storage
}
fn urls(path: &std::path::Path) -> [String; 3] {
    [
        "none".into(),
        format!("sqlite://{}", path.join("page.db").display()),
        format!("fs://{}", path.join("records").display()),
    ]
}
async fn seed(storage: &Storage) {
    storage
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    for id in ["alpha", "bravo", "charlie"] {
        storage
            .create_session(SessionAttributes {
                id: id.into(),
                application: "app".into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    for _ in 0..12 {
        storage
            .create_task("alpha".into(), Some(Bytes::from_static(b"payload")), None)
            .await
            .unwrap();
    }
}
fn task_filter() -> TaskPageFilter {
    TaskPageFilter {
        session_id: "alpha".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn paging_tasks_supports_direct_jumps_and_optional_payloads() {
    let dir = tempfile::tempdir().unwrap();
    for url in urls(dir.path()) {
        let storage = test_storage(&url).await;
        seed(&storage).await;
        let request = PageRequest {
            page_size: 5,
            order_by: "id asc".into(),
            ..Default::default()
        };
        let first = storage.page_tasks(&request, &task_filter()).await.unwrap();
        assert_eq!(
            first.items.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(first
            .items
            .iter()
            .all(|t| t.input.is_none() && t.affinity.is_empty() && t.events.is_empty()));
        assert_eq!(
            (
                first.paging.page_number,
                first.paging.page_size,
                first.paging.total_pages
            ),
            (1, 5, 3)
        );
        assert_eq!(
            (first.paging.total_count, first.paging.resource_count),
            (12, 12)
        );
        assert_eq!(first.paging.state_counts["Pending"], 12);
        let third = storage
            .page_tasks(
                &PageRequest {
                    page_number: 3,
                    ..request.clone()
                },
                &task_filter(),
            )
            .await
            .unwrap();
        assert_eq!(
            third.items.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert_eq!(third.paging.total_pages, 3);
        let beyond = storage
            .page_tasks(
                &PageRequest {
                    page_number: 4,
                    ..request.clone()
                },
                &task_filter(),
            )
            .await
            .unwrap();
        assert!(beyond.items.is_empty());
        assert_eq!(beyond.paging.page_number, 4);
        assert_eq!(beyond.paging.total_count, 12);
        let included = storage
            .page_tasks(
                &request,
                &TaskPageFilter {
                    include_payloads: true,
                    ..task_filter()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            included.items[0].input,
            Some(Bytes::from_static(b"payload"))
        );
        assert!(!included.items[0].events.is_empty());
        let filtered = storage
            .page_tasks(
                &request,
                &TaskPageFilter {
                    ids: vec![1, 10, 11, 12],
                    ..task_filter()
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered.paging.total_count, 4);
        assert_eq!(filtered.paging.total_pages, 1);
        let empty = storage
            .page_tasks(
                &request,
                &TaskPageFilter {
                    state: Some(TaskState::Running),
                    ..task_filter()
                },
            )
            .await
            .unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(empty.paging.total_pages, 0);
        assert_eq!(empty.paging.resource_count, 12);
        assert!(storage
            .page_tasks(
                &request,
                &TaskPageFilter {
                    session_id: "missing".into(),
                    ..Default::default()
                }
            )
            .await
            .is_err());
        assert!(storage
            .page_tasks(
                &request,
                &TaskPageFilter {
                    session_id: "bravo".into(),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items
            .is_empty());
    }
}

#[tokio::test]
async fn paging_sessions_reflects_offset_mutations_and_includes_history() {
    let dir = tempfile::tempdir().unwrap();
    for url in urls(dir.path()) {
        let storage = test_storage(&url).await;
        seed(&storage).await;
        let request = PageRequest {
            page_size: 1,
            page_number: 2,
            order_by: "id asc".into(),
        };
        let second = storage
            .page_sessions(&request, &SessionPageFilter::default())
            .await
            .unwrap();
        assert_eq!(second.items[0].id, "bravo");
        let first = storage
            .page_sessions(
                &PageRequest {
                    page_number: 1,
                    ..request.clone()
                },
                &SessionPageFilter::default(),
            )
            .await
            .unwrap();
        assert!(first.items[0].tasks.is_empty());
        assert_eq!(first.items[0].task_counts.pending, 12);
        storage.engine.close_session("alpha".into()).await.unwrap();
        storage.engine.delete_session("alpha".into()).await.unwrap();
        // Offsets reflect the current collection: deletion shifts the second row.
        let shifted = storage
            .page_sessions(&request, &SessionPageFilter::default())
            .await
            .unwrap();
        assert_eq!(shifted.items[0].id, "charlie");
        assert_eq!(shifted.paging.total_pages, 2);
        storage.engine.close_session("bravo".into()).await.unwrap();
        {
            lock_ptr!(storage.sessions).unwrap().remove("bravo");
        }
        let closed = storage
            .page_sessions(
                &PageRequest::default(),
                &SessionPageFilter {
                    state: Some(SessionState::Closed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(closed.items[0].id, "bravo");
        assert_eq!(
            (closed.paging.total_count, closed.paging.resource_count),
            (1, 2)
        );
        assert_eq!(closed.paging.state_counts["Closed"], 1);
    }
}

#[tokio::test]
async fn paging_rejects_invalid_bounds_and_offset_overflow() {
    let dir = tempfile::tempdir().unwrap();
    for url in urls(dir.path()) {
        let storage = test_storage(&url).await;
        seed(&storage).await;
        for request in [
            PageRequest {
                page_size: -1,
                ..Default::default()
            },
            PageRequest {
                page_size: 1001,
                ..Default::default()
            },
            PageRequest {
                page_number: -1,
                ..Default::default()
            },
            PageRequest {
                page_number: i64::MAX,
                page_size: 2,
                ..Default::default()
            },
        ] {
            assert!(storage
                .page_sessions(&request, &SessionPageFilter::default())
                .await
                .is_err());
        }
        for order in ["state asc", "id sideways", "creation_time asc extra"] {
            assert!(storage
                .page_sessions(
                    &PageRequest {
                        order_by: order.into(),
                        ..Default::default()
                    },
                    &SessionPageFilter::default()
                )
                .await
                .is_err());
        }
        let distant = storage
            .page_tasks(
                &PageRequest {
                    page_number: i64::MAX,
                    page_size: 1,
                    ..Default::default()
                },
                &task_filter(),
            )
            .await
            .unwrap();
        assert!(distant.items.is_empty());
        assert_eq!(distant.paging.page_number, i64::MAX);
        let maximum = storage
            .page_tasks(
                &PageRequest {
                    page_size: 1000,
                    ..Default::default()
                },
                &task_filter(),
            )
            .await
            .unwrap();
        assert_eq!(maximum.items.len(), 12);
        assert_eq!(maximum.paging.page_size, 1000);
    }
}

#[tokio::test]
async fn filesystem_paging_does_not_read_skipped_or_unselected_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let storage = test_storage(&format!("fs://{}", dir.path().display())).await;
    seed(&storage).await;
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("sessions/alpha/inputs.bin"))
        .unwrap()
        .set_len(7)
        .unwrap();
    let filter = TaskPageFilter {
        include_payloads: true,
        ..task_filter()
    };
    let request = PageRequest {
        page_size: 1,
        order_by: "id asc".into(),
        ..Default::default()
    };
    let first = storage.page_tasks(&request, &filter).await.unwrap();
    assert_eq!(first.items[0].input, Some(Bytes::from_static(b"payload")));
    assert_eq!(first.paging.total_count, 12);
    assert!(storage
        .page_tasks(
            &PageRequest {
                page_number: 2,
                ..request.clone()
            },
            &filter
        )
        .await
        .is_err());
    let metadata = storage
        .page_tasks(
            &PageRequest {
                page_size: 100,
                ..request
            },
            &TaskPageFilter {
                include_payloads: false,
                ..filter
            },
        )
        .await
        .unwrap();
    assert_eq!(metadata.items.len(), 12);
    let sessions = storage
        .page_sessions(&PageRequest::default(), &SessionPageFilter::default())
        .await
        .unwrap();
    assert_eq!(
        sessions
            .items
            .iter()
            .find(|s| s.id == "alpha")
            .unwrap()
            .task_counts
            .pending,
        12
    );
    // A malformed earlier affinity payload cannot affect a direct jump to a later task.
    let other = tempfile::tempdir().unwrap();
    let clean = test_storage(&format!("fs://{}", other.path().display())).await;
    clean
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    clean
        .create_session(SessionAttributes {
            id: "alpha".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    clean
        .create_task(
            "alpha".into(),
            None,
            Some(TaskOptions {
                affinity: [Bytes::from_static(b"key")].into(),
            }),
        )
        .await
        .unwrap();
    clean
        .create_task("alpha".into(), Some(Bytes::from_static(b"later")), None)
        .await
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(other.path().join("sessions/alpha/affinity.bin"))
        .unwrap()
        .set_len(0)
        .unwrap();
    let later = clean
        .page_tasks(
            &PageRequest {
                page_number: 2,
                page_size: 1,
                order_by: "id".into(),
            },
            &TaskPageFilter {
                include_payloads: true,
                ..task_filter()
            },
        )
        .await
        .unwrap();
    assert_eq!(later.items[0].input, Some(Bytes::from_static(b"later")));
}

#[tokio::test]
async fn paging_normalizes_metadata_filters_and_preserves_capacity_totals() {
    let storage = test_storage("none").await;
    seed(&storage).await;
    let page = storage
        .page_sessions(
            &PageRequest {
                page_size: 1,
                page_number: 2,
                order_by: "name asc".into(),
            },
            &SessionPageFilter {
                ids: vec!["charlie".into(), "alpha".into(), "alpha".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items[0].id, "charlie");
    assert_eq!(page.paging.total_count, 2);
    {
        let mut nodes = lock_ptr!(storage.nodes).unwrap();
        for (name, state, cpu) in [
            ("first", NodeState::Ready, 2),
            ("second", NodeState::NotReady, 4),
        ] {
            nodes.insert(
                name.into(),
                stdng::new_ptr(Node {
                    name: name.into(),
                    state,
                    capacity: ResourceRequirement {
                        cpu,
                        memory: 100,
                        gpu: 0,
                    },
                    ..Default::default()
                }),
            );
        }
    }
    let nodes = storage
        .page_nodes(
            &PageRequest::default(),
            &NodePageFilter {
                state: Some(NodeState::Ready),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(nodes.items.len(), 1);
    assert_eq!(nodes.paging.resource_count, 2);
    assert_eq!(nodes.paging.state_counts["NotReady"], 1);
    assert_eq!(nodes.paging.totals["cpu"], 6);
}

#[tokio::test]
async fn paging_default_is_bounded_to_one_hundred() {
    let storage = test_storage("none").await;
    seed(&storage).await;
    for _ in 12..105 {
        storage
            .create_task("alpha".into(), None, None)
            .await
            .unwrap();
    }
    let first = storage
        .page_tasks(&PageRequest::default(), &task_filter())
        .await
        .unwrap();
    assert_eq!(first.items.len(), 100);
    assert_eq!(
        (
            first.paging.page_number,
            first.paging.page_size,
            first.paging.total_pages
        ),
        (1, 100, 2)
    );
    let maximum = storage
        .page_tasks(
            &PageRequest {
                page_size: 1000,
                ..Default::default()
            },
            &task_filter(),
        )
        .await
        .unwrap();
    assert_eq!(maximum.items.len(), 105);
    assert_eq!(maximum.paging.total_pages, 1);
}

#[tokio::test]
async fn paging_state_counts_use_protobuf_enum_names() {
    let storage = test_storage("none").await;
    {
        lock_ptr!(storage.executors).unwrap().insert(
            "idle".into(),
            stdng::new_ptr(Executor {
                id: "idle".into(),
                state: ExecutorState::Idle,
                ..Default::default()
            }),
        );
    }
    let page = storage
        .page_executors(&PageRequest::default(), &ExecutorPageFilter::default())
        .await
        .unwrap();
    assert_eq!(page.paging.state_counts["ExecutorIdle"], 1);
    assert_eq!(page.paging.state_counts["ExecutorUnknown"], 0);
    assert_eq!(page.paging.state_counts.len(), 8);
    let nodes = storage
        .page_nodes(&PageRequest::default(), &NodePageFilter::default())
        .await
        .unwrap();
    assert_eq!(
        nodes
            .paging
            .state_counts
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["NotReady", "Ready", "Unknown"]
    );
}

#[tokio::test]
async fn paging_memory_resources_traverse_both_directions_with_stable_summaries() {
    let storage = test_storage("none").await;
    for name in ["charlie", "alpha", "bravo"] {
        storage
            .register_application(name.into(), ApplicationAttributes::default())
            .await
            .unwrap();
    }
    {
        let entries = lock_ptr!(storage.applications).unwrap();
        for ptr in entries.values() {
            lock_ptr!(ptr).unwrap().creation_time =
                chrono::DateTime::from_timestamp(100, 0).unwrap();
        }
    }
    {
        let mut nodes = lock_ptr!(storage.nodes).unwrap();
        let mut executors = lock_ptr!(storage.executors).unwrap();
        for name in ["charlie", "alpha", "bravo"] {
            nodes.insert(
                name.into(),
                stdng::new_ptr(Node {
                    name: name.into(),
                    state: NodeState::Ready,
                    ..Default::default()
                }),
            );
            executors.insert(
                name.into(),
                stdng::new_ptr(Executor {
                    id: name.into(),
                    state: ExecutorState::Idle,
                    ..Default::default()
                }),
            );
        }
    }
    for resource in ["applications", "nodes", "executors"] {
        for direction in ["asc", "desc"] {
            let mut request = PageRequest {
                page_size: 1,
                page_number: 1,
                order_by: format!(
                    "{} {direction}",
                    if resource == "applications" {
                        "creation_time"
                    } else {
                        "id"
                    }
                ),
            };
            let mut ids = Vec::new();
            let mut previous: Option<PageInfo> = None;
            loop {
                let (items, info) = match resource {
                    "applications" => {
                        let page = storage
                            .page_applications(&request, &ApplicationPageFilter::default())
                            .await
                            .unwrap();
                        (
                            page.items.into_iter().map(|i| i.name).collect::<Vec<_>>(),
                            page.paging,
                        )
                    }
                    "nodes" => {
                        let page = storage
                            .page_nodes(&request, &NodePageFilter::default())
                            .await
                            .unwrap();
                        (
                            page.items.into_iter().map(|i| i.name).collect::<Vec<_>>(),
                            page.paging,
                        )
                    }
                    _ => {
                        let page = storage
                            .page_executors(&request, &ExecutorPageFilter::default())
                            .await
                            .unwrap();
                        (
                            page.items.into_iter().map(|i| i.id).collect::<Vec<_>>(),
                            page.paging,
                        )
                    }
                };
                assert_eq!(items.len(), 1);
                assert_eq!(
                    (info.total_count, info.resource_count, info.total_pages),
                    (3, 3, 3)
                );
                assert_eq!(info.page_number, request.page_number);
                if let Some(prior) = previous.as_ref() {
                    assert_eq!(info.state_counts, prior.state_counts);
                    assert_eq!(info.totals, prior.totals);
                }
                ids.extend(items);
                if info.page_number as u64 == info.total_pages {
                    break;
                }
                request.page_number += 1;
                previous = Some(info);
                assert!(request.page_number <= 3);
            }
            let mut expected = vec![
                "alpha".to_string(),
                "bravo".to_string(),
                "charlie".to_string(),
            ];
            if direction == "desc" {
                expected.reverse();
            }
            assert_eq!(ids, expected, "{resource} {direction}");
        }
        let request = PageRequest {
            page_number: 2,
            page_size: 1,
            ..Default::default()
        };
        let (len, info) = match resource {
            "applications" => {
                let page = storage
                    .page_applications(
                        &request,
                        &ApplicationPageFilter {
                            names: vec!["missing".into()],
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                (page.items.len(), page.paging)
            }
            "nodes" => {
                let page = storage
                    .page_nodes(
                        &request,
                        &NodePageFilter {
                            names: vec!["missing".into()],
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                (page.items.len(), page.paging)
            }
            _ => {
                let page = storage
                    .page_executors(
                        &request,
                        &ExecutorPageFilter {
                            ids: vec!["missing".into()],
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                (page.items.len(), page.paging)
            }
        };
        assert_eq!(len, 0);
        assert_eq!(
            (info.total_count, info.resource_count, info.total_pages),
            (0, 3, 0)
        );
        assert_eq!(info.page_number, 2);
    }
}

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

use super::*;
use common::apis::{
    Node, NodeInfo, NodeState, ResourceRequirement, Shim, SESSION_BIND_FAILED,
    SESSION_RETRY_LIMIT_REACHED,
};
use common::ctx::{
    FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits, FlameRecovery,
    FlameSessionRecovery,
};
use tokio::sync::mpsc;

/// Creates test storage in a unique filesystem directory.
async fn create_test_storage() -> StoragePtr {
    create_test_storage_with_retry_limits(common::ctx::DEFAULT_SESSION_RETRY_LIMITS).await
}

async fn create_test_storage_with_retry_limits(retry_limits: u32) -> StoragePtr {
    let unique_id = format!(
        "{}_{:?}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        std::thread::current().id()
    );
    let test_dir = std::env::temp_dir().join(format!("flame_test_{}", unique_id));
    std::fs::create_dir_all(&test_dir).unwrap();
    let url = format!("fs://{}", test_dir.display());

    let ctx = FlameClusterContext {
        cluster: FlameCluster {
            name: "test".to_string(),
            endpoint: "http://localhost:8080".to_string(),
            advertise: None,
            storage: url,
            resreq: None,
            policies: vec!["priority".to_string(), "drf".to_string()],
            schedule_interval: 1000,
            executors: FlameExecutors::default(),
            tls: None,
            limits: FlameLimits {
                max_sessions: None,
                node: common::ctx::FlameNodeLimits { max_executors: 10 },
                ..Default::default()
            },
            recovery: FlameRecovery {
                session: FlameSessionRecovery { retry_limits },
            },
            pprof: None,
        },
        cache: None,
    };

    crate::storage::new_ptr(&ctx).await.unwrap()
}

/// Creates a test node.
fn create_test_node(name: &str) -> Node {
    Node {
        name: name.to_string(),
        state: NodeState::Unknown,
        capacity: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        allocatable: ResourceRequirement {
            cpu: 8,
            memory: 16384,
            gpu: 0,
        },
        info: NodeInfo {
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
            ..Default::default()
        },
        shims: vec![common::apis::Shim::Host],
    }
}

fn create_test_application() -> ApplicationAttributes {
    ApplicationAttributes {
        shim: Shim::default(),
        ..Default::default()
    }
}

fn create_test_session_attr(id: &str) -> SessionAttributes {
    SessionAttributes {
        batch_size: 1,
        id: id.to_string(),
        application: "test-app".to_string(),
        common_data: None,
        tokens: Default::default(),
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq: Some(ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        }),
    }
}

async fn create_binding_executor(controller: &ControllerPtr, ssn_id: &str) -> String {
    controller
        .register_application("test-app".to_string(), create_test_application())
        .await
        .unwrap();
    controller
        .storage()
        .register_node(&create_test_node("bind-node"))
        .await
        .unwrap();
    controller
        .create_session(create_test_session_attr(ssn_id))
        .await
        .unwrap();
    let executor = controller
        .create_executor("bind-node".to_string(), ssn_id.to_string())
        .await
        .unwrap();
    controller.register_executor(&executor).await.unwrap();
    controller
        .bind_session(executor.id.clone(), ssn_id.to_string())
        .await
        .unwrap();
    executor.id
}

async fn completed_task_with_stale_executor() -> (ControllerPtr, String, Task) {
    completed_task_with_stale_executor_in(create_test_storage().await).await
}

async fn completed_task_with_stale_executor_in(
    storage: StoragePtr,
) -> (ControllerPtr, String, Task) {
    let controller = new_ptr(storage.clone());
    let ssn_id = format!("terminal-task-session-{}", uuid::Uuid::new_v4());
    let executor_id = create_binding_executor(&controller, &ssn_id).await;
    controller
        .bind_executor_completed(executor_id.clone(), None, None)
        .await
        .unwrap();
    let task = controller.create_task(ssn_id, None, None).await.unwrap();
    controller.launch_task(executor_id.clone()).await.unwrap();
    let session = storage.get_session_ptr(task.ssn_id.clone()).unwrap();
    let active = storage.get_task_ptr(task.gid()).unwrap();
    // Simulate a result commit before executor ownership was cleared.
    storage
        .update_task_result(
            session,
            active,
            TaskResult {
                state: TaskState::Succeed,
                output: Some(bytes::Bytes::from_static(b"first result")),
                message: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        storage.get_task_ptr(task.gid()),
        Err(FlameError::NotFound(_))
    ));
    (controller, executor_id, task)
}

async fn bound_task_waiter(minimum: u32) -> (ControllerPtr, String) {
    let config = FlameClusterContext {
        cluster: FlameCluster {
            storage: "none".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let controller = new_ptr(crate::storage::new_ptr(&config).await.unwrap());
    let executor_id = create_binding_executor(&controller, "task-wait-session").await;
    {
        let session = controller
            .storage
            .get_session_ptr("task-wait-session".into())
            .unwrap();
        lock_ptr!(session).unwrap().min_instances = minimum;
    }
    controller
        .bind_executor_completed(executor_id.clone(), None, None)
        .await
        .unwrap();
    (controller, executor_id)
}

// ========================================================================
// Controller::register_node Tests
// ========================================================================

// ========================================================================
// Controller::drain_node Tests
// ========================================================================

mod bind_session_failed;
mod controller;
mod delete_node;
mod drain_node;
mod register_node;
mod snapshot;
mod suspend;
mod wait_for_session;

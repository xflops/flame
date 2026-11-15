/*
Copyright 2025 The Flame Authors.
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
use common::apis::{Node, NodeInfo, NodeState, ResourceRequirement, Shim};
use common::ctx::{FlameCluster, FlameClusterContext, FlameExecutors, FlameLimits};

/// Helper to create a test node with specified state
fn create_test_node(name: &str, state: NodeState) -> NodePtr {
    stdng::new_ptr(Node {
        name: name.to_string(),
        state,
        capacity: ResourceRequirement::default(),
        allocatable: ResourceRequirement::default(),
        info: NodeInfo::default(),
        shims: vec![common::apis::Shim::Host],
    })
}

/// Helper to get current state from node pointer
fn get_state(node_ptr: &NodePtr) -> Result<NodeState, FlameError> {
    let node = lock_ptr!(node_ptr)?;
    Ok(node.state)
}

/// Creates a mock storage for testing state transitions (async version).
async fn create_mock_storage() -> StoragePtr {
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
            recovery: Default::default(),
            pprof: None,
        },
        cache: None,
    };

    crate::storage::new_ptr(&ctx).await.unwrap()
}

// ========================================================================
// UnknownState Tests
// ========================================================================

// ========================================================================
// ReadyState Tests
// ========================================================================

// ========================================================================
// NotReadyState Tests
// ========================================================================

// ========================================================================
// State Factory Tests
// ========================================================================

// ========================================================================
// Full State Machine Transition Tests
// ========================================================================

mod factory;
mod not_ready_state;
mod ready_state;
mod state_machine;
mod unknown_state;

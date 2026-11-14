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
use chrono::Utc;

/// Helper to create a test executor with given resreq.
fn create_test_executor(
    id: &str,
    resreq: ResourceRequirement,
    state: ExecutorState,
) -> ExecutorInfoPtr {
    Arc::new(ExecutorInfo {
        id: id.to_string(),
        node: "test-node".to_string(),
        resreq,
        shim: Shim::Host,
        application: String::new(),
        task_id: None,
        ssn_id: None,
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state,
        signal: None,
        attributes: HashSet::new(),
    })
}

/// Helper to create a test session with given resreq.
fn create_test_session(
    id: &str,
    resreq: Option<ResourceRequirement>,
    state: SessionState,
) -> SessionInfoPtr {
    Arc::new(SessionInfo {
        batch_size: 1,
        id: id.to_string(),
        application: "test-app".to_string(),
        tasks_status: HashMap::from([(TaskState::Pending, 1)]),
        creation_time: Utc::now(),
        completion_time: None,
        state,
        min_instances: 0,
        max_instances: None,
        priority: 0,
        resreq,
        retry_count: 0,
        task_index: HashMap::new(),
    })
}

/// Default per-slot unit kept for backward-compatible test scaffolding.
fn unit_rr() -> ResourceRequirement {
    ResourceRequirement {
        cpu: 1,
        memory: 1024,
        gpu: 0,
    }
}

/// Scale `unit_rr` by `n`. Mirrors the old `slots × unit` arithmetic the
/// pre-cleanup tests relied on.
fn slots_rr(n: u32) -> ResourceRequirement {
    unit_rr().mul(n)
}

mod snapshot;

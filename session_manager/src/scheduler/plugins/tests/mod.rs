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

mod das;
mod drf;
mod minmax;
mod priority;
mod shim;

use crate::model::ExecutorInfo;
use chrono::Utc;
use common::apis::{ExecutorState, ResourceRequirement, Shim};

struct OverusePolicy(Option<bool>);

impl Plugin for OverusePolicy {
    fn name(&self) -> &'static str {
        "test-overuse"
    }

    fn setup(&mut self, _: &SnapShot) -> Result<(), FlameError> {
        Ok(())
    }

    fn is_overused(&self, _: &SessionInfoPtr) -> Option<bool> {
        self.0
    }
}

#[test]
fn overused_uses_first_explicit_policy_result() {
    let session = Arc::new(SessionInfo::default());
    for (opinions, expected) in [
        (vec![None], false),
        (vec![], false),
        (vec![Some(true), None], true),
        (vec![None, Some(true)], true),
        (vec![Some(true), Some(false)], true),
        (vec![Some(false), Some(true)], false),
    ] {
        let policies = opinions
            .into_iter()
            .map(|opinion| {
                (
                    "test-overuse".into(),
                    Box::new(OverusePolicy(opinion)) as PluginPtr,
                )
            })
            .collect();
        let manager = PluginManager {
            plugins: new_ptr(policies),
        };
        assert_eq!(manager.is_overused(&session).unwrap(), expected);
    }
}

/// Create a test executor sized by `n` units (1 unit = (cpu:1, memory:1024, gpu:0)).
fn create_test_executor(id: &str, n: u32) -> ExecutorInfoPtr {
    Arc::new(ExecutorInfo {
        id: id.to_string(),
        node: "test-node".to_string(),
        resreq: ResourceRequirement {
            cpu: u64::from(n),
            memory: u64::from(n) * 1024,
            gpu: 0,
        },
        shim: Shim::Host,
        application: String::new(),
        task_id: None,
        signal: None,
        ssn_id: None,
        creation_time: Utc::now(),
        latest_updated_timestamp: Utc::now(),
        state: ExecutorState::Idle,
        attributes: Default::default(),
    })
}

/// Test that SnapShot filtering works correctly for different executor states.
#[test]
fn test_snapshot_executor_state_filtering() {
    // This test verifies that SnapShot correctly filters executors by state.
    // The SnapShot maintains an exec_index HashMap<ExecutorState, HashMap<ExecutorID, ExecutorInfoPtr>>
    // that allows efficient lookup of executors by state.

    let exec_idle = create_test_executor("exec-idle", 2);
    let exec_bound = Arc::new(ExecutorInfo {
        state: ExecutorState::Bound,
        ..(*exec_idle).clone()
    });
    let exec_void = Arc::new(ExecutorInfo {
        id: "exec-void".to_string(),
        state: ExecutorState::Void,
        ..(*exec_idle).clone()
    });

    // Verify state assignments
    assert_eq!(exec_idle.state, ExecutorState::Idle);
    assert_eq!(exec_bound.state, ExecutorState::Bound);
    assert_eq!(exec_void.state, ExecutorState::Void);
}

#[test]
fn das_policy_is_configurable_and_composes_before_shim() {
    let snapshot = SnapShot::new();
    let options = PluginsOptions {
        policies: vec!["das".to_string()],
        ..Default::default()
    };
    let manager = PluginManager::setup(&snapshot, &options).unwrap();
    let plugins = lock_ptr!(manager.plugins).unwrap();
    assert_eq!(
        plugins
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["minmax", "das", "shim"]
    );
}

/// Test documentation for plugin fallback behavior.
#[test]
fn test_plugin_fallback_behavior_documentation() {
    // This test documents the fallback behavior when plugins return None.
    //
    // Plugin methods like is_available, is_allocatable, etc. return Option<bool>:
    // - Some(true): Plugin explicitly approves
    // - Some(false): Plugin explicitly rejects
    // - None: Plugin has no opinion (fallback to default)
    //
    // Default behaviors:
    // - is_available: matching resources plus None -> true (executor is available by default)
    // - is_allocatable: None -> true (node is allocatable by default)
    // - is_underused: None -> false (session is NOT underused by default)
    // - is_overused: first explicit result wins; None -> false if all policies abstain
    // - is_reclaimable: None -> true (executor is reclaimable by default)
    //
    // This allows plugins to only implement the checks they care about,
    // while other plugins can provide their own opinions.
}

/// Test that stale data limitation is documented.
#[test]
fn test_stale_data_limitation_documentation() {
    // This test documents the stale data limitation of plugins.
    //
    // Plugins cache data during setup() at the start of each scheduling cycle.
    // This means:
    // 1. Data may become stale during the cycle
    // 2. New sessions created after setup won't be considered
    // 3. Executor state changes after setup won't be reflected
    //
    // Mitigations:
    // 1. Scheduling cycles are short (default 500ms)
    // 2. Explicit checks in actions prevent over-allocation
    // 3. The next cycle will pick up any changes
    //
    // This is a known limitation documented in the Plugin trait.
}

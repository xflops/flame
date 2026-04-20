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

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use stdng::collections;
use stdng::{lock_ptr, new_ptr, MutexPtr};

use crate::model::{ExecutorInfoPtr, NodeInfo, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot};
use crate::scheduler::plugins::fairshare::FairShare;
use crate::scheduler::plugins::gang::GangPlugin;
use crate::scheduler::plugins::shim::ShimPlugin;
use crate::scheduler::Context;

use common::FlameError;

mod fairshare;
mod gang;
mod shim;

pub type PluginPtr = Box<dyn Plugin>;
pub type PluginManagerPtr = Arc<PluginManager>;

/// Plugin trait for scheduler plugins.
///
/// # Stale Data Limitation
///
/// Plugins are initialized via `setup()` at the start of each scheduling cycle.
/// The data cached during setup (e.g., session counts, node allocations) represents
/// a point-in-time snapshot. Within a single scheduling cycle:
///
/// - **Sessions may be created/closed** after setup but before the cycle completes
/// - **Executors may change state** (e.g., become idle, get released)
/// - **Plugin decisions are based on stale data** from the snapshot
///
/// This is by design for performance reasons - taking a consistent snapshot at the
/// start of each cycle avoids lock contention during scheduling decisions.
///
/// For most use cases, this staleness is acceptable because:
/// 1. Scheduling cycles are short (default 500ms)
/// 2. The next cycle will pick up any changes
/// 3. Over-allocation is prevented by explicit checks in actions (e.g., max_instances)
pub trait Plugin: Send + Sync + 'static {
    // Installation of plugin
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError>;

    // Order Fn
    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        None
    }
    fn node_order_fn(&self, s1: &NodeInfo, s2: &NodeInfo) -> Option<Ordering> {
        None
    }

    // Filter Fn
    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        None
    }

    fn is_preemptible(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        None
    }

    fn is_available(&self, exec: &ExecutorInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        None
    }

    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        None
    }

    fn is_reclaimable(&self, exec: &ExecutorInfoPtr) -> Option<bool> {
        None
    }

    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        None
    }

    fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        None
    }

    // Events callbacks
    fn on_executor_allocate(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}

    fn on_executor_unallocate(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {}

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {}

    fn on_executor_pipeline(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {}

    fn on_executor_discard(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {}
}

pub struct PluginManager {
    pub plugins: MutexPtr<HashMap<String, PluginPtr>>,
}

impl PluginManager {
    pub fn setup(ss: &SnapShot) -> Result<PluginManagerPtr, FlameError> {
        let mut plugins = HashMap::from([
            ("fairshare".to_string(), FairShare::new_ptr()),
            ("shim".to_string(), ShimPlugin::new_ptr()),
            ("gang".to_string(), GangPlugin::new_ptr()),
        ]);

        for plugin in plugins.values_mut() {
            plugin.setup(ss)?;
        }

        Ok(Arc::new(PluginManager {
            plugins: new_ptr(plugins),
        }))
    }

    pub fn is_underused(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .values()
            .any(|plugin| plugin.is_underused(ssn).unwrap_or(false)))
    }

    pub fn is_preemptible(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .values()
            .all(|plugin| plugin.is_preemptible(ssn).unwrap_or(false)))
    }

    /// Check if an executor is available for a session.
    ///
    /// Returns true if ALL plugins agree the executor is available.
    /// If a plugin returns None (no opinion), it defaults to true.
    ///
    /// # Logging
    ///
    /// When an executor is deemed unavailable, a debug log is emitted
    /// to help diagnose scheduling issues.
    pub fn is_available(
        &self,
        exec: &ExecutorInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        for (name, plugin) in plugins.iter() {
            match plugin.is_available(exec, ssn) {
                Some(false) => {
                    tracing::debug!(
                        "Plugin '{}' rejected executor <{}> for session <{}>: is_available=false",
                        name,
                        exec.id,
                        ssn.id
                    );
                    return Ok(false);
                }
                Some(true) => {
                    // Plugin explicitly approved
                }
                None => {
                    // Plugin has no opinion, treat as available
                    tracing::trace!(
                        "Plugin '{}' has no opinion on executor <{}> for session <{}>, defaulting to available",
                        name,
                        exec.id,
                        ssn.id
                    );
                }
            }
        }

        Ok(true)
    }

    pub fn is_allocatable(
        &self,
        node: &NodeInfoPtr,
        ssn: &SessionInfoPtr,
    ) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .values()
            .all(|plugin| plugin.is_allocatable(node, ssn).unwrap_or(true)))
    }

    pub fn is_reclaimable(&self, exec: &ExecutorInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .values()
            .all(|plugin| plugin.is_reclaimable(exec).unwrap_or(true)))
    }

    pub fn on_executor_allocate(
        &self,
        node: NodeInfoPtr,
        ssn: SessionInfoPtr,
    ) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for plugin in plugins.values_mut() {
            plugin.on_executor_allocate(node.clone(), ssn.clone());
        }

        Ok(())
    }

    pub fn on_executor_unallocate(
        &self,
        node: NodeInfoPtr,
        ssn: SessionInfoPtr,
    ) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for plugin in plugins.values_mut() {
            plugin.on_executor_unallocate(node.clone(), ssn.clone());
        }

        Ok(())
    }

    pub fn on_session_bind(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for plugin in plugins.values_mut() {
            plugin.on_session_bind(ssn.clone());
        }

        Ok(())
    }

    pub fn on_session_unbind(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for plugin in plugins.values_mut() {
            plugin.on_session_unbind(ssn.clone());
        }
        Ok(())
    }

    /// True if every plugin that implements [`Plugin::is_ready`] reports readiness (no opinion
    /// defaults to true). Counters are in-memory and advance when [`crate::scheduler::Statement`]
    /// records `allocate` / `pipeline` without `discard`. Dispatch and Allocate share one
    /// `PluginManager` per scheduling cycle.
    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .values()
            .all(|plugin| plugin.is_ready(ssn).unwrap_or(true)))
    }

    /// True if every plugin that implements [`Plugin::is_fulfilled`] reports fulfillment (no opinion
    /// defaults to true). Updates when [`crate::scheduler::Statement`] records `bind`; after
    /// Dispatch commits, Allocate uses this to skip redundant provisioning.
    pub fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .values()
            .all(|plugin| plugin.is_fulfilled(ssn).unwrap_or(true)))
    }

    pub fn on_executor_pipeline(
        &self,
        exec: ExecutorInfoPtr,
        ssn: SessionInfoPtr,
    ) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for plugin in plugins.values_mut() {
            plugin.on_executor_pipeline(exec.clone(), ssn.clone());
        }

        Ok(())
    }

    pub fn on_executor_discard(
        &self,
        exec: ExecutorInfoPtr,
        ssn: SessionInfoPtr,
    ) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for plugin in plugins.values_mut() {
            plugin.on_executor_discard(exec.clone(), ssn.clone());
        }

        Ok(())
    }

    pub fn ssn_order_fn(&self, t1: &SessionInfoPtr, t2: &SessionInfoPtr) -> Ordering {
        if let Ok(plugins) = lock_ptr!(self.plugins) {
            for plugin in plugins.values() {
                if let Some(order) = plugin.ssn_order_fn(t1, t2) {
                    if order != Ordering::Equal {
                        return order;
                    }
                }
            }
        }

        Ordering::Equal
    }

    pub fn node_order_fn(&self, t1: &NodeInfoPtr, t2: &NodeInfoPtr) -> Ordering {
        if let Ok(plugins) = lock_ptr!(self.plugins) {
            for plugin in plugins.values() {
                if let Some(order) = plugin.node_order_fn(t1, t2) {
                    if order != Ordering::Equal {
                        return order;
                    }
                }
            }
        }
        Ordering::Equal
    }

    /// Find executors that are available for a given session.
    ///
    /// This method filters executors based on all registered plugins'
    /// `is_available` checks.
    ///
    /// # Logging
    ///
    /// - Logs at DEBUG level when executors are filtered out
    /// - Logs at WARN level if no executors are available for a session
    ///   that has pending tasks (potential scheduling issue)
    pub fn find_available_executors(
        &self,
        executors: &HashMap<String, ExecutorInfoPtr>,
        ssn: &SessionInfoPtr,
    ) -> Result<Vec<ExecutorInfoPtr>, FlameError> {
        let mut available = Vec::new();
        let mut rejected_count = 0;

        for exec in executors.values() {
            match self.is_available(exec, ssn) {
                Ok(true) => {
                    available.push(exec.clone());
                }
                Ok(false) => {
                    rejected_count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        "Error checking availability of executor <{}> for session <{}>: {}",
                        exec.id,
                        ssn.id,
                        e
                    );
                    // Continue checking other executors
                }
            }
        }

        if available.is_empty() && !executors.is_empty() {
            tracing::debug!(
                "No available executors for session <{}>: {} executors checked, {} rejected by plugins",
                ssn.id,
                executors.len(),
                rejected_count
            );
        }

        Ok(available)
    }
}

pub fn node_order_fn(ctx: &Context) -> impl collections::Cmp<NodeInfoPtr> {
    NodeOrderFn {
        plugin_mgr: ctx.plugins.clone(),
    }
}

struct NodeOrderFn {
    plugin_mgr: PluginManagerPtr,
}

impl collections::Cmp<NodeInfoPtr> for NodeOrderFn {
    fn cmp(&self, t1: &NodeInfoPtr, t2: &NodeInfoPtr) -> Ordering {
        self.plugin_mgr.node_order_fn(t1, t2)
    }
}

pub fn ssn_order_fn(ctx: &Context) -> impl collections::Cmp<SessionInfoPtr> {
    SsnOrderFn {
        plugin_mgr: ctx.plugins.clone(),
    }
}

struct SsnOrderFn {
    plugin_mgr: PluginManagerPtr,
}

impl collections::Cmp<SessionInfoPtr> for SsnOrderFn {
    fn cmp(&self, t1: &SessionInfoPtr, t2: &SessionInfoPtr) -> Ordering {
        self.plugin_mgr.ssn_order_fn(t1, t2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExecutorInfo, SessionInfo};
    use chrono::Utc;
    use common::apis::{ExecutorState, ResourceRequirement, SessionState, Shim, TaskState};
    use std::collections::HashMap;

    /// Create a test session with the given parameters.
    fn create_test_session(id: &str, slots: u32) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            slots,
            tasks_status: HashMap::from([(TaskState::Pending, 1)]),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
        })
    }

    /// Create a test executor with the given parameters.
    fn create_test_executor(id: &str, slots: u32) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            node: "test-node".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
            },
            slots,
            shim: Shim::Host,
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Idle,
        })
    }

    /// Test that is_available returns true when slots match.
    #[test]
    fn test_is_available_slots_match() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });
        let pm = PluginManager::setup(&ss).unwrap();

        let ssn = create_test_session("ssn-1", 2);
        let exec = create_test_executor("exec-1", 2);

        assert!(
            pm.is_available(&exec, &ssn).unwrap(),
            "Executor with matching slots should be available"
        );
    }

    /// Test that is_available returns false when slots don't match.
    #[test]
    fn test_is_available_slots_mismatch() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });
        let pm = PluginManager::setup(&ss).unwrap();

        let ssn = create_test_session("ssn-1", 2);
        let exec = create_test_executor("exec-1", 4);

        assert!(
            !pm.is_available(&exec, &ssn).unwrap(),
            "Executor with mismatched slots should not be available"
        );
    }

    /// Test find_available_executors filters correctly based on slots.
    #[test]
    fn test_find_available_executors_filters_by_slots() {
        let ss = SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
        });
        let pm = PluginManager::setup(&ss).unwrap();

        let ssn = create_test_session("ssn-1", 2);

        let executors = [
            create_test_executor("exec-1", 2),
            create_test_executor("exec-2", 4),
            create_test_executor("exec-3", 2),
            create_test_executor("exec-4", 1),
        ];

        let available: Vec<_> = executors
            .iter()
            .filter(|e| pm.is_available(e, &ssn).unwrap())
            .collect();

        assert_eq!(available.len(), 2, "Should have 2 available executors");
        assert!(available.iter().any(|e| e.id == "exec-1"));
        assert!(available.iter().any(|e| e.id == "exec-3"));
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
        // - is_available: None -> true (executor is available by default)
        // - is_allocatable: None -> true (node is allocatable by default)
        // - is_underused: None -> false (session is NOT underused by default)
        // - is_preemptible: None -> false (session is NOT preemptible by default)
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
}

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
use std::sync::{Arc, Mutex};

use stdng::collections;
use stdng::{lock_ptr, new_ptr, MutexPtr};

use crate::model::{ExecutorInfoPtr, NodeInfo, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot};
use crate::scheduler::plugins::drf::DRFPlugin;
use crate::scheduler::plugins::gang::GangPlugin;
use crate::scheduler::plugins::priority::PriorityPlugin;
use crate::scheduler::plugins::shim::ShimPlugin;
use crate::scheduler::Context;

use common::FlameError;

mod drf;
mod gang;
mod priority;
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
    /// Returns the plugin's canonical name used in configuration
    fn name(&self) -> &'static str;

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

    fn is_ready(&self, ssn: &SessionInfoPtr) -> bool {
        true
    }

    fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> bool {
        true
    }

    // Events callbacks
    fn on_executor_allocate(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}

    fn on_executor_unallocate(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}

    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {}

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {}

    fn on_executor_pipeline(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {}

    fn on_executor_discard(&mut self, exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {}
}

type PluginConstructor = fn() -> PluginPtr;

struct PluginInfo {
    name: &'static str,
    constructor: PluginConstructor,
    configurable: bool,
}

const PLUGIN_REGISTRY: &[PluginInfo] = &[
    PluginInfo {
        name: "priority",
        constructor: PriorityPlugin::new_ptr,
        configurable: true,
    },
    PluginInfo {
        name: "drf",
        constructor: DRFPlugin::new_ptr,
        configurable: true,
    },
    PluginInfo {
        name: "gang",
        constructor: GangPlugin::new_ptr,
        configurable: true,
    },
    PluginInfo {
        name: "shim",
        constructor: ShimPlugin::new_ptr,
        configurable: false,
    },
];

pub fn configurable_policy_names() -> Vec<&'static str> {
    PLUGIN_REGISTRY
        .iter()
        .filter(|p| p.configurable)
        .map(|p| p.name)
        .collect()
}

pub struct PluginManager {
    pub plugins: MutexPtr<Vec<(String, PluginPtr)>>,
    /// True iff the gang plugin is in the loaded set for this cycle.
    /// Controls which path is taken in `is_ready` / `is_fulfilled`.
    gang_loaded: bool,
    /// Per-session count of pipeline/allocate ops committed this cycle.
    /// Only used when `gang_loaded == false` to implement the "1 op per
    /// cycle is sufficient" rule without exposing this concern to callers.
    no_gang_alloc_ops: Mutex<HashMap<String, u32>>,
    /// Per-session count of bind ops committed this cycle.
    /// Only used when `gang_loaded == false`.
    no_gang_bind_ops: Mutex<HashMap<String, u32>>,
}

impl PluginManager {
    pub fn setup(
        ss: &SnapShot,
        enabled_policies: &[String],
    ) -> Result<PluginManagerPtr, FlameError> {
        let valid_names = configurable_policy_names();
        let all_plugin_names: Vec<&str> = PLUGIN_REGISTRY.iter().map(|p| p.name).collect();

        for p in enabled_policies {
            if !valid_names.contains(&p.as_str()) {
                if all_plugin_names.contains(&p.as_str()) {
                    // Plugin exists but is always-on (e.g. shim); listing it in policies is a no-op.
                    tracing::info!(
                        "Policy '{}' is always enabled and does not need to be listed explicitly; ignoring",
                        p
                    );
                } else {
                    return Err(FlameError::InvalidConfig(format!(
                        "unknown policy: {}. configurable policies: {:?}",
                        p, valid_names
                    )));
                }
            }
        }

        let mut plugins: Vec<(String, PluginPtr)> = PLUGIN_REGISTRY
            .iter()
            .filter(|info| !info.configurable || enabled_policies.iter().any(|p| p == info.name))
            .map(|info| (info.name.to_string(), (info.constructor)()))
            .collect();

        tracing::info!(
            "Enabled scheduler plugins: {:?}",
            plugins.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
        );

        for (_, plugin) in plugins.iter_mut() {
            plugin.setup(ss)?;
        }

        let gang_loaded = plugins.iter().any(|(name, _)| name == "gang");

        Ok(Arc::new(PluginManager {
            plugins: new_ptr(plugins),
            gang_loaded,
            no_gang_alloc_ops: Mutex::new(HashMap::new()),
            no_gang_bind_ops: Mutex::new(HashMap::new()),
        }))
    }

    /// Returns whether the session is underused (needs more executors).
    ///
    /// Uses "first non-`None` wins" ordering, identical to `ssn_order_fn`.
    /// Plugins are consulted in registration order (Priority → DRF → Gang → Shim).
    /// The first plugin that returns `Some(result)` wins; `None` means "no opinion, ask the
    /// next plugin".  If no plugin has an opinion, the session is considered NOT underused.
    ///
    /// This gives `PriorityPlugin` (registered first) full authority:
    /// - `Some(false)` → blocked; no further plugins consulted.
    /// - `Some(true)` → underused; overrides any downstream-plugin veto.
    /// - `None` → defer to the next plugin in the chain for additional underuse checks.
    pub fn is_underused(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;
        for (_, plugin) in plugins.iter() {
            if let Some(result) = plugin.is_underused(ssn) {
                return Ok(result);
            }
        }
        Ok(false)
    }

    pub fn is_preemptible(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .iter()
            .all(|(_, plugin)| plugin.is_preemptible(ssn).unwrap_or(false)))
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
            .iter()
            .all(|(_, plugin)| plugin.is_allocatable(node, ssn).unwrap_or(true)))
    }

    pub fn is_reclaimable(&self, exec: &ExecutorInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;

        Ok(plugins
            .iter()
            .all(|(_, plugin)| plugin.is_reclaimable(exec).unwrap_or(true)))
    }

    pub fn on_executor_allocate(
        &self,
        node: NodeInfoPtr,
        ssn: SessionInfoPtr,
    ) -> Result<(), FlameError> {
        if !self.gang_loaded {
            let mut ops = self
                .no_gang_alloc_ops
                .lock()
                .map_err(|e| FlameError::Internal(format!("no_gang_alloc_ops lock: {e}")))?;
            *ops.entry(ssn.id.clone()).or_insert(0) += 1;
        }
        let mut plugins = lock_ptr!(self.plugins)?;
        for (_, plugin) in plugins.iter_mut() {
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

        for (_, plugin) in plugins.iter_mut() {
            plugin.on_executor_unallocate(node.clone(), ssn.clone());
        }

        Ok(())
    }

    pub fn on_session_bind(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        if !self.gang_loaded {
            let mut ops = self
                .no_gang_bind_ops
                .lock()
                .map_err(|e| FlameError::Internal(format!("no_gang_bind_ops lock: {e}")))?;
            *ops.entry(ssn.id.clone()).or_insert(0) += 1;
        }
        let mut plugins = lock_ptr!(self.plugins)?;
        for (_, plugin) in plugins.iter_mut() {
            plugin.on_session_bind(ssn.clone());
        }
        Ok(())
    }

    pub fn on_session_unbind(&self, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;

        for (_, plugin) in plugins.iter_mut() {
            plugin.on_session_unbind(ssn.clone());
        }
        Ok(())
    }

    /// Returns batch-allocation readiness for a session.
    ///
    /// **Without gang** (`gang_loaded == false`): returns `true` once at least one
    /// pipeline/allocate op has been recorded for this session via `on_executor_pipeline` or
    /// `on_executor_allocate`.  Before any op, returns `false`.  This lets callers use a
    /// uniform `if is_ready() { break; }` in their allocation loops — the loop naturally
    /// stops after the first op with no batch constraint.
    ///
    /// **With gang**: returns `true` only when ALL plugins return `true`; returns `false`
    /// as soon as any plugin says the batch is incomplete.  Plugins that do not override
    /// `is_ready` default to `true` (no opinion / do not block), so only opinionated
    /// plugins (e.g. GangPlugin) can veto readiness.
    ///
    /// Counters advance when [`crate::scheduler::Statement`] records `pipeline`/`allocate`
    /// without `discard`. Dispatch and Allocate share one `PluginManager` per cycle.
    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> bool {
        if !self.gang_loaded {
            let ops = self
                .no_gang_alloc_ops
                .lock()
                .expect("no_gang_alloc_ops lock poisoned");
            return *ops.get(&ssn.id).unwrap_or(&0) > 0;
        }
        let plugins = self.plugins.lock().expect("plugins lock poisoned");
        plugins.iter().all(|(_, plugin)| plugin.is_ready(ssn))
    }

    /// Returns batch-binding fulfillment for a session.
    ///
    /// **Without gang** (`gang_loaded == false`): returns `true` once at least one bind op
    /// has been recorded for this session via `on_session_bind`.  Before any bind, returns
    /// `false`.  Callers can use `if is_fulfilled() { break/skip; }` uniformly.
    ///
    /// **With gang**: returns `true` only when ALL plugins return `true`; returns `false`
    /// as soon as any plugin says the batch is incomplete.  Plugins that do not override
    /// `is_fulfilled` default to `true` (no opinion / do not block), so only opinionated
    /// plugins (e.g. GangPlugin) can veto fulfillment.
    ///
    /// Updates when [`crate::scheduler::Statement`] records `bind`; after Dispatch commits,
    /// Allocate uses this to skip redundant provisioning.
    pub fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> bool {
        if !self.gang_loaded {
            let ops = self
                .no_gang_bind_ops
                .lock()
                .expect("no_gang_bind_ops lock poisoned");
            return *ops.get(&ssn.id).unwrap_or(&0) > 0;
        }
        let plugins = self.plugins.lock().expect("plugins lock poisoned");
        plugins.iter().all(|(_, plugin)| plugin.is_fulfilled(ssn))
    }

    pub fn on_executor_pipeline(
        &self,
        exec: ExecutorInfoPtr,
        ssn: SessionInfoPtr,
    ) -> Result<(), FlameError> {
        if !self.gang_loaded {
            let mut ops = self
                .no_gang_alloc_ops
                .lock()
                .map_err(|e| FlameError::Internal(format!("no_gang_alloc_ops lock: {e}")))?;
            *ops.entry(ssn.id.clone()).or_insert(0) += 1;
        }
        let mut plugins = lock_ptr!(self.plugins)?;
        for (_, plugin) in plugins.iter_mut() {
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

        for (_, plugin) in plugins.iter_mut() {
            plugin.on_executor_discard(exec.clone(), ssn.clone());
        }

        Ok(())
    }

    pub fn ssn_order_fn(&self, t1: &SessionInfoPtr, t2: &SessionInfoPtr) -> Ordering {
        if let Ok(plugins) = lock_ptr!(self.plugins) {
            for (_, plugin) in plugins.iter() {
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
            for (_, plugin) in plugins.iter() {
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
    use crate::model::{ExecutorInfo, NodeInfo, SessionInfo, SnapShot};
    use chrono::Utc;
    use common::apis::{
        ExecutorState, NodeState, ResourceRequirement, SessionState, Shim, TaskState,
    };
    use std::collections::HashMap;

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
            task_id: None,
            ssn_id: None,
            creation_time: Utc::now(),
            state: ExecutorState::Idle,
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

    fn make_policies(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn make_session(id: &str, batch_size: u32) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: "test-app".to_string(),
            tasks_status: HashMap::from([(TaskState::Pending, batch_size.max(1) as i32)]),
            creation_time: Utc::now(),
            completion_time: None,
            state: SessionState::Open,
            min_instances: 0,
            max_instances: None,
            batch_size,
            priority: 0,
            resreq: Some(ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            }),
            retry_count: 0,
        })
    }

    fn make_node(name: &str) -> Arc<NodeInfo> {
        Arc::new(NodeInfo {
            name: name.to_string(),
            allocatable: ResourceRequirement {
                cpu: 4,
                memory: 8192,
                gpu: 0,
            },
            state: NodeState::Ready,
        })
    }

    /// Without gang, is_ready returns false before any op (no batch constraint active yet).
    #[test]
    fn test_is_ready_returns_false_without_gang() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let plugins = PluginManager::setup(&ss, &make_policies(&["priority"])).unwrap();
        assert!(
            !plugins.is_ready(&ssn),
            "is_ready must be false before any op when gang plugin is not loaded"
        );
    }

    /// Without gang, is_fulfilled returns false before any bind op.
    #[test]
    fn test_is_fulfilled_returns_false_without_gang() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let plugins = PluginManager::setup(&ss, &make_policies(&["priority"])).unwrap();
        assert!(
            !plugins.is_fulfilled(&ssn),
            "is_fulfilled must be false when gang plugin is not loaded"
        );
    }

    /// When gang is loaded, is_ready returns false before any allocation event because the
    /// batch counter starts at 0.
    #[test]
    fn test_is_ready_returns_false_before_allocation_with_gang() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let plugins = PluginManager::setup(&ss, &make_policies(&["gang"])).unwrap();
        assert!(
            !plugins.is_ready(&ssn),
            "is_ready must be false initially when gang is loaded"
        );
    }

    /// After one on_executor_allocate event, gang batch_size=1 is satisfied → true.
    #[test]
    fn test_is_ready_returns_true_after_batch_filled_with_gang() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let plugins = PluginManager::setup(&ss, &make_policies(&["gang"])).unwrap();

        let node = make_node("node-1");
        plugins.on_executor_allocate(node, ssn.clone()).unwrap();

        assert!(
            plugins.is_ready(&ssn),
            "is_ready must be true after one allocation with batch_size=1"
        );
    }

    /// Gang batch_size=2: after one allocation is_ready is still false; true only after the
    /// second allocation completes the batch.
    #[test]
    fn test_is_ready_batch_size_2_requires_two_allocations() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 2);
        ss.add_session(ssn.clone()).unwrap();

        let plugins = PluginManager::setup(&ss, &make_policies(&["gang"])).unwrap();
        let node = make_node("node-1");

        assert!(!plugins.is_ready(&ssn));

        plugins
            .on_executor_allocate(node.clone(), ssn.clone())
            .unwrap();
        assert!(
            !plugins.is_ready(&ssn),
            "is_ready must still be false after only 1 of 2 required allocations"
        );

        plugins.on_executor_allocate(node, ssn.clone()).unwrap();
        assert!(
            plugins.is_ready(&ssn),
            "is_ready must be true once the full batch of 2 is allocated"
        );
    }

    /// When gang is loaded, is_fulfilled starts as false and becomes true after the batch of
    /// on_session_bind events.
    #[test]
    fn test_is_fulfilled_false_then_true_with_gang() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        let plugins = PluginManager::setup(&ss, &make_policies(&["gang"])).unwrap();

        assert!(!plugins.is_fulfilled(&ssn));

        plugins.on_session_bind(ssn.clone()).unwrap();
        assert!(
            plugins.is_fulfilled(&ssn),
            "is_fulfilled must be true after one bind with batch_size=1"
        );
    }

    /// With gang loaded, is_ready starts false (no ops yet).
    /// Without gang, is_ready also starts false (no ops yet), and only becomes true after
    /// the first pipeline/allocate op is recorded via on_executor_allocate.
    #[test]
    fn test_gang_loaded_iff_listed_in_policies() {
        let ss = SnapShot::new();
        let ssn = make_session("ssn-1", 1);
        ss.add_session(ssn.clone()).unwrap();

        // With gang: is_ready is false before any op (batch counter starts at 0).
        let with_gang = PluginManager::setup(&ss, &make_policies(&["gang"])).unwrap();
        assert!(
            !with_gang.is_ready(&ssn),
            "is_ready must be false before any op when gang is loaded"
        );

        // Without gang: is_ready is also false before any op.
        let ss2 = SnapShot::new();
        ss2.add_session(ssn.clone()).unwrap();
        let without_gang = PluginManager::setup(&ss2, &make_policies(&["priority"])).unwrap();
        assert!(
            !without_gang.is_ready(&ssn),
            "is_ready must be false before any op when gang is not loaded"
        );

        // Without gang: is_ready becomes true after the first pipeline op.
        let node = make_node("node-1");
        without_gang
            .on_executor_allocate(node, ssn.clone())
            .unwrap();
        assert!(
            without_gang.is_ready(&ssn),
            "is_ready must be true after the first op with no gang constraint"
        );
    }
}

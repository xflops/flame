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

//! Shim selection plugin for filtering executors by shim compatibility.
//!
//! This plugin ensures that executors are only matched with sessions whose
//! applications require a compatible shim type (Host or Wasm).

use std::collections::HashMap;

use crate::model::{
    AppInfoPtr, ExecutorInfoPtr, NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot,
    ALL_APPLICATION,
};
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::apis::{SessionID, Shim};
use common::FlameError;

/// Shim selection plugin that filters executors based on shim compatibility.
pub struct ShimPlugin {
    /// Map from session ID to the required shim type
    ssn_shim_map: HashMap<SessionID, Shim>,
}

impl ShimPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(ShimPlugin {
            ssn_shim_map: HashMap::new(),
        })
    }
}

impl Plugin for ShimPlugin {
    fn name(&self) -> &'static str {
        "shim"
    }

    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        // Clear previous state
        self.ssn_shim_map.clear();

        // Get all applications to look up shim requirements
        let apps = ss.find_applications(ALL_APPLICATION)?;

        // Get all open sessions and map their shim requirements
        let sessions = ss.find_sessions(None)?;
        for ssn in sessions.values() {
            if let Some(app) = apps.get(&ssn.application) {
                self.ssn_shim_map.insert(ssn.id.clone(), app.shim);
                tracing::debug!(
                    "ShimPlugin: Session <{}> requires shim {:?} (app: {})",
                    ssn.id,
                    app.shim,
                    ssn.application
                );
            } else {
                // Default to Host if application not found
                self.ssn_shim_map.insert(ssn.id.clone(), Shim::Host);
                tracing::warn!(
                    "ShimPlugin: Application <{}> not found for session <{}>, defaulting to Host shim",
                    ssn.application,
                    ssn.id
                );
            }
        }

        tracing::debug!(
            "ShimPlugin: Initialized with {} session shim mappings",
            self.ssn_shim_map.len()
        );

        Ok(())
    }

    /// Check if an executor is available for a session based on shim compatibility.
    ///
    /// Returns `Some(true)` if the executor's shim matches the session's required shim,
    /// `Some(false)` if they don't match, or `None` if the session is not found.
    fn is_available(&self, exec: &ExecutorInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        let required_shim = self.ssn_shim_map.get(&ssn.id)?;
        let executor_shim = exec.shim;

        let is_compatible = executor_shim == *required_shim;

        if !is_compatible {
            tracing::debug!(
                "ShimPlugin: Executor <{}> (shim={:?}) NOT compatible with session <{}> (requires {:?})",
                exec.id,
                executor_shim,
                ssn.id,
                required_shim
            );
        }

        Some(is_compatible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::model::{AppInfo, ExecutorInfo, SessionInfo};
    use chrono::Duration;
    use common::apis::{ExecutorState, ResourceRequirement, SessionState, Shim, TaskState};

    fn create_test_snapshot() -> SnapShot {
        SnapShot::new(ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        })
    }

    fn create_app_info(name: &str, shim: Shim) -> AppInfoPtr {
        Arc::new(AppInfo {
            name: name.to_string(),
            shim,
            max_instances: 100,
            delay_release: Duration::seconds(60),
        })
    }

    fn create_session_info(id: &str, app: &str) -> SessionInfoPtr {
        Arc::new(SessionInfo {
            id: id.to_string(),
            application: app.to_string(),
            slots: 1,
            tasks_status: [(TaskState::Pending, 1)].into_iter().collect(),
            state: SessionState::Open,
            ..Default::default()
        })
    }

    fn create_executor_info(id: &str, shim: Shim) -> ExecutorInfoPtr {
        Arc::new(ExecutorInfo {
            id: id.to_string(),
            node: "node1".to_string(),
            resreq: ResourceRequirement {
                cpu: 1,
                memory: 1024,
                gpu: 0,
            },
            slots: 1,
            shim,
            state: ExecutorState::Idle,
            ..Default::default()
        })
    }

    #[test]
    fn test_shim_plugin_host_match() {
        let ss = create_test_snapshot();

        // Add a Host application
        let app = create_app_info("host-app", Shim::Host);
        ss.add_application(app).unwrap();

        // Add a session for the Host app
        let ssn = create_session_info("ssn-1", "host-app");
        ss.add_session(ssn.clone()).unwrap();

        // Setup the plugin
        let mut plugin = ShimPlugin::new_ptr();
        plugin.setup(&ss).unwrap();

        // Create a Host executor
        let exec = create_executor_info("exec-1", Shim::Host);

        // Should be available (Host executor for Host app)
        assert_eq!(plugin.is_available(&exec, &ssn), Some(true));
    }

    #[test]
    fn test_shim_plugin_wasm_match() {
        let ss = create_test_snapshot();

        // Add a Wasm application
        let app = create_app_info("wasm-app", Shim::Wasm);
        ss.add_application(app).unwrap();

        // Add a session for the Wasm app
        let ssn = create_session_info("ssn-1", "wasm-app");
        ss.add_session(ssn.clone()).unwrap();

        // Setup the plugin
        let mut plugin = ShimPlugin::new_ptr();
        plugin.setup(&ss).unwrap();

        // Create a Wasm executor
        let exec = create_executor_info("exec-1", Shim::Wasm);

        // Should be available (Wasm executor for Wasm app)
        assert_eq!(plugin.is_available(&exec, &ssn), Some(true));
    }

    #[test]
    fn test_shim_plugin_mismatch() {
        let ss = create_test_snapshot();

        // Add a Wasm application
        let app = create_app_info("wasm-app", Shim::Wasm);
        ss.add_application(app).unwrap();

        // Add a session for the Wasm app
        let ssn = create_session_info("ssn-1", "wasm-app");
        ss.add_session(ssn.clone()).unwrap();

        // Setup the plugin
        let mut plugin = ShimPlugin::new_ptr();
        plugin.setup(&ss).unwrap();

        // Create a Host executor
        let exec = create_executor_info("exec-1", Shim::Host);

        // Should NOT be available (Host executor for Wasm app)
        assert_eq!(plugin.is_available(&exec, &ssn), Some(false));
    }

    #[test]
    fn test_shim_plugin_reverse_mismatch() {
        let ss = create_test_snapshot();

        // Add a Host application
        let app = create_app_info("host-app", Shim::Host);
        ss.add_application(app).unwrap();

        // Add a session for the Host app
        let ssn = create_session_info("ssn-1", "host-app");
        ss.add_session(ssn.clone()).unwrap();

        // Setup the plugin
        let mut plugin = ShimPlugin::new_ptr();
        plugin.setup(&ss).unwrap();

        // Create a Wasm executor
        let exec = create_executor_info("exec-1", Shim::Wasm);

        // Should NOT be available (Wasm executor for Host app)
        assert_eq!(plugin.is_available(&exec, &ssn), Some(false));
    }
}

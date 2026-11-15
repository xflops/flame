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

    /// Check if a node can be asked for an executor for a session: the node's executor
    /// manager has to run the shim of the session's application. A node that reported no
    /// shims runs none.
    ///
    /// Returns `None` if the session is not found.
    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> {
        let required_shim = self.ssn_shim_map.get(&ssn.id)?;
        let is_compatible = node.shims.contains(required_shim);

        if !is_compatible {
            tracing::debug!(
                "ShimPlugin: Node <{}> (shims={:?}) NOT compatible with session <{}> (requires {:?})",
                node.name,
                node.shims,
                ssn.id,
                required_shim
            );
        }

        Some(is_compatible)
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

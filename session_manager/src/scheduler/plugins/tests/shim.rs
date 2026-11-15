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

use std::sync::Arc;

use chrono::Duration;

use super::super::shim::ShimPlugin;
use super::super::{Plugin, PluginPtr};
use crate::model::{
    AppInfo, AppInfoPtr, ExecutorInfo, ExecutorInfoPtr, NodeInfo, NodeInfoPtr, SessionInfo,
    SessionInfoPtr, SnapShot,
};
use common::apis::{
    ApplicationState, ExecutorState, ResourceRequirement, SessionState, Shim, TaskState,
};

fn create_test_snapshot() -> SnapShot {
    SnapShot::new()
}

fn create_app_info(name: &str, shim: Shim) -> AppInfoPtr {
    Arc::new(AppInfo {
        name: name.to_string(),
        state: ApplicationState::Enabled,
        shim,
        max_instances: 100,
        delay_release: Duration::seconds(60),
    })
}

fn create_session_info(id: &str, app: &str) -> SessionInfoPtr {
    Arc::new(SessionInfo {
        id: id.to_string(),
        application: app.to_string(),
        tasks_status: [(TaskState::Pending, 1)].into_iter().collect(),
        state: SessionState::Open,
        resreq: Some(ResourceRequirement {
            cpu: 1,
            memory: 1024,
            gpu: 0,
        }),
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
fn create_node_info(name: &str, shims: Vec<Shim>) -> NodeInfoPtr {
    Arc::new(NodeInfo {
        name: name.to_string(),
        shims,
        ..Default::default()
    })
}

fn setup_plugin(app_shim: Shim) -> (PluginPtr, SessionInfoPtr) {
    let ss = create_test_snapshot();
    ss.add_application(create_app_info("app", app_shim))
        .unwrap();
    let ssn = create_session_info("ssn-1", "app");
    ss.add_session(ssn.clone()).unwrap();
    let mut plugin = ShimPlugin::new_ptr();
    plugin.setup(&ss).unwrap();
    (plugin, ssn)
}

#[test]
fn test_node_that_runs_the_shim_is_allocatable() {
    let (plugin, ssn) = setup_plugin(Shim::Host);
    assert_eq!(
        plugin.is_allocatable(&create_node_info("n", vec![Shim::Host]), &ssn),
        Some(true)
    );
    // One of several.
    assert_eq!(
        plugin.is_allocatable(&create_node_info("n", vec![Shim::Cri, Shim::Host]), &ssn),
        Some(true)
    );
}

#[test]
fn test_node_that_does_not_run_the_shim_is_not_allocatable() {
    let (plugin, ssn) = setup_plugin(Shim::Host);
    assert_eq!(
        plugin.is_allocatable(&create_node_info("n", vec![Shim::Cri]), &ssn),
        Some(false)
    );

    let (plugin, ssn) = setup_plugin(Shim::Cri);
    assert_eq!(
        plugin.is_allocatable(&create_node_info("n", vec![Shim::Host, Shim::Wasm]), &ssn),
        Some(false)
    );
}

#[test]
fn test_node_that_reported_no_shims_is_not_allocatable() {
    let (plugin, ssn) = setup_plugin(Shim::Host);
    assert_eq!(
        plugin.is_allocatable(&create_node_info("n", vec![]), &ssn),
        Some(false)
    );
}

#[test]
fn test_unknown_session_does_not_decide_node_allocation() {
    let (plugin, _) = setup_plugin(Shim::Host);
    let other = create_session_info("other", "app");
    assert_eq!(
        plugin.is_allocatable(&create_node_info("n", vec![Shim::Host]), &other),
        None
    );
}

/*
Copyright 2026 The Flame Authors.
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

use super::*;
use crate::model::{AppInfo, ExecutorInfo, NodeInfo, SessionInfo};
use common::apis::TaskState;

fn plugin(snapshot: &SnapShot) -> MinMaxPlugin {
    let mut plugin = MinMaxPlugin::default();
    plugin.setup(snapshot).unwrap();
    plugin
}

fn limited_session(max_instances: Option<u32>) -> SessionInfoPtr {
    Arc::new(SessionInfo {
        id: "session".to_string(),
        application: "app".to_string(),
        max_instances,
        tasks_status: [(TaskState::Pending, 10)].into_iter().collect(),
        ..Default::default()
    })
}

fn executor(id: &str, node: &str, state: ExecutorState) -> ExecutorInfoPtr {
    Arc::new(ExecutorInfo {
        id: id.to_string(),
        node: node.to_string(),
        state,
        ..Default::default()
    })
}

mod minmax;

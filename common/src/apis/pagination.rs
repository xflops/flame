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

//! Bounded list requests and their filtered collection summaries.

use super::{ApplicationState, ExecutorState, NodeState, SessionState, TaskID, TaskState};
use std::collections::BTreeMap;

/// Optional paging settings. A zero size selects the server default of 100.
#[derive(Clone, Debug, Default)]
pub struct PageRequest {
    pub page_size: i32,
    pub page_number: i64,
    pub order_by: String,
}

/// Collection counts and the selected page for a list response.
#[derive(Clone, Debug, Default)]
pub struct PageInfo {
    pub page_number: i64,
    pub page_size: i32,
    pub total_pages: u64,
    pub total_count: u64,
    pub resource_count: u64,
    pub state_counts: BTreeMap<String, u64>,
    pub totals: BTreeMap<String, u64>,
}

/// A bounded selection and its collection summary.
#[derive(Clone, Debug)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub paging: PageInfo,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationPageFilter {
    pub state: Option<ApplicationState>,
    pub names: Vec<String>,
}
#[derive(Clone, Debug, Default)]
pub struct SessionPageFilter {
    pub application: Option<String>,
    pub state: Option<SessionState>,
    pub ids: Vec<String>,
}
#[derive(Clone, Debug, Default)]
pub struct NodePageFilter {
    pub state: Option<NodeState>,
    pub names: Vec<String>,
}
#[derive(Clone, Debug, Default)]
pub struct ExecutorPageFilter {
    pub state: Option<ExecutorState>,
    pub application: Option<String>,
    pub node: Option<String>,
    pub session_id: Option<String>,
    pub ids: Vec<String>,
}
#[derive(Clone, Debug, Default)]
pub struct TaskPageFilter {
    pub session_id: String,
    pub state: Option<TaskState>,
    pub ids: Vec<TaskID>,
    pub include_payloads: bool,
}

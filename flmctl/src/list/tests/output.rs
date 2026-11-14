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

use flame_rs::apis::ExecutorState;

use super::*;

#[test]
fn executor_table_shows_application() {
    let table = executor_table(&[Executor {
        id: "executor-1".to_string(),
        application: "app-1".to_string(),
        state: ExecutorState::Idle,
        session_id: None,
        node: "node-1".to_string(),
    }])
    .to_string();

    assert!(table.contains("App"));
    assert!(table.contains("app-1"));
}

#[test]
fn executor_json_is_an_array_with_state() {
    let json = format_json(&[Executor {
        id: "executor-1".to_string(),
        application: "app-1".to_string(),
        state: ExecutorState::Idle,
        session_id: None,
        node: "node-1".to_string(),
    }])
    .unwrap();

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value[0]["id"], "executor-1");
    assert_eq!(value[0]["state"], "Idle");
    assert_eq!(value[0]["application"], "app-1");
}

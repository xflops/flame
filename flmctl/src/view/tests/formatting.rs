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
use chrono::TimeZone;
use flame_rs::client::Event;

#[test]
fn format_events_includes_session_event_details() {
    let event = Event {
        code: 1001,
        message: Some("bind failed".to_string()),
        creation_time: chrono::Utc.with_ymd_and_hms(2026, 5, 8, 10, 1, 2).unwrap(),
    };

    let formatted = format_events(&[event]);

    assert!(formatted.contains("10:01:02.000"));
    assert!(formatted.contains("bind failed"));
    assert!(formatted.contains("(1001)"));
}

#[test]
fn format_task_summary_includes_all_states() {
    let summary = format_task_summary(3, 1, 2, 1);

    assert_eq!(summary, "3 pending, 1 running, 2 succeed, 1 failed");
    assert!(!summary.contains("4 failed"));
}

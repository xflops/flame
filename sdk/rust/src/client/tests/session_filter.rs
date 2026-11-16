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

#[test]
fn default_filter_requests_the_default_page() {
    let request = SessionFilter::default().to_request();
    assert_eq!(request.application, None);
    assert_eq!(request.state, None);
    assert!(request.ids.is_empty());
    assert_eq!(request.paging, None);
}

#[test]
fn filter_maps_state_application_and_ids() {
    let request = SessionFilter {
        application: Some("app".to_string()),
        state: Some(SessionState::Closed),
        ids: vec!["a".to_string(), "b".to_string()],
        ..SessionFilter::default()
    }
    .to_request();
    assert_eq!(request.application.as_deref(), Some("app"));
    assert_eq!(request.state, Some(SessionState::Closed as i32));
    assert_eq!(request.ids, vec!["a".to_string(), "b".to_string()]);
}

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

use super::*;

fn args(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[test]
fn no_values_select_filtered_defaults_for_each_kind() {
    assert_eq!(
        SessionSelection::parse(&[]).unwrap(),
        SessionSelection::AllClosed
    );
    assert_eq!(
        NodeSelection::parse(&[]).unwrap(),
        NodeSelection::AllNotReady
    );
}

#[test]
fn named_ids_are_trimmed_and_node_duplicates_are_deleted_once() {
    let sessions = args(&["id-1", " id-2 "]);
    assert_eq!(
        SessionSelection::parse(&sessions).unwrap(),
        SessionSelection::Ids(vec!["id-1", "id-2"])
    );
    let nodes = args(&["worker-a", " worker-b ", "worker-a"]);
    assert_eq!(
        NodeSelection::parse(&nodes).unwrap(),
        NodeSelection::Ids(vec!["worker-a", "worker-b"])
    );
}

#[tokio::test]
async fn selection_errors_are_reported_before_connecting() {
    let ctx = FlameContext::default();
    for (sessions, nodes) in [(None, None), (Some([].as_slice()), Some([].as_slice()))] {
        let error = run(&ctx, sessions, nodes).await.unwrap_err().to_string();
        assert!(error.contains("--session"), "{error}");
    }
    for values in [args(&[""]), args(&["id-1", " "])] {
        let error = delete_sessions(&ctx, &values)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("session ids"), "{error}");
        let error = delete_nodes(&ctx, &values).await.unwrap_err().to_string();
        assert!(error.contains("node names"), "{error}");
    }
}

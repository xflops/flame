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

#[tokio::test]
async fn deletion_is_persistent_and_keeps_other_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("fs://{}", dir.path().display());
    let config = FlameClusterContext {
        cluster: FlameCluster {
            storage: url,
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = crate::storage::new_ptr(&config).await.unwrap();
    let controller = new_ptr(storage.clone());
    for (name, state) in [
        ("unknown-old", NodeState::Unknown),
        ("not-ready-old", NodeState::NotReady),
        ("current", NodeState::Ready),
    ] {
        let mut node = create_test_node(name);
        node.state = state;
        storage.register_node(&node).await.unwrap();
    }
    assert!(matches!(
        controller.release_node("current").await,
        Err(FlameError::InvalidState(_))
    ));
    for name in ["unknown-old", "not-ready-old"] {
        controller.release_node(name).await.unwrap();
        assert!(controller.get_node(name).unwrap().is_none());
        assert!(matches!(
            controller.release_node(name).await,
            Err(FlameError::NotFound(_))
        ));
    }
    let recovered = crate::storage::new_ptr(&config).await.unwrap();
    recovered.load_data().await.unwrap();
    let nodes = recovered.list_nodes().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].name, "current");
    controller
        .register_node(&create_test_node("unknown-old"), &[])
        .await
        .unwrap();
    assert_eq!(
        controller.get_node("unknown-old").unwrap().unwrap().state,
        NodeState::Ready
    );
}

#[tokio::test]
async fn deletion_rejects_every_executor_state() {
    let storage = create_test_storage().await;
    let controller = new_ptr(storage.clone());
    controller
        .register_application("test-app".into(), create_test_application())
        .await
        .unwrap();
    controller
        .create_session(create_test_session_attr("node-delete-executors"))
        .await
        .unwrap();
    let node = create_test_node("occupied");
    storage.register_node(&node).await.unwrap();
    let mut executor = controller
        .create_executor(node.name.clone(), "node-delete-executors".into())
        .await
        .unwrap();
    for node_state in [NodeState::Unknown, NodeState::NotReady] {
        storage
            .update_node_state(&node.name, node_state)
            .await
            .unwrap();
        for state in [
            ExecutorState::Void,
            ExecutorState::Idle,
            ExecutorState::Bound,
            ExecutorState::Releasing,
            ExecutorState::Released,
        ] {
            executor.state = state;
            storage.update_executor(&executor).await.unwrap();
            assert!(matches!(
                controller.release_node(&node.name).await,
                Err(FlameError::InvalidState(_))
            ));
            assert_eq!(
                controller.get_executor(executor.id.clone()).unwrap().state,
                state
            );
            assert_eq!(
                controller.get_node(&node.name).unwrap().unwrap().state,
                node_state
            );
        }
    }
}

#[tokio::test]
async fn deletion_validates_the_node_name() {
    let controller = new_ptr(create_test_storage().await);
    for name in ["", "   "] {
        assert!(matches!(
            controller.release_node(name).await,
            Err(FlameError::InvalidConfig(_))
        ));
    }
    assert!(matches!(
        controller.release_node("missing").await,
        Err(FlameError::NotFound(_))
    ));
}

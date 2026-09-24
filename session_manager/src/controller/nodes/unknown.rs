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

use crate::controller::nodes::NodeStates;
use crate::model::ExecutorFilter;
use crate::storage::StoragePtr;
use stdng::{lock_ptr, logs::TraceFn, trace_fn, MutexPtr};

use common::apis::{Node, NodePtr, NodeState};
use common::FlameError;

/// State handler for nodes in Unknown state.
///
/// Unknown state means the node was previously connected but has disconnected,
/// and the cleanup timer is running. It can transition to:
/// - Ready: if the node reconnects (register_node)
/// - NotReady: if the cleanup timer expires (shutdown)
pub struct UnknownState {
    pub storage: StoragePtr,
    pub node: NodePtr,
}

#[async_trait::async_trait]
impl NodeStates for UnknownState {
    async fn register_node(&self) -> Result<(), FlameError> {
        trace_fn!("UnknownState::register_node");

        let node_name = {
            let mut node = lock_ptr!(self.node)?;
            node.state = NodeState::Ready;
            tracing::info!(
                "Node <{}> reconnected, transitioning from Unknown to Ready",
                node.name
            );
            node.name.clone()
        };

        // Persist the state change
        self.storage
            .update_node_state(&node_name, NodeState::Ready)
            .await
    }

    async fn drain(&self) -> Result<(), FlameError> {
        trace_fn!("UnknownState::drain");

        Err(FlameError::InvalidState(
            "Node is already in Unknown state (draining)".to_string(),
        ))
    }

    async fn shutdown(&self) -> Result<(), FlameError> {
        trace_fn!("UnknownState::shutdown");

        let node_name = {
            let mut node = lock_ptr!(self.node)?;
            node.state = NodeState::NotReady;
            tracing::info!(
                "Node <{}> shutdown, transitioning from Unknown to NotReady",
                node.name
            );
            node.name.clone()
        };

        // Clean up all executors on this node
        let executors = self
            .storage
            .list_executors(Some(&ExecutorFilter::by_node(&node_name)))?;

        if !executors.is_empty() {
            tracing::info!(
                "Cleaning up {} executors for node <{}>",
                executors.len(),
                node_name
            );
            self.storage.delete_executors(&executors).await?;
        }

        // Persist the state change
        self.storage
            .update_node_state(&node_name, NodeState::NotReady)
            .await
    }

    async fn update_node(&self, _node: &Node) -> Result<(), FlameError> {
        trace_fn!("UnknownState::update_node");

        Err(FlameError::InvalidState(
            "Cannot update node in Unknown state".to_string(),
        ))
    }

    async fn release_node(&self) -> Result<(), FlameError> {
        trace_fn!("UnknownState::release_node");

        // Allow release from any state
        Ok(())
    }
}

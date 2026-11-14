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

//! Node state machine for managing node lifecycle.
//!
//! State transitions:
//! - Unknown -> Ready (on register/reconnect)
//! - Ready -> Unknown (on drain - cleanup timer starts)
//! - Unknown -> NotReady (on shutdown after cleanup timeout)
//! - NotReady -> Ready (on reconnect after shutdown)

use std::sync::Arc;

use crate::controller::nodes::{
    not_ready::NotReadyState, ready::ReadyState, unknown::UnknownState,
};
use crate::storage::StoragePtr;

use common::apis::{Node, NodePtr, NodeState};
use common::FlameError;
use stdng::{lock_ptr, MutexPtr};

mod not_ready;
mod ready;
mod unknown;

/// Creates a state handler based on the node's current state.
pub fn from(storage: StoragePtr, node_ptr: NodePtr) -> Result<Arc<dyn NodeStates>, FlameError> {
    let node = lock_ptr!(node_ptr)?;
    tracing::debug!("Build state <{:?}> for Node <{}>.", node.state, node.name);

    match node.state {
        NodeState::Unknown => Ok(Arc::new(UnknownState {
            storage,
            node: node_ptr.clone(),
        })),
        NodeState::Ready => Ok(Arc::new(ReadyState {
            storage,
            node: node_ptr.clone(),
        })),
        NodeState::NotReady => Ok(Arc::new(NotReadyState {
            storage,
            node: node_ptr.clone(),
        })),
    }
}

/// Trait defining the operations available for each node state.
///
/// Each state implements this trait, returning errors for invalid operations
/// and performing state transitions for valid ones.
#[async_trait::async_trait]
pub trait NodeStates: Send + Sync + 'static {
    /// Register a node (transition to Ready state).
    /// Valid from: Unknown, NotReady
    async fn register_node(&self) -> Result<(), FlameError>;

    /// Mark node as draining (transition to Unknown state, starts cleanup timer).
    /// Valid from: Ready
    async fn drain(&self) -> Result<(), FlameError>;

    /// Shutdown node after cleanup timeout (transition to NotReady state).
    /// Valid from: Unknown
    async fn shutdown(&self) -> Result<(), FlameError>;

    /// Update node information (heartbeat, resource update).
    /// Valid from: Ready
    async fn update_node(&self, node: &Node) -> Result<(), FlameError>;

    /// Release/unregister a node completely.
    /// Valid from: any state
    async fn release_node(&self) -> Result<(), FlameError>;
}

#[cfg(test)]
mod tests;

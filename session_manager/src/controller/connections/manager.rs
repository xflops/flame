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

//! ConnectionManager manages all node connections in the controller.
//!
//! This is the entry point for connection management, delegating state-specific
//! operations to the appropriate state handlers via the State Pattern.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use common::FlameError;
use stdng::{lock_ptr, new_ptr, MutexPtr};

use super::from;
use crate::model::{
    ConnectionCallbacks, ConnectionState, Executor, NodeConnection, NodeConnectionPtr,
    NodeConnectionReceiver, NodeConnectionSender, DEFAULT_DRAIN_TIMEOUT_SECS,
};

/// Manages all node connections in the controller.
///
/// ConnectionManager uses the State Pattern internally, delegating operations
/// to state-specific handlers. This provides clean separation between:
/// - Connection registry (this struct)
/// - State-specific behavior (states module)
pub struct ConnectionManager<C: ConnectionCallbacks> {
    /// Map of node_name -> NodeConnection (thread-safe)
    connections: MutexPtr<HashMap<String, NodeConnectionPtr>>,
    /// Drain timeout duration
    drain_timeout: Duration,
    /// Callbacks for connection events
    callbacks: Arc<C>,
}

impl<C: ConnectionCallbacks> Clone for ConnectionManager<C> {
    fn clone(&self) -> Self {
        Self {
            connections: self.connections.clone(),
            drain_timeout: self.drain_timeout,
            callbacks: self.callbacks.clone(),
        }
    }
}

impl<C: ConnectionCallbacks> ConnectionManager<C> {
    /// Creates a new ConnectionManager with default drain timeout.
    pub fn new(callbacks: C) -> Self {
        Self::with_timeout(callbacks, Duration::from_secs(DEFAULT_DRAIN_TIMEOUT_SECS))
    }

    /// Creates a new ConnectionManager with custom drain timeout.
    pub fn with_timeout(callbacks: C, drain_timeout: Duration) -> Self {
        ConnectionManager {
            connections: new_ptr(HashMap::new()),
            drain_timeout,
            callbacks: Arc::new(callbacks),
        }
    }

    /// Connects a node (creates connection if not exists, reconnects if draining).
    ///
    /// Called during register_node. Creates NodeConnection if not exists,
    /// then delegates to state machine for connect logic.
    ///
    /// Note: Always calls on_connected callback, including for reconnections from Draining.
    ///
    /// Returns the sender and receiver handles for the connection.
    pub async fn connect(
        &self,
        node_name: &str,
    ) -> Result<(NodeConnectionSender, NodeConnectionReceiver), FlameError> {
        // Get or create connection
        let conn_ptr = {
            let mut connections = lock_ptr!(self.connections)?;

            if let Some(existing) = connections.get(node_name) {
                existing.clone()
            } else {
                // Create new connection in Connected state
                let conn = NodeConnection::new(node_name.to_string());
                let conn_ptr = new_ptr(conn);
                connections.insert(node_name.to_string(), conn_ptr.clone());
                tracing::info!("Node <{}> connection created", node_name);
                conn_ptr
            }
        };

        // Let state machine handle connect (idempotent for Connected state)
        let state_handler = from(conn_ptr.clone())?;
        state_handler.connect().await?;

        // Always notify callback (for both new connections and reconnections)
        self.callbacks.on_connected(node_name).await?;

        // Return sender and receiver handles
        let (sender, receiver) = {
            let conn = lock_ptr!(conn_ptr)?;
            (conn.sender(), conn.receiver())
        };

        Ok((sender, receiver))
    }

    /// Gets the sender and receiver handles for a node's connection.
    ///
    /// Returns None if the node doesn't have a connection.
    pub fn get_channel(
        &self,
        node_name: &str,
    ) -> Result<Option<(NodeConnectionSender, NodeConnectionReceiver)>, FlameError> {
        let connections = lock_ptr!(self.connections)?;

        if let Some(conn_ptr) = connections.get(node_name) {
            let conn = lock_ptr!(conn_ptr)?;
            Ok(Some((conn.sender(), conn.receiver())))
        } else {
            Ok(None)
        }
    }

    /// Drains a node and starts the drain timer.
    ///
    /// Uses the state machine to validate and perform the transition.
    pub async fn drain(&self, node_name: &str) -> Result<bool, FlameError> {
        let conn_ptr = {
            let connections = lock_ptr!(self.connections)?;

            match connections.get(node_name) {
                Some(ptr) => ptr.clone(),
                None => return Ok(false),
            }
        };

        // Use state machine to handle drain
        let state_handler = from(conn_ptr.clone())?;
        if let Err(FlameError::InvalidState(_)) = state_handler.drain().await {
            tracing::debug!("Node <{}> already draining/closed", node_name);
            return Ok(false);
        }

        // Set up drain timer
        self.setup_drain_timer(node_name, conn_ptr.clone())?;

        // Notify callback
        self.callbacks.on_draining(node_name).await?;

        Ok(true)
    }

    /// Sets up the drain timer for a disconnected node.
    fn setup_drain_timer(
        &self,
        node_name: &str,
        conn_ptr: NodeConnectionPtr,
    ) -> Result<(), FlameError> {
        let cancel_token = CancellationToken::new();

        // Store the cancel token in the connection
        {
            let mut conn = lock_ptr!(conn_ptr)?;
            conn.drain_cancel = Some(cancel_token.clone());
        }

        // Spawn the drain timer task
        let timeout = self.drain_timeout;
        let node_name_clone = node_name.to_string();
        let manager = self.clone();
        let conn_ptr_clone = conn_ptr;

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(timeout) => {
                    // Timer expired, trigger shutdown via state machine
                    if let Err(e) = manager.handle_drain_timeout(&node_name_clone, conn_ptr_clone).await {
                        tracing::error!(
                            "Failed to handle drain timeout for node <{}>: {}",
                            node_name_clone,
                            e
                        );
                    }
                }
                _ = cancel_token.cancelled() => {
                    // Timer was cancelled (node reconnected)
                    tracing::debug!("Drain timer cancelled for node <{}>", node_name_clone);
                }
            }
        });

        tracing::info!(
            "Node <{}> disconnected, drain timer started ({:?})",
            node_name,
            timeout
        );

        Ok(())
    }

    /// Handles drain timeout by closing the connection permanently and removing it.
    async fn handle_drain_timeout(
        &self,
        node_name: &str,
        conn_ptr: NodeConnectionPtr,
    ) -> Result<(), FlameError> {
        // Use state machine to close the connection
        let state_handler = from(conn_ptr)?;
        state_handler.close().await?;

        // Notify callback before removing
        self.callbacks.on_closed(node_name).await?;

        // Remove the closed connection from registry
        {
            let mut connections = lock_ptr!(self.connections)?;
            connections.remove(node_name);
            tracing::debug!("Removed closed connection for node <{}>", node_name);
        }

        Ok(())
    }

    /// Gets the connection state of a node.
    pub fn get_state(&self, node_name: &str) -> Option<ConnectionState> {
        let connections = lock_ptr!(self.connections).ok()?;
        let conn_ptr = connections.get(node_name)?;
        let conn = lock_ptr!(conn_ptr).ok()?;
        Some(conn.state.clone())
    }

    /// Checks if a node is currently connected.
    pub fn is_connected(&self, node_name: &str) -> bool {
        self.get_state(node_name) == Some(ConnectionState::Connected)
    }

    /// Notifies a node about an executor update by pushing to its queue.
    pub async fn notify_executor(
        &self,
        node_name: &str,
        executor: &Executor,
    ) -> Result<(), FlameError> {
        let conn_ptr = {
            let connections = lock_ptr!(self.connections)?;

            connections.get(node_name).cloned().ok_or_else(|| {
                FlameError::NotFound(format!("No connection for node <{}>", node_name))
            })?
        };

        // Use state machine to notify executor
        let state_handler = from(conn_ptr)?;
        state_handler.notify_executor(executor).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestCallbacks {
        connected_count: AtomicUsize,
        draining_count: AtomicUsize,
        closed_count: AtomicUsize,
    }

    impl TestCallbacks {
        fn new() -> Self {
            Self {
                connected_count: AtomicUsize::new(0),
                draining_count: AtomicUsize::new(0),
                closed_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ConnectionCallbacks for TestCallbacks {
        async fn on_connected(&self, _node_name: &str) -> Result<(), FlameError> {
            self.connected_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn on_draining(&self, _node_name: &str) -> Result<(), FlameError> {
            self.draining_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn on_closed(&self, _node_name: &str) -> Result<(), FlameError> {
            self.closed_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_connect_disconnect() {
        let callbacks = TestCallbacks::new();
        let manager = ConnectionManager::with_timeout(callbacks, Duration::from_millis(100));

        // Connect (creates in Connected state, returns sender and receiver)
        let (_sender, _receiver) = manager.connect("node1").await.unwrap();
        assert!(manager.is_connected("node1"));

        // Disconnect
        let result = manager.drain("node1").await.unwrap();
        assert!(result);
        assert!(!manager.is_connected("node1"));
        assert_eq!(manager.get_state("node1"), Some(ConnectionState::Draining));

        // Wait for timeout - connection will be removed after close
        tokio::time::sleep(Duration::from_millis(150)).await;
        // After timeout, connection is removed from registry
        assert_eq!(manager.get_state("node1"), None);
    }

    #[tokio::test]
    async fn test_reconnect_before_timeout() {
        let callbacks = TestCallbacks::new();
        let manager = ConnectionManager::with_timeout(callbacks, Duration::from_secs(10));

        // Connect (Connected state)
        manager.connect("node1").await.unwrap();

        // Drain
        manager.drain("node1").await.unwrap();
        assert_eq!(manager.get_state("node1"), Some(ConnectionState::Draining));

        // Reconnect before timeout (connect from Draining state)
        manager.connect("node1").await.unwrap();
        assert!(manager.is_connected("node1"));
    }

    #[tokio::test]
    async fn test_reconnect_after_shutdown() {
        let callbacks = TestCallbacks::new();
        let manager = ConnectionManager::with_timeout(callbacks, Duration::from_millis(50));

        // Connect, drain, and wait for shutdown
        manager.connect("node1").await.unwrap();
        manager.drain("node1").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // After timeout, connection is removed from registry
        assert_eq!(manager.get_state("node1"), None);

        // Reconnect after shutdown - connect creates new connection
        manager.connect("node1").await.unwrap();
        assert!(manager.is_connected("node1"));
    }
}

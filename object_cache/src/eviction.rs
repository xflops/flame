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

//! Eviction policy module for ObjectCache.
//!
//! This module provides an extensible eviction policy interface (`EvictionPolicy` trait)
//! and implementations including LRU (Least Recently Used) as the default policy.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use bytesize::ByteSize;
use common::ctx::parse_memory_size;
use serde_derive::Deserialize;
use stdng::{lock_ptr, new_ptr, MutexPtr};

const DEFAULT_MAX_MEMORY: &str = "1G";
const DEFAULT_EVICTION_POLICY: &str = "lru";

/// Configuration for eviction policy
#[derive(Debug, Clone, Deserialize, Default)]
pub struct EvictionConfig {
    /// Eviction policy: "lru" or "none"
    pub policy: Option<String>,
    /// Maximum memory for cached objects (string with units: "1G", "512M")
    pub max_memory: Option<String>,
    /// Maximum number of objects in memory
    pub max_objects: Option<usize>,
}

impl EvictionConfig {
    pub fn policy_name(&self) -> &str {
        self.policy.as_deref().unwrap_or(DEFAULT_EVICTION_POLICY)
    }

    pub fn max_memory_bytes(&self) -> u64 {
        let max_memory_str = self.max_memory.as_deref().unwrap_or(DEFAULT_MAX_MEMORY);
        parse_memory_size(max_memory_str).unwrap_or(1024 * 1024 * 1024)
    }
}

/// Eviction policy trait - defines the interface for cache eviction strategies.
///
/// Implementations must be thread-safe (Send + Sync).
pub trait EvictionPolicy: Send + Sync {
    /// Record an access to an object (updates LRU order).
    fn on_access(&self, key: &str);

    /// Select objects to evict, returns keys in eviction order (least recently used first).
    /// Returns empty vec if no eviction is needed.
    fn victims(&self, count: usize) -> Vec<String>;

    /// Called when an object is evicted from memory.
    fn on_evict(&self, key: &str);

    /// Called when an object is added to the cache.
    fn on_add(&self, key: &str, size: u64);

    /// Called when an object is removed (deleted, not evicted).
    fn on_remove(&self, key: &str);
}

/// Type alias for boxed eviction policy.
pub type EvictionPolicyPtr = Box<dyn EvictionPolicy>;

/// Node in the doubly-linked list for LRU tracking.
#[derive(Debug, Clone)]
struct LRUNode {
    #[allow(dead_code)] // Used for debugging
    key: String,
    size: u64,
    prev: Option<String>,
    next: Option<String>,
}

/// LRU eviction policy implementation.
///
/// Uses a doubly-linked list + HashMap for O(1) operations:
/// - Access recording: O(1)
/// - Eviction selection: O(1) per item
/// - Memory tracking: O(1) using atomic operations
pub struct LRUPolicy {
    /// Maximum memory in bytes
    max_memory: u64,
    /// Maximum number of objects (optional)
    max_objects: Option<usize>,
    /// LRU nodes indexed by key
    nodes: MutexPtr<HashMap<String, LRUNode>>,
    /// Head of the list (least recently used)
    head: MutexPtr<Option<String>>,
    /// Tail of the list (most recently used)
    tail: MutexPtr<Option<String>>,
    /// Current memory usage in bytes
    current_memory: AtomicU64,
    /// Current object count
    current_count: AtomicUsize,
}

impl LRUPolicy {
    /// Create a new LRU policy with the given configuration.
    pub fn new(config: &EvictionConfig) -> Self {
        let max_memory = config.max_memory_bytes();
        let max_objects = config.max_objects;

        tracing::info!(
            "Creating LRU eviction policy: max_memory={}, max_objects={:?}",
            ByteSize::b(max_memory),
            max_objects
        );

        Self {
            max_memory,
            max_objects,
            nodes: new_ptr(HashMap::new()),
            head: new_ptr(None),
            tail: new_ptr(None),
            current_memory: AtomicU64::new(0),
            current_count: AtomicUsize::new(0),
        }
    }

    /// Get current memory usage in bytes (for testing/monitoring).
    #[cfg(test)]
    pub fn current_memory(&self) -> u64 {
        self.current_memory.load(Ordering::Relaxed)
    }

    /// Get current object count (for testing/monitoring).
    #[cfg(test)]
    pub fn current_count(&self) -> usize {
        self.current_count.load(Ordering::Relaxed)
    }

    /// Check if eviction is needed based on current state.
    fn should_evict(&self) -> bool {
        let mem = self.current_memory.load(Ordering::Relaxed);
        let count = self.current_count.load(Ordering::Relaxed);

        let memory_exceeded = mem > self.max_memory;
        let count_exceeded = self.max_objects.is_some_and(|max| count > max);

        if memory_exceeded || count_exceeded {
            tracing::debug!(
                "LRU: eviction needed - memory={}/{}, count={}/{:?}",
                ByteSize::b(mem),
                ByteSize::b(self.max_memory),
                count,
                self.max_objects
            );
        }

        memory_exceeded || count_exceeded
    }

    /// Remove a node from the linked list (internal helper).
    fn remove_from_list(
        nodes: &mut HashMap<String, LRUNode>,
        head: &mut Option<String>,
        tail: &mut Option<String>,
        key: &str,
    ) {
        if let Some(node) = nodes.get(key).cloned() {
            // Update prev node's next pointer
            if let Some(ref prev_key) = node.prev {
                if let Some(prev_node) = nodes.get_mut(prev_key) {
                    prev_node.next = node.next.clone();
                }
            } else {
                // This was the head
                *head = node.next.clone();
            }

            // Update next node's prev pointer
            if let Some(ref next_key) = node.next {
                if let Some(next_node) = nodes.get_mut(next_key) {
                    next_node.prev = node.prev.clone();
                }
            } else {
                // This was the tail
                *tail = node.prev.clone();
            }

            // Clear the node's pointers
            if let Some(n) = nodes.get_mut(key) {
                n.prev = None;
                n.next = None;
            }
        }
    }

    /// Add a node to the tail of the list (most recently used).
    fn add_to_tail(
        nodes: &mut HashMap<String, LRUNode>,
        head: &mut Option<String>,
        tail: &mut Option<String>,
        key: &str,
    ) {
        if let Some(node) = nodes.get_mut(key) {
            node.prev = tail.clone();
            node.next = None;

            if let Some(ref tail_key) = tail {
                if let Some(tail_node) = nodes.get_mut(tail_key) {
                    tail_node.next = Some(key.to_string());
                }
            }

            *tail = Some(key.to_string());

            if head.is_none() {
                *head = Some(key.to_string());
            }
        }
    }
}

impl EvictionPolicy for LRUPolicy {
    fn on_access(&self, key: &str) {
        let mut nodes = match lock_ptr!(self.nodes) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Failed to lock nodes: {}", e);
                return;
            }
        };
        let mut head = match lock_ptr!(self.head) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to lock head: {}", e);
                return;
            }
        };
        let mut tail = match lock_ptr!(self.tail) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to lock tail: {}", e);
                return;
            }
        };

        if nodes.contains_key(key) {
            // Move to tail (most recently used)
            Self::remove_from_list(&mut nodes, &mut head, &mut tail, key);
            Self::add_to_tail(&mut nodes, &mut head, &mut tail, key);
            tracing::trace!("LRU: recorded access for key={}", key);
        }
    }

    fn victims(&self, count: usize) -> Vec<String> {
        // Return empty if no eviction needed
        if !self.should_evict() {
            return Vec::new();
        }

        let nodes = match lock_ptr!(self.nodes) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Failed to lock nodes: {}", e);
                return Vec::new();
            }
        };
        let head = match lock_ptr!(self.head) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to lock head: {}", e);
                return Vec::new();
            }
        };

        let mut keys = Vec::with_capacity(count);
        let mut current = head.clone();

        while keys.len() < count {
            match current {
                Some(ref key) => {
                    keys.push(key.clone());
                    current = nodes.get(key).and_then(|n| n.next.clone());
                }
                None => break,
            }
        }

        tracing::debug!("LRU: selected {} keys for eviction", keys.len());
        keys
    }

    fn on_evict(&self, key: &str) {
        let mut nodes = match lock_ptr!(self.nodes) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Failed to lock nodes: {}", e);
                return;
            }
        };
        let mut head = match lock_ptr!(self.head) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to lock head: {}", e);
                return;
            }
        };
        let mut tail = match lock_ptr!(self.tail) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to lock tail: {}", e);
                return;
            }
        };

        if let Some(node) = nodes.get(key) {
            let size = node.size;
            Self::remove_from_list(&mut nodes, &mut head, &mut tail, key);
            nodes.remove(key);
            self.current_memory.fetch_sub(size, Ordering::Relaxed);
            self.current_count.fetch_sub(1, Ordering::Relaxed);
            tracing::debug!("LRU: evicted key={}, size={}", key, ByteSize::b(size));
        }
    }

    fn on_add(&self, key: &str, size: u64) {
        let mut nodes = match lock_ptr!(self.nodes) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Failed to lock nodes: {}", e);
                return;
            }
        };
        let mut head = match lock_ptr!(self.head) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to lock head: {}", e);
                return;
            }
        };
        let mut tail = match lock_ptr!(self.tail) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to lock tail: {}", e);
                return;
            }
        };

        // If key already exists, update it
        if let Some(existing) = nodes.get(key) {
            let old_size = existing.size;
            Self::remove_from_list(&mut nodes, &mut head, &mut tail, key);
            self.current_memory.fetch_sub(old_size, Ordering::Relaxed);
            self.current_count.fetch_sub(1, Ordering::Relaxed);
        }

        // Add new node
        let node = LRUNode {
            key: key.to_string(),
            size,
            prev: None,
            next: None,
        };
        nodes.insert(key.to_string(), node);
        Self::add_to_tail(&mut nodes, &mut head, &mut tail, key);

        self.current_memory.fetch_add(size, Ordering::Relaxed);
        self.current_count.fetch_add(1, Ordering::Relaxed);

        tracing::debug!(
            "LRU: added key={}, size={}, total_memory={}, total_count={}",
            key,
            ByteSize::b(size),
            ByteSize::b(self.current_memory.load(Ordering::Relaxed)),
            self.current_count.load(Ordering::Relaxed)
        );
    }

    fn on_remove(&self, key: &str) {
        // Same as on_evict - remove from tracking
        self.on_evict(key);
    }
}

/// No-op eviction policy for backward compatibility or testing.
///
/// This policy never evicts anything - all objects remain in memory indefinitely.
pub struct NoEvictionPolicy;

impl NoEvictionPolicy {
    pub fn new() -> Self {
        tracing::info!("Creating NoEviction policy - objects will not be evicted");
        Self
    }
}

impl Default for NoEvictionPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl EvictionPolicy for NoEvictionPolicy {
    fn on_access(&self, _key: &str) {
        // No-op
    }

    fn victims(&self, _count: usize) -> Vec<String> {
        Vec::new()
    }

    fn on_evict(&self, _key: &str) {
        // No-op
    }

    fn on_add(&self, _key: &str, _size: u64) {
        // No-op
    }

    fn on_remove(&self, _key: &str) {
        // No-op
    }
}

/// Create an eviction policy based on configuration.
pub fn new_policy(config: Option<&EvictionConfig>) -> EvictionPolicyPtr {
    let config = config.cloned().unwrap_or_default();

    // Check environment variable overrides
    let policy_name = std::env::var("FLAME_CACHE_EVICTION_POLICY")
        .ok()
        .unwrap_or_else(|| config.policy_name().to_string());

    let mut effective_config = config.clone();

    if let Ok(max_mem) = std::env::var("FLAME_CACHE_MAX_MEMORY") {
        effective_config.max_memory = Some(max_mem);
    }

    if let Ok(max_obj) = std::env::var("FLAME_CACHE_MAX_OBJECTS") {
        if let Ok(val) = max_obj.parse::<usize>() {
            effective_config.max_objects = Some(val);
        }
    }

    match policy_name.as_str() {
        "none" => Box::new(NoEvictionPolicy::new()),
        _ => Box::new(LRUPolicy::new(&effective_config)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_policy_basic() {
        let config = EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("1M".to_string()), // 1MB
            max_objects: None,
        };
        let policy = LRUPolicy::new(&config);

        // Add objects
        policy.on_add("key1", 100);
        policy.on_add("key2", 200);
        policy.on_add("key3", 300);

        assert_eq!(policy.current_count(), 3);
        assert_eq!(policy.current_memory(), 600);
        assert!(policy.victims(1).is_empty()); // Under 1MB limit, no victims
    }

    #[test]
    fn test_lru_policy_eviction_trigger() {
        let config = EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("1M".to_string()), // 1MB = 1048576 bytes
            max_objects: None,
        };
        let policy = LRUPolicy::new(&config);

        // Add object that exceeds limit
        policy.on_add("key1", 1048577); // Just over 1MB

        assert!(!policy.victims(1).is_empty()); // Should have victims
    }

    #[test]
    fn test_lru_policy_access_order() {
        let config = EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("100".to_string()), // Very low limit to trigger eviction
            max_objects: None,
        };
        let policy = LRUPolicy::new(&config);

        // Add objects in order
        policy.on_add("key1", 100);
        policy.on_add("key2", 100);
        policy.on_add("key3", 100);

        // Access key1, making it most recently used
        policy.on_access("key1");

        // Select for eviction - should get key2 first (least recently used)
        let to_evict = policy.victims(2);
        assert_eq!(to_evict.len(), 2);
        assert_eq!(to_evict[0], "key2");
        assert_eq!(to_evict[1], "key3");
    }

    #[test]
    fn test_lru_policy_evict() {
        let config = EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("100M".to_string()),
            max_objects: None,
        };
        let policy = LRUPolicy::new(&config);

        policy.on_add("key1", 100);
        policy.on_add("key2", 200);

        assert_eq!(policy.current_count(), 2);
        assert_eq!(policy.current_memory(), 300);

        policy.on_evict("key1");

        assert_eq!(policy.current_count(), 1);
        assert_eq!(policy.current_memory(), 200);
    }

    #[test]
    fn test_lru_policy_max_objects() {
        let config = EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("1000M".to_string()), // High memory limit
            max_objects: Some(2),                  // Low object limit
        };
        let policy = LRUPolicy::new(&config);

        policy.on_add("key1", 100);
        policy.on_add("key2", 100);
        assert!(policy.victims(1).is_empty()); // No victims yet

        policy.on_add("key3", 100);
        assert!(!policy.victims(1).is_empty()); // Exceeds max_objects, has victims
    }

    #[test]
    fn test_no_eviction_policy() {
        let policy = NoEvictionPolicy::new();

        policy.on_add("key1", 1_000_000_000); // 1GB
        assert!(policy.victims(1).is_empty()); // Never has victims - NoEviction never evicts
    }

    #[test]
    fn test_new_policy_lru() {
        let config = EvictionConfig {
            policy: Some("lru".to_string()),
            max_memory: Some("512M".to_string()),
            max_objects: None,
        };
        let policy = new_policy(Some(&config));

        // Verify it's an LRU policy by checking it tracks objects
        policy.on_add("test", 100);
        // Under limit, no victims
        assert!(policy.victims(1).is_empty());
    }

    #[test]
    fn test_new_policy_none() {
        let config = EvictionConfig {
            policy: Some("none".to_string()),
            max_memory: None,
            max_objects: None,
        };
        let policy = new_policy(Some(&config));

        // Verify it's a NoEviction policy - never returns victims
        policy.on_add("test", 100);
        assert!(policy.victims(1).is_empty());
    }
}

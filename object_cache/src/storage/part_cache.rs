/*
Copyright 2026 The Flame Authors.
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

//! Byte-bounded LRU of verified immutable parts, shared by storage readers.

use super::PartReader;
use crate::model::{Chunk, Part};
use async_trait::async_trait;
use bytes::Bytes;
use common::{ctx::FlameCachePool, FlameError};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct PartKey {
    object_id: String,
    part: u64,
}

#[derive(Default)]
struct State {
    pool: FlameCachePool,
    entries: HashMap<String, HashMap<u64, CachedPart>>,
    order: BTreeMap<u64, PartKey>,
    clock: u64,
    used: u64,
}

impl State {
    /// Remove the exact previous clock and reinsert the part as most recently used.
    fn get_patch(&mut self, object_id: &str, part: u64) -> Option<CachedPart> {
        if self.pool.size == 0 {
            return None;
        }
        let old_clock = self.entries.get(object_id)?.get(&part)?.clock;
        self.order.remove(&old_clock);
        let clock = self.next_clock();
        let cached = self.entries.get_mut(object_id)?.get_mut(&part)?;
        cached.clock = clock;
        let data = cached.clone();
        self.order.insert(
            clock,
            PartKey {
                object_id: cached.object_id.clone(),
                part: cached.part.part,
            },
        );
        Some(data)
    }

    /// Create readers for every cached part of an object, sorted by logical part ID.
    fn patch_readers(&self, object_id: &str) -> Vec<MemoryReader> {
        let Some(parts) = self.entries.get(object_id) else {
            return Vec::new();
        };
        let mut cached: Vec<_> = parts.values().cloned().collect();
        cached.sort_unstable_by_key(|cached| cached.part.part);
        cached
            .into_iter()
            .map(|cached| MemoryReader {
                cached,
                position: 0,
                offset: 0,
            })
            .collect()
    }

    /// Collect only parts that can fit as complete entries; empty parts need no payload cache.
    fn cacheable(&self, size: u64) -> bool {
        size > 0 && size <= self.pool.size
    }

    /// Insert a complete verified part, evicting least recently used entries only when it is ready.
    fn insert_patch(&mut self, object_id: &str, part: Part, chunks: Vec<Bytes>) {
        let size = chunks.iter().map(|chunk| chunk.len() as u64).sum();
        if size != part.size || !self.cacheable(size) {
            return;
        }
        if self
            .entries
            .get(object_id)
            .is_some_and(|parts| parts.contains_key(&part.part))
        {
            return;
        }
        while self.used > self.pool.size - size {
            let oldest = self.pop();
            self.used -= oldest.part.size;
        }
        let clock = self.next_clock();
        let entry = CachedPart {
            chunks: Arc::new(chunks),
            part,
            object_id: object_id.to_owned(),
            clock,
        };
        self.order.insert(
            clock,
            PartKey {
                object_id: entry.object_id.clone(),
                part: entry.part.part,
            },
        );
        self.entries
            .entry(object_id.to_owned())
            .or_default()
            .insert(part.part, entry);
        self.used += size;
    }

    /// Remove collected payloads and their exact ordering clocks immediately.
    fn remove_object(&mut self, object_id: &str) {
        if let Some(parts) = self.entries.remove(object_id) {
            for cached in parts.into_values() {
                self.order.remove(&cached.clock);
                self.used -= cached.part.size;
            }
        }
    }

    /// Allocate an access sequence, preserving LRU order if the sequence wraps.
    fn next_clock(&mut self) -> u64 {
        if self.clock == u64::MAX {
            let order = std::mem::take(&mut self.order);
            for (clock, (_, key)) in order.into_iter().enumerate() {
                let clock = clock as u64;
                self.entries
                    .get_mut(&key.object_id)
                    .and_then(|parts| parts.get_mut(&key.part))
                    .expect("resident LRU entry")
                    .clock = clock;
                self.order.insert(clock, key);
            }
            self.clock = self.order.len() as u64;
        }
        let clock = self.clock;
        self.clock += 1;
        clock
    }

    /// Remove the oldest order key and its resident payload without stale-key checks.
    fn pop(&mut self) -> CachedPart {
        let (_, key) = self.order.pop_first().expect("resident LRU key");
        let parts = self
            .entries
            .get_mut(&key.object_id)
            .expect("resident object");
        let cached = parts.remove(&key.part).expect("resident LRU entry");
        if parts.is_empty() {
            self.entries.remove(&key.object_id);
        }
        cached
    }
}

/// Retain verified filesystem chunks without concatenating or copying their payloads.
#[derive(Clone)]
struct CachedPart {
    part: Part,
    object_id: String,
    clock: u64,
    chunks: Arc<Vec<Bytes>>,
}

/// Serve a verified memory part using the same bounded, part-relative chunk framing as disk.
#[derive(Clone)]
pub(super) struct MemoryReader {
    cached: CachedPart,
    position: usize,
    offset: u64,
}

#[async_trait]
impl PartReader for MemoryReader {
    /// Return the immutable descriptor from the validated catalog snapshot.
    fn part(&self) -> Part {
        self.cached.part
    }

    /// Clone the next verified chunk without copying its payload or repeating disk checksums.
    async fn next_chunk(&mut self) -> Result<Option<Chunk>, FlameError> {
        let Some(bytes) = self.cached.chunks.get(self.position) else {
            return Ok(None);
        };
        let chunk = Chunk {
            part: self.cached.part.part,
            offset: self.offset,
            data: bytes.clone(),
        };
        self.position += 1;
        self.offset += bytes.len() as u64;
        Ok(Some(chunk))
    }
}

/// Cache committed parts by incarnation and logical ID within a resident payload budget.
pub(super) struct PartCache {
    state: Mutex<State>,
}

impl PartCache {
    /// Create an empty cache; zero capacity disables caching.
    pub(super) fn new(pool: FlameCachePool) -> Self {
        Self {
            state: Mutex::new(State {
                pool,
                ..State::default()
            }),
        }
    }

    /// Lock state and look up cached chunks while refreshing their recency.
    pub(super) fn get_patch(&self, object_id: &str, part: u64) -> Option<Arc<Vec<Bytes>>> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_patch(object_id, part)
            .map(|cached| cached.chunks)
    }

    /// Lock state and create cached part readers, omitting filesystem misses.
    pub(super) fn patch_readers(&self, object_id: &str) -> Vec<MemoryReader> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .patch_readers(object_id)
    }

    /// Lock state and check whether a complete part fits in the pool.
    pub(super) fn cacheable(&self, size: u64) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cacheable(size)
    }

    /// Lock state and insert a complete verified part.
    pub(super) fn insert_patch(&self, object_id: &str, part: Part, chunks: Vec<Bytes>) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert_patch(object_id, part, chunks);
    }

    /// Lock state and invalidate a collected object incarnation.
    pub(super) fn remove_object(&self, object_id: &str) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove_object(object_id);
    }

    /// Inspect payload bytes currently owned by the LRU in cache tests.
    #[cfg(test)]
    pub(super) fn used(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .used
    }

    /// Observe the one-key-per-part invariant after hits, evictions, and invalidations.
    #[cfg(test)]
    pub(super) fn order_len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .order
            .len()
    }
}

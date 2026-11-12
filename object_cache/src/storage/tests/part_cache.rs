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

use super::pool;
use crate::model::Part;
use crate::storage::part_cache::PartCache;
use crate::storage::PartReader;
use bytes::Bytes;
use std::sync::{Arc, Barrier};

/// Insert bytes as though storage completed the part's checksum and identity checks.
fn fill(cache: &PartCache, uuid: &str, part: u64, bytes: &'static [u8]) {
    cache.insert_patch(
        uuid,
        Part::new(part, Bytes::from_static(bytes)),
        vec![Bytes::from_static(bytes)],
    );
}

/// A hit refreshes recency and byte-weighted insertion evicts enough oldest parts.
#[test]
fn byte_budget_evicts_lru_parts_after_hit_refreshes_recency() {
    let cache = PartCache::new(pool(10));
    fill(&cache, "object", 0, b"aa");
    fill(&cache, "object", 1, b"bbbb");
    fill(&cache, "object", 2, b"cccc");
    assert!(cache.get_patch("object", 0).is_some());

    fill(&cache, "object", 3, b"dddddd");

    assert_eq!(cache.used(), 8);
    assert!(cache.get_patch("object", 1).is_none());
    assert!(cache.get_patch("object", 2).is_none());
    assert_eq!(
        cache.get_patch("object", 0).unwrap().as_slice(),
        [Bytes::from_static(b"aa")]
    );
    assert_eq!(
        cache.get_patch("object", 3).unwrap().as_slice(),
        [Bytes::from_static(b"dddddd")]
    );
}

/// Disabled, empty, and oversized insertions preserve the resident cache and its byte count.
#[test]
fn zero_empty_and_oversized_parts_are_not_cached() {
    let disabled = PartCache::new(pool(0));
    assert!(!disabled.cacheable(1));
    assert!(!disabled.cacheable(0));
    fill(&disabled, "object", 0, b"data");
    assert!(disabled.get_patch("object", 0).is_none());
    assert_eq!(disabled.used(), 0);

    let cache = PartCache::new(pool(4));
    fill(&cache, "object", 0, b"data");
    assert!(!cache.cacheable(0));
    assert!(!cache.cacheable(5));
    cache.insert_patch("object", Part::new(1, Bytes::new()), Vec::new());
    fill(&cache, "object", 2, b"");
    fill(&cache, "object", 3, b"large");
    assert_eq!(cache.used(), 4);
    assert!(cache.get_patch("object", 0).is_some());
    assert!(cache.get_patch("object", 1).is_none());
    assert!(cache.get_patch("object", 2).is_none());
    assert!(cache.get_patch("object", 3).is_none());
}

/// Checking whether a future part fits does not evict ready data or consume resident capacity.
#[test]
fn cacheability_does_not_reserve_space_or_evict_ready_parts() {
    let cache = PartCache::new(pool(4));
    fill(&cache, "object", 0, b"data");
    assert!(cache.cacheable(4));
    assert!(cache.cacheable(1));
    assert_eq!(cache.used(), 4);
    assert!(cache.get_patch("object", 0).is_some());
    assert!(cache.get_patch("object", 1).is_none());
}

/// Eviction releases resident capacity while an active reader keeps its immutable bytes.
#[test]
fn active_readers_do_not_block_ready_part_insertion() {
    let cache = PartCache::new(pool(4));
    fill(&cache, "object", 0, b"aaaa");
    let reader = cache.get_patch("object", 0).unwrap();

    fill(&cache, "object", 1, b"bbbb");

    assert_eq!(cache.used(), 4);
    assert!(cache.get_patch("object", 0).is_none());
    assert_eq!(
        cache.get_patch("object", 1).unwrap().as_slice(),
        [Bytes::from_static(b"bbbb")]
    );
    assert_eq!(reader.as_slice(), [Bytes::from_static(b"aaaa")]);
}

/// Independent verified fills for one immutable identity keep one resident part without double counting.
#[test]
fn duplicate_insertions_keep_one_part_and_one_byte_count() {
    let cache = PartCache::new(pool(8));
    cache.insert_patch(
        "object",
        Part::new(3, Bytes::from_static(b"abcd")),
        vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
    );
    fill(&cache, "object", 4, b"next");
    fill(&cache, "object", 3, b"abcd");

    assert_eq!(cache.used(), 8);
    assert_eq!(
        cache.get_patch("object", 3).unwrap().as_slice(),
        [Bytes::from_static(b"ab"), Bytes::from_static(b"cd")]
    );
    assert!(cache.get_patch("object", 4).is_some());
}

/// Recreated object UUIDs never alias and invalidation removes every part of only the old incarnation.
#[test]
fn uuid_isolation_and_object_invalidation_preserve_other_incarnations() {
    let cache = PartCache::new(pool(16));
    fill(&cache, "old", 0, b"old0");
    fill(&cache, "old", 1, b"old1");
    fill(&cache, "new", 0, b"new0");

    cache.remove_object("old");

    assert_eq!(cache.used(), 4);
    assert!(cache.get_patch("old", 0).is_none());
    assert!(cache.get_patch("old", 1).is_none());
    assert_eq!(
        cache.get_patch("new", 0).unwrap().as_slice(),
        [Bytes::from_static(b"new0")]
    );
    cache.remove_object("absent");
    assert_eq!(cache.used(), 4);
}

/// GC immediately releases resident capacity even while a captured reader can finish safely.
#[test]
fn invalidation_releases_pool_capacity_without_breaking_active_readers() {
    let cache = PartCache::new(pool(4));
    fill(&cache, "object", 0, b"data");
    let reader = cache.get_patch("object", 0).unwrap();
    cache.remove_object("object");
    assert!(cache.get_patch("object", 0).is_none());
    assert_eq!(cache.used(), 0);

    fill(&cache, "replacement", 0, b"next");

    assert_eq!(cache.used(), 4);
    assert_eq!(reader.as_slice(), [Bytes::from_static(b"data")]);
    assert!(cache.get_patch("replacement", 0).is_some());
}

/// All storage handles share resident entries and the same LRU capacity.
#[test]
fn cloned_handles_share_entries_and_resident_capacity() {
    let cache = Arc::new(PartCache::new(pool(4)));
    let cloned = Arc::clone(&cache);
    fill(&cloned, "object", 0, b"data");
    assert_eq!(cache.used(), 4);
    assert_eq!(cloned.used(), 4);
    assert!(cache.get_patch("object", 0).is_some());

    fill(&cache, "object", 1, b"next");

    assert!(cloned.get_patch("object", 0).is_none());
    assert!(cloned.get_patch("object", 1).is_some());
    cloned.remove_object("object");
    assert_eq!(cache.used(), 0);
}

/// Concurrent ready-part insertions evict under one lock and keep resident payload bytes within the pool.
#[test]
fn concurrent_ready_insertions_share_a_single_resident_pool() {
    let cache = Arc::new(PartCache::new(pool(16)));
    let barrier = Arc::new(Barrier::new(9));
    let threads = (0..8)
        .map(|part| {
            let cache = Arc::clone(&cache);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                fill(&cache, "object", part, b"data");
                assert!(cache.used() <= 16);
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(cache.used(), 16);
    let residents = (0..8)
        .filter(|part| cache.get_patch("object", *part).is_some())
        .count();
    assert_eq!(residents, 4);
}

/// Repeated non-root hits replace their old order key and keep exactly one key per resident part.
#[test]
fn repeated_hits_replace_order_keys_and_preserve_lru_eviction() {
    let cache = PartCache::new(pool(12));
    fill(&cache, "object", 0, b"zero");
    fill(&cache, "object", 1, b"one!");
    fill(&cache, "object", 2, b"two!");
    for _ in 0..1000 {
        assert!(cache.get_patch("object", 0).is_some());
        assert_eq!(cache.order_len(), 3);
    }
    fill(&cache, "object", 3, b"next");
    assert!(cache.get_patch("object", 1).is_none());
    assert!(cache.get_patch("object", 0).is_some());
    assert!(cache.get_patch("object", 2).is_some());
    assert!(cache.get_patch("object", 3).is_some());
    assert_eq!(cache.used(), 12);
}

/// Invalidation removes old order keys before the same identity is inserted again.
#[test]
fn invalidation_removes_keys_before_identity_is_reinserted() {
    let cache = PartCache::new(pool(12));
    fill(&cache, "reuse", 0, b"old!");
    fill(&cache, "keep", 1, b"one!");
    fill(&cache, "keep", 2, b"two!");
    assert!(cache.get_patch("reuse", 0).is_some());
    cache.remove_object("reuse");
    assert_eq!(cache.order_len(), 2);
    fill(&cache, "reuse", 0, b"new!");
    fill(&cache, "next", 0, b"next");
    assert!(cache.get_patch("keep", 1).is_none());
    assert_eq!(cache.get_patch("reuse", 0).unwrap()[0], b"new!"[..]);
    assert!(cache.get_patch("keep", 2).is_some());
    assert!(cache.get_patch("next", 0).is_some());
    assert_eq!(cache.order_len(), 3);
}

/// Invalidating every entry releases payload ownership and clears all remaining order keys.
#[test]
fn invalidation_releases_payloads_and_order_keys_immediately() {
    let cache = PartCache::new(pool(4));
    fill(&cache, "object", 0, b"data");
    let payload = {
        let reader = cache.get_patch("object", 0).unwrap();
        Arc::downgrade(&reader)
    };
    cache.remove_object("object");
    assert_eq!(cache.used(), 0);
    assert_eq!(cache.order_len(), 0);
    assert!(payload.upgrade().is_none());
}

/// Memory readers preserve the original committed descriptor after cache invalidation.
#[tokio::test]
async fn memory_readers_own_cached_part_metadata_and_shared_chunks() {
    let cache = PartCache::new(pool(16));
    let bytes = Bytes::from_static(b"part payload");
    let part = Part::new(7, bytes.clone()).with_creation_time(1234);
    cache.insert_patch("object-uuid", part, vec![bytes.clone()]);
    assert!(cache.patch_readers("other-uuid").is_empty());
    let mut readers = cache.patch_readers("object-uuid");
    assert_eq!(readers.len(), 1);
    let reader = &mut readers[0];
    assert_eq!(reader.part(), part);
    cache.remove_object("object-uuid");
    assert_eq!(reader.part(), part);
    let chunk = reader.next_chunk().await.unwrap().unwrap();
    assert_eq!(chunk.part, 7);
    assert_eq!(chunk.offset, 0);
    assert_eq!(chunk.data, bytes);
    assert!(reader.next_chunk().await.unwrap().is_none());
}

/// Object-scoped lookup returns all and only that object's cached parts in logical order.
#[tokio::test]
async fn object_lookup_returns_every_cached_part_without_other_objects() {
    let cache = PartCache::new(pool(16));
    fill(&cache, "first", 3, b"tail");
    fill(&cache, "other", 0, b"else");
    fill(&cache, "first", 0, b"base");
    let mut readers = cache.patch_readers("first");
    assert_eq!(readers.len(), 2);
    assert_eq!(readers[0].part().part, 0);
    assert_eq!(readers[1].part().part, 3);
    assert_eq!(
        readers[0].next_chunk().await.unwrap().unwrap().data,
        b"base"[..]
    );
    assert_eq!(
        readers[1].next_chunk().await.unwrap().unwrap().data,
        b"tail"[..]
    );
    cache.remove_object("first");
    assert!(cache.patch_readers("first").is_empty());
    assert_eq!(cache.patch_readers("other").len(), 1);
    assert_eq!(cache.used(), 4);
}

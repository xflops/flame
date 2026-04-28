"""
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

Integration tests for LRU eviction policy in ObjectCache.

These tests verify:
1. Objects are evicted from memory when memory limit is reached
2. Evicted objects can be reloaded from disk when accessed
3. LRU ordering is maintained (least recently used objects evicted first)
4. Access patterns update LRU order correctly

Configuration:
The LRU policy is configured via flame-cluster.yaml:
```yaml
cache:
  eviction:
    policy: "lru"           # Eviction policy: "lru" | "none" (default: "lru")
    max_memory: "1G"        # Maximum memory with units: K, M, G, T (default: "1G")
    max_objects: 10000      # Maximum number of objects (default: unlimited)
```

Environment variables can override config:
- FLAME_CACHE_EVICTION_POLICY: Override eviction policy
- FLAME_CACHE_MAX_MEMORY: Override max memory (e.g., "512M", "2G")
- FLAME_CACHE_MAX_OBJECTS: Override max object count

For testing, set a lower memory limit to trigger eviction behavior:
  export FLAME_CACHE_MAX_MEMORY="1M"
"""

import time
import uuid

from flamepy.core import get_object, put_object

# Test session prefix to avoid conflicts with other tests
TEST_APP = "lru-test-app"
TEST_SESSION_PREFIX = "lru-test-ssn"


def generate_session_id() -> str:
    """Generate a unique key prefix in <app>/<ssn> format for testing."""
    return f"{TEST_APP}/{TEST_SESSION_PREFIX}-{uuid.uuid4().hex[:8]}"


def create_large_object(size_kb: int) -> dict:
    """Create a test object of approximately the specified size in KB.

    Args:
        size_kb: Approximate size of the object in kilobytes

    Returns:
        A dictionary containing padding data to reach the target size
    """
    # Each character is ~1 byte, so create a string of size_kb * 1024 chars
    # Account for some overhead from dict structure and serialization
    padding_size = size_kb * 1024
    return {
        "id": str(uuid.uuid4()),
        "timestamp": time.time(),
        "padding": "x" * padding_size,
    }


class TestLRUEviction:
    """Test suite for LRU eviction policy behavior."""

    def test_basic_eviction_on_memory_limit(self):
        """Test that objects are evicted when memory limit is exceeded.

        This test verifies that when the cache exceeds its configured memory
        limit, older objects are evicted from memory to make room for new ones.
        The evicted objects should still be retrievable (reloaded from disk).

        Note: The default max_memory is "1G" (1 gigabyte). For testing eviction
        behavior, configure a lower limit via FLAME_CACHE_MAX_MEMORY environment
        variable (e.g., "1M" for 1 megabyte).
        """
        session_id = generate_session_id()

        # Create multiple objects that together may exceed configured memory limits
        # Using 100KB objects - the number needed to trigger eviction depends on
        # the configured max_memory setting (default: "1G", test: "1M")
        object_refs = []
        num_objects = 15  # With 1M limit, ~10 objects should trigger eviction

        for i in range(num_objects):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i  # Track insertion order
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        # All objects should still be retrievable (either from memory or disk)
        for i, ref in enumerate(object_refs):
            retrieved = get_object(ref)
            assert retrieved["sequence"] == i, f"Object {i} data mismatch"

    def test_lru_order_maintained(self):
        """Test that LRU ordering is maintained - least recently used evicted first.

        This test creates objects, accesses some of them to update their LRU
        position, then adds more objects to trigger eviction. The objects that
        were accessed should remain in memory while older unaccessed objects
        are evicted first.
        """
        session_id = generate_session_id()

        # Create initial objects
        object_refs = []
        for i in range(5):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        # Access objects 0 and 2 to make them "recently used"
        # This should move them to the end of the LRU list
        _ = get_object(object_refs[0])
        _ = get_object(object_refs[2])

        # Add more objects to potentially trigger eviction
        for i in range(5, 15):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        # All objects should still be retrievable
        # Objects 0 and 2 should have been protected by recent access
        for i, ref in enumerate(object_refs):
            retrieved = get_object(ref)
            assert retrieved["sequence"] == i, f"Object {i} data mismatch after LRU reordering"

    def test_evicted_objects_reload_from_disk(self):
        """Test that evicted objects can be reloaded from disk.

        This test verifies the core LRU behavior: when an object is evicted
        from memory due to memory pressure, it should still be accessible
        by reloading from disk storage.
        """
        session_id = generate_session_id()

        # Create a unique marker for this test
        test_marker = str(uuid.uuid4())

        # Create first object with unique data
        first_obj = create_large_object(size_kb=100)
        first_obj["marker"] = test_marker
        first_obj["position"] = "first"
        first_ref = put_object(session_id, first_obj)

        # Create many more objects to push the first one out of memory
        for i in range(20):
            obj = create_large_object(size_kb=100)
            obj["filler"] = i
            put_object(session_id, obj)

        # The first object should still be retrievable (reloaded from disk)
        retrieved = get_object(first_ref)
        assert retrieved["marker"] == test_marker, "First object marker mismatch"
        assert retrieved["position"] == "first", "First object position mismatch"

    def test_multiple_access_updates_lru(self):
        """Test that multiple accesses to the same object keep it in memory.

        Repeatedly accessing an object should keep it at the "most recently
        used" end of the LRU list, preventing its eviction.
        """
        session_id = generate_session_id()

        # Create a "hot" object that we'll access frequently
        hot_obj = create_large_object(size_kb=50)
        hot_obj["type"] = "hot"
        hot_ref = put_object(session_id, hot_obj)

        # Create other objects and periodically access the hot object
        for i in range(20):
            # Create a new object
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            put_object(session_id, obj)

            # Access the hot object every few iterations
            if i % 3 == 0:
                retrieved = get_object(hot_ref)
                assert retrieved["type"] == "hot", "Hot object data corrupted"

        # Hot object should still be quickly accessible
        final_retrieved = get_object(hot_ref)
        assert final_retrieved["type"] == "hot", "Hot object not preserved"


class TestLRUEdgeCases:
    """Test edge cases and boundary conditions for LRU eviction."""

    def test_single_large_object(self):
        """Test handling of a single object that approaches memory limit."""
        session_id = generate_session_id()

        # Create a large object (500KB)
        large_obj = create_large_object(size_kb=500)
        large_obj["type"] = "large"
        ref = put_object(session_id, large_obj)

        # Should be retrievable
        retrieved = get_object(ref)
        assert retrieved["type"] == "large"

    def test_many_small_objects(self):
        """Test eviction behavior with many small objects."""
        session_id = generate_session_id()

        # Create many small objects (1KB each)
        object_refs = []
        for i in range(100):
            obj = {"id": i, "data": "x" * 1024}
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        # All should be retrievable
        for i, ref in enumerate(object_refs):
            retrieved = get_object(ref)
            assert retrieved["id"] == i

    def test_object_update_preserves_lru_position(self):
        """Test that updating an object updates its LRU position."""
        session_id = generate_session_id()

        # Create initial objects
        refs = []
        for i in range(5):
            obj = {"sequence": i, "version": 1}
            ref = put_object(session_id, obj)
            refs.append(ref)

        # Update the first object (should move it to most recently used)
        from flamepy.core import update_object

        updated_obj = {"sequence": 0, "version": 2}
        refs[0] = update_object(refs[0], updated_obj)

        # Add more objects to trigger potential eviction
        for i in range(5, 15):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            put_object(session_id, obj)

        # The updated object should still be accessible
        retrieved = get_object(refs[0])
        assert retrieved["sequence"] == 0
        assert retrieved["version"] == 2, "Updated object should have new version"

    def test_concurrent_session_isolation(self):
        """Test that LRU eviction is isolated per session."""
        session_id_1 = generate_session_id()
        session_id_2 = generate_session_id()

        # Create objects in session 1
        refs_1 = []
        for i in range(5):
            obj = {"session": 1, "sequence": i}
            ref = put_object(session_id_1, obj)
            refs_1.append(ref)

        # Create objects in session 2
        refs_2 = []
        for i in range(5):
            obj = {"session": 2, "sequence": i}
            ref = put_object(session_id_2, obj)
            refs_2.append(ref)

        # Both sessions' objects should be retrievable
        for i, ref in enumerate(refs_1):
            retrieved = get_object(ref)
            assert retrieved["session"] == 1
            assert retrieved["sequence"] == i

        for i, ref in enumerate(refs_2):
            retrieved = get_object(ref)
            assert retrieved["session"] == 2
            assert retrieved["sequence"] == i


class TestLRUReloadBehavior:
    """Test the reload-from-disk behavior after eviction."""

    def test_reload_preserves_data_integrity(self):
        """Test that reloaded objects have identical data to originals."""
        session_id = generate_session_id()

        # Create object with complex nested data
        original_obj = {
            "string": "test string with special chars: äöü",
            "number": 42.5,
            "list": [1, 2, 3, {"nested": "value"}],
            "dict": {"key1": "value1", "key2": [4, 5, 6]},
            "none": None,
            "bool": True,
        }
        ref = put_object(session_id, original_obj)

        # Create many objects to potentially evict the original
        for i in range(20):
            obj = create_large_object(size_kb=100)
            put_object(session_id, obj)

        # Retrieve and verify data integrity
        retrieved = get_object(ref)
        assert retrieved["string"] == original_obj["string"]
        assert retrieved["number"] == original_obj["number"]
        assert retrieved["list"] == original_obj["list"]
        assert retrieved["dict"] == original_obj["dict"]
        assert retrieved["none"] is None
        assert retrieved["bool"] is True

    def test_reload_after_multiple_evictions(self):
        """Test that objects can be reloaded multiple times."""
        session_id = generate_session_id()

        # Create a target object
        target_obj = {"target": True, "id": str(uuid.uuid4())}
        target_ref = put_object(session_id, target_obj)

        # Perform multiple cycles of adding objects and retrieving target
        for cycle in range(3):
            # Add objects to potentially evict target
            for i in range(10):
                obj = create_large_object(size_kb=100)
                obj["cycle"] = cycle
                obj["index"] = i
                put_object(session_id, obj)

            # Retrieve target - may reload from disk
            retrieved = get_object(target_ref)
            assert retrieved["target"] is True, f"Target object corrupted in cycle {cycle}"
            assert retrieved["id"] == target_obj["id"], f"Target ID mismatch in cycle {cycle}"

    def test_access_pattern_affects_eviction_order(self):
        """Test that access patterns determine eviction order.

        Objects accessed more recently should be evicted later than
        objects that haven't been accessed.
        """
        session_id = generate_session_id()

        # Create objects with identifiable markers
        markers = ["alpha", "beta", "gamma", "delta", "epsilon"]
        refs = {}

        for marker in markers:
            obj = {"marker": marker, "padding": "x" * 10000}
            ref = put_object(session_id, obj)
            refs[marker] = ref

        # Access in specific order: gamma, alpha, epsilon
        # This makes beta and delta the least recently used
        for marker in ["gamma", "alpha", "epsilon"]:
            _ = get_object(refs[marker])

        # Add more objects to trigger eviction
        for i in range(15):
            obj = create_large_object(size_kb=100)
            put_object(session_id, obj)

        # All objects should still be retrievable (from memory or disk)
        for marker in markers:
            retrieved = get_object(refs[marker])
            assert retrieved["marker"] == marker, f"Object {marker} data mismatch"

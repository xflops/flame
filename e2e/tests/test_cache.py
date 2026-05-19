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
"""

import time
import uuid

import flamepy.core.cache as cache_module
import pyarrow as pa
import pyarrow.flight as flight
import pytest
from flamepy.core import FlameContext, ObjectRef, get_object, patch_object, put_object, update_object


def test_cache_put_and_get():
    """Test basic put and get operations."""
    key_prefix = "test-app/test-session-001"
    test_data = {"message": "Hello, Flame!", "value": 42}

    ref = put_object(key_prefix, test_data)

    assert isinstance(ref, ObjectRef)
    assert ref.endpoint is not None
    assert ref.key is not None
    assert ref.key.startswith(key_prefix + "/")
    assert ref.version == 1

    result = get_object(ref)
    assert result == test_data


def test_cache_put_and_get_native_arrow_table():
    """Test that Arrow tables round-trip through the native cache path."""
    key_prefix = f"test-app/test-native-arrow-{uuid.uuid4().hex[:8]}"
    table = pa.table({"col1": [1, 2, 3], "col2": ["a", "b", "c"]})

    ref = put_object(key_prefix, table)

    assert ref.version == 1

    result = get_object(ref)
    assert isinstance(result, pa.Table)
    assert result.to_pydict() == table.to_pydict()
    assert b"flame.cache.format" not in (result.schema.metadata or {})


def test_cache_native_arrow_table_update():
    """Test that native Arrow table updates rewrite the base object."""
    key_prefix = f"test-app/test-native-arrow-update-{uuid.uuid4().hex[:8]}"
    table = pa.table({"value": [1, 2, 3]})
    updated = pa.table({"value": [4, 5], "label": ["x", "y"]})

    ref = put_object(key_prefix, table)
    updated_ref = update_object(ref, updated)

    assert updated_ref.key == ref.key
    assert updated_ref.version == ref.version + 1

    result = get_object(updated_ref)
    assert isinstance(result, pa.Table)
    assert result.to_pydict() == updated.to_pydict()


def test_cache_update():
    """Test update operation."""
    key_prefix = "test-app/test-session-002"
    original_data = {"count": 0}
    updated_data = {"count": 1}

    ref = put_object(key_prefix, original_data)
    new_ref = update_object(ref, updated_data)

    assert new_ref.key == ref.key
    assert new_ref.endpoint == ref.endpoint

    result = get_object(new_ref)
    assert result == updated_data


def test_cache_with_complex_objects():
    """Test caching complex Python objects."""
    key_prefix = "test-app/test-session-003"

    class ComplexObject:
        def __init__(self, name, data):
            self.name = name
            self.data = data

        def __eq__(self, other):
            return self.name == other.name and self.data == other.data

    test_obj = ComplexObject("test", [1, 2, 3, {"nested": "value"}])

    ref = put_object(key_prefix, test_obj)
    retrieved_obj = get_object(ref)

    assert isinstance(retrieved_obj, ComplexObject)
    assert retrieved_obj.name == test_obj.name
    assert retrieved_obj.data == test_obj.data


def test_objectref_encode_decode():
    """Test ObjectRef serialization and deserialization."""
    ref = ObjectRef(endpoint="grpc://127.0.0.1:9090", key="app/session123/obj456", version=0)

    # Encode
    encoded = ref.encode()
    assert isinstance(encoded, bytes)

    # Decode
    decoded = ObjectRef.decode(encoded)
    assert decoded.endpoint == ref.endpoint
    assert decoded.key == ref.key
    assert decoded.version == ref.version


# ============================================================================
# PATCH Operation Tests
# ============================================================================


def _raw_deserializer(base, deltas):
    """Deserializer that returns base and deltas as a dict for testing."""
    return {"base": base, "deltas": deltas}


def _remote_patch_without_local_cache_invalidation(ref: ObjectRef, delta):
    """Patch through Flight directly to emulate another client process."""
    batch = cache_module._serialize_object(delta)
    client = cache_module._get_flight_client(ref.endpoint, cache_module._get_cache_tls_config())
    descriptor = flight.FlightDescriptor.for_command(f"PATCH:{ref.key}".encode())
    return cache_module._do_put_remote(client, descriptor, batch)


def _remote_update_without_local_cache_invalidation(ref: ObjectRef, new_obj):
    """Update through Flight directly to emulate another client process."""
    batch = cache_module._serialize_object(new_obj)
    client = cache_module._get_flight_client(ref.endpoint, cache_module._get_cache_tls_config())
    descriptor = flight.FlightDescriptor.for_path(ref.key)
    return cache_module._do_put_remote(client, descriptor, batch)


def _cached_object(ref: ObjectRef):
    with cache_module._cache_lock:
        return cache_module._object_cache[(ref.endpoint, ref.key)]


def test_patch_single_delta():
    """Test patching an object with a single delta."""
    key_prefix = "test-app/test-patch-001"
    base_data = {"logs": []}
    delta_data = {"worker": 1, "log": "started"}

    ref = put_object(key_prefix, base_data)
    patched_ref = patch_object(ref, delta_data)

    assert patched_ref.key == ref.key
    assert patched_ref.endpoint == ref.endpoint

    result = get_object(patched_ref, deserializer=_raw_deserializer)

    assert result["base"] == base_data
    assert len(result["deltas"]) == 1
    assert result["deltas"][0] == delta_data


def test_patch_multiple_deltas():
    """Test patching an object with multiple deltas."""
    key_prefix = "test-app/test-patch-002"
    base_data = {"results": []}

    ref = put_object(key_prefix, base_data)

    delta1 = {"worker": 1, "result": 100}
    delta2 = {"worker": 2, "result": 200}
    delta3 = {"worker": 3, "result": 300}

    ref = patch_object(ref, delta1)
    ref = patch_object(ref, delta2)
    ref = patch_object(ref, delta3)

    result = get_object(ref, deserializer=_raw_deserializer)

    assert result["base"] == base_data
    assert len(result["deltas"]) == 3
    assert result["deltas"][0] == delta1
    assert result["deltas"][1] == delta2
    assert result["deltas"][2] == delta3


def test_incremental_get_applies_remote_patch_only_response():
    """Test a cached client applies only patches appended by another client."""
    key_prefix = f"test-app/test-incremental-patch-{uuid.uuid4().hex[:8]}"
    base_data = {"items": ["base"]}
    delta_data_1 = {"items": ["patch-1"]}
    delta_data_2 = {"items": ["patch-2"]}

    ref = put_object(key_prefix, base_data)
    assert get_object(ref, deserializer=_raw_deserializer) == {"base": base_data, "deltas": []}

    patched_ref = _remote_patch_without_local_cache_invalidation(ref, delta_data_1)
    fetch_result = cache_module._fetch_object_data(ref, ref.version)

    assert fetch_result.mode == cache_module.FetchMode.PATCHES
    assert fetch_result.version == patched_ref.version
    assert [patch.data for patch in fetch_result.patches] == [delta_data_1]

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result == {"base": base_data, "deltas": [delta_data_1]}

    cached = _cached_object(ref)
    assert cached.version == patched_ref.version
    assert [patch.data for patch in cached.patches] == [delta_data_1]

    patched_ref_2 = _remote_patch_without_local_cache_invalidation(ref, delta_data_2)
    second_fetch_result = cache_module._fetch_object_data(ref, cached.version)

    assert second_fetch_result.mode == cache_module.FetchMode.PATCHES
    assert second_fetch_result.version == patched_ref_2.version
    assert [patch.data for patch in second_fetch_result.patches] == [delta_data_2]

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result == {"base": base_data, "deltas": [delta_data_1, delta_data_2]}

    cached = _cached_object(ref)
    assert cached.version == patched_ref_2.version
    assert [patch.data for patch in cached.patches] == [delta_data_1, delta_data_2]


def test_version_zero_forces_full_response_with_cached_object():
    """Test version=0 gets the full base plus patches even with a local cache."""
    key_prefix = f"test-app/test-incremental-full-{uuid.uuid4().hex[:8]}"
    base_data = {"items": ["base"]}
    delta_data = {"items": ["patch-1"]}

    ref = put_object(key_prefix, base_data)
    assert get_object(ref, deserializer=_raw_deserializer) == {"base": base_data, "deltas": []}
    patched_ref = _remote_patch_without_local_cache_invalidation(ref, delta_data)

    forced_ref = ObjectRef(endpoint=ref.endpoint, key=ref.key, version=0)
    fetch_result = cache_module._fetch_object_data(forced_ref, 0)

    assert fetch_result.mode == cache_module.FetchMode.FULL
    assert fetch_result.version == patched_ref.version
    assert fetch_result.base == base_data
    assert [patch.data for patch in fetch_result.patches] == [delta_data]

    result = get_object(forced_ref, deserializer=_raw_deserializer)
    assert result == {"base": base_data, "deltas": [delta_data]}


def test_incremental_get_falls_back_to_full_after_remote_update():
    """Test stale cached base is replaced by a full response after update."""
    key_prefix = f"test-app/test-incremental-update-{uuid.uuid4().hex[:8]}"
    base_data = {"version": 1}
    updated_data = {"version": 2}

    ref = put_object(key_prefix, base_data)
    assert get_object(ref, deserializer=_raw_deserializer) == {"base": base_data, "deltas": []}

    updated_ref = _remote_update_without_local_cache_invalidation(ref, updated_data)
    fetch_result = cache_module._fetch_object_data(ref, ref.version)

    assert fetch_result.mode == cache_module.FetchMode.FULL
    assert fetch_result.version == updated_ref.version
    assert fetch_result.base == updated_data
    assert fetch_result.patches == []

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result == {"base": updated_data, "deltas": []}

    cached = _cached_object(ref)
    assert cached.version == updated_ref.version
    assert cached.data == updated_data
    assert cached.patches == []


def test_patch_preserves_delta_order():
    """Test that deltas are returned in the order they were appended."""
    key_prefix = "test-app/test-patch-003"
    base_data = {"sequence": "start"}

    ref = put_object(key_prefix, base_data)

    for i in range(5):
        ref = patch_object(ref, {"index": i, "value": f"delta_{i}"})

    result = get_object(ref, deserializer=_raw_deserializer)

    assert result["base"] == base_data
    assert len(result["deltas"]) == 5
    for i, delta in enumerate(result["deltas"]):
        assert delta["index"] == i
        assert delta["value"] == f"delta_{i}"


def test_patch_with_complex_delta():
    """Test patching with complex Python objects as deltas."""
    key_prefix = "test-app/test-patch-004"

    class LogEntry:
        def __init__(self, level, message):
            self.level = level
            self.message = message

        def __eq__(self, other):
            return self.level == other.level and self.message == other.message

    base_data = {"log_name": "app.log"}
    delta1 = LogEntry("INFO", "Application started")
    delta2 = LogEntry("WARNING", "Low memory")
    delta3 = LogEntry("ERROR", "Connection failed")

    ref = put_object(key_prefix, base_data)

    ref = patch_object(ref, delta1)
    ref = patch_object(ref, delta2)
    ref = patch_object(ref, delta3)

    result = get_object(ref, deserializer=_raw_deserializer)

    assert result["base"] == base_data
    assert len(result["deltas"]) == 3
    assert result["deltas"][0] == delta1
    assert result["deltas"][1] == delta2
    assert result["deltas"][2] == delta3


def test_update_clears_deltas():
    """Test that update operation clears all existing deltas."""
    key_prefix = "test-app/test-patch-005"
    base_data = {"version": 1}

    ref = put_object(key_prefix, base_data)

    ref = patch_object(ref, {"delta": 1})
    ref = patch_object(ref, {"delta": 2})
    ref = patch_object(ref, {"delta": 3})

    result = get_object(ref, deserializer=_raw_deserializer)
    assert len(result["deltas"]) == 3

    new_base = {"version": 2}
    ref = update_object(ref, new_base)

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result["base"] == new_base
    assert result["deltas"] == []


def test_put_clears_deltas():
    """Test that put operation on same key clears existing deltas."""
    key_prefix = "test-app/test-patch-006"
    base_data = {"initial": True}

    ref = put_object(key_prefix, base_data)

    ref = patch_object(ref, {"delta": "a"})
    ref = patch_object(ref, {"delta": "b"})

    result = get_object(ref, deserializer=_raw_deserializer)
    assert len(result["deltas"]) == 2

    new_ref = put_object(key_prefix, {"new": True})

    new_result = get_object(new_ref)
    assert new_result == {"new": True}


def test_patch_nonexistent_object():
    """Test that patching a non-existent object raises an error."""
    ctx = FlameContext()
    cache_config = ctx.cache
    cache_endpoint = cache_config.get("endpoint") if isinstance(cache_config, dict) else cache_config

    fake_ref = ObjectRef(endpoint=cache_endpoint, key="test-app/nonexistent-session/nonexistent-object", version=0)

    with pytest.raises(Exception):
        patch_object(fake_ref, {"delta": "data"})


def test_patch_with_nested_data():
    """Test patching with deeply nested data structures."""
    key_prefix = "test-app/test-patch-007"
    base_data = {"root": {"level1": {"level2": []}}}

    ref = put_object(key_prefix, base_data)

    delta = {"nested": {"deep": {"data": [1, 2, 3], "more": {"key": "value"}}}}
    ref = patch_object(ref, delta)

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result["base"] == base_data
    assert len(result["deltas"]) == 1
    assert result["deltas"][0] == delta


def test_patch_with_binary_data():
    """Test patching with binary data."""
    key_prefix = "test-app/test-patch-008"
    base_data = {"type": "binary_container"}

    ref = put_object(key_prefix, base_data)

    binary_delta = {"binary": b"\x00\x01\x02\x03\xff\xfe\xfd"}
    ref = patch_object(ref, binary_delta)

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result["base"] == base_data
    assert len(result["deltas"]) == 1
    assert result["deltas"][0]["binary"] == binary_delta["binary"]


def test_patch_with_none_values():
    """Test patching with None values in data."""
    key_prefix = "test-app/test-patch-009"
    base_data = {"value": None}

    ref = put_object(key_prefix, base_data)

    delta = {"key": None, "list": [None, 1, None]}
    ref = patch_object(ref, delta)

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result["base"] == base_data
    assert len(result["deltas"]) == 1
    assert result["deltas"][0] == delta


def test_patch_large_number_of_deltas():
    """Test patching with a large number of deltas."""
    key_prefix = "test-app/test-patch-010"
    base_data = {"counter": 0}

    ref = put_object(key_prefix, base_data)

    num_deltas = 50
    for i in range(num_deltas):
        ref = patch_object(ref, {"increment": i + 1})

    result = get_object(ref, deserializer=_raw_deserializer)
    assert result["base"] == base_data
    assert len(result["deltas"]) == num_deltas

    for i, delta in enumerate(result["deltas"]):
        assert delta["increment"] == i + 1


@pytest.mark.skip(reason="MAX_DELTAS_PER_OBJECT=1000 is too slow for E2E; tested via unit tests")
def test_patch_max_deltas_limit():
    """Test that patching beyond MAX_DELTAS_PER_OBJECT (1000) raises an error."""
    pass


# ============================================================================
# Cache Pressure Retrieval Tests
# ============================================================================

"""
Integration stress tests for object retrieval under cache pressure.

These tests intentionally assert only observable cache behavior through the
public API: objects remain retrievable and data stays intact after many writes.
The actual LRU eviction policy and ordering are covered by object_cache unit
tests, where cache limits and policy internals are directly configurable.
"""

TEST_APP = "cache-pressure-test-app"
TEST_SESSION_PREFIX = "cache-pressure-test-ssn"


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
    padding_size = size_kb * 1024
    return {
        "id": str(uuid.uuid4()),
        "timestamp": time.time(),
        "padding": "x" * padding_size,
    }


class TestCachePressureRetrieval:
    """Test cache retrieval behavior after many writes."""

    def test_objects_remain_retrievable_after_many_writes(self):
        """Test that objects remain retrievable after writing many objects."""
        session_id = generate_session_id()

        object_refs = []
        num_objects = 15

        for i in range(num_objects):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        for i, ref in enumerate(object_refs):
            retrieved = get_object(ref)
            assert retrieved["sequence"] == i, f"Object {i} data mismatch"

    def test_accessed_objects_remain_retrievable_after_more_writes(self):
        """Test that accessed objects remain retrievable after more writes."""
        session_id = generate_session_id()

        object_refs = []
        for i in range(5):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        _ = get_object(object_refs[0])
        _ = get_object(object_refs[2])

        for i in range(5, 15):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        for i, ref in enumerate(object_refs):
            retrieved = get_object(ref)
            assert retrieved["sequence"] == i, f"Object {i} data mismatch after more writes"

    def test_early_object_remains_retrievable_after_many_writes(self):
        """Test that an early object remains retrievable after many writes."""
        session_id = generate_session_id()

        test_marker = str(uuid.uuid4())

        first_obj = create_large_object(size_kb=100)
        first_obj["marker"] = test_marker
        first_obj["position"] = "first"
        first_ref = put_object(session_id, first_obj)

        for i in range(20):
            obj = create_large_object(size_kb=100)
            obj["filler"] = i
            put_object(session_id, obj)

        retrieved = get_object(first_ref)
        assert retrieved["marker"] == test_marker, "First object marker mismatch"
        assert retrieved["position"] == "first", "First object position mismatch"

    def test_repeatedly_accessed_object_remains_retrievable(self):
        """Test that repeated access does not corrupt object retrieval."""
        session_id = generate_session_id()

        hot_obj = create_large_object(size_kb=50)
        hot_obj["type"] = "hot"
        hot_ref = put_object(session_id, hot_obj)

        for i in range(20):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            put_object(session_id, obj)

            if i % 3 == 0:
                retrieved = get_object(hot_ref)
                assert retrieved["type"] == "hot", "Hot object data corrupted"

        final_retrieved = get_object(hot_ref)
        assert final_retrieved["type"] == "hot", "Hot object not preserved"


class TestCachePressureEdgeCases:
    """Test cache retrieval edge cases under write pressure."""

    def test_single_large_object(self):
        """Test handling of a single object that approaches memory limit."""
        session_id = generate_session_id()

        large_obj = create_large_object(size_kb=500)
        large_obj["type"] = "large"
        ref = put_object(session_id, large_obj)

        retrieved = get_object(ref)
        assert retrieved["type"] == "large"

    def test_many_small_objects(self):
        """Test retrieval with many small objects."""
        session_id = generate_session_id()

        object_refs = []
        for i in range(100):
            obj = {"id": i, "data": "x" * 1024}
            ref = put_object(session_id, obj)
            object_refs.append(ref)

        for i, ref in enumerate(object_refs):
            retrieved = get_object(ref)
            assert retrieved["id"] == i

    def test_object_update_remains_retrievable_after_more_writes(self):
        """Test that an updated object remains retrievable after more writes."""
        session_id = generate_session_id()

        refs = []
        for i in range(5):
            obj = {"sequence": i, "version": 1}
            ref = put_object(session_id, obj)
            refs.append(ref)

        updated_obj = {"sequence": 0, "version": 2}
        refs[0] = update_object(refs[0], updated_obj)

        for i in range(5, 15):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            put_object(session_id, obj)

        retrieved = get_object(refs[0])
        assert retrieved["sequence"] == 0
        assert retrieved["version"] == 2, "Updated object should have new version"

    def test_concurrent_session_isolation(self):
        """Test that cache keys are isolated per session."""
        session_id_1 = generate_session_id()
        session_id_2 = generate_session_id()

        refs_1 = []
        for i in range(5):
            obj = {"session": 1, "sequence": i}
            ref = put_object(session_id_1, obj)
            refs_1.append(ref)

        refs_2 = []
        for i in range(5):
            obj = {"session": 2, "sequence": i}
            ref = put_object(session_id_2, obj)
            refs_2.append(ref)

        for i, ref in enumerate(refs_1):
            retrieved = get_object(ref)
            assert retrieved["session"] == 1
            assert retrieved["sequence"] == i

        for i, ref in enumerate(refs_2):
            retrieved = get_object(ref)
            assert retrieved["session"] == 2
            assert retrieved["sequence"] == i


class TestCachePressureDataIntegrity:
    """Test data integrity after repeated cache writes."""

    def test_retrieval_preserves_data_integrity_after_many_writes(self):
        """Test that retrieved objects have identical data to originals."""
        session_id = generate_session_id()

        original_obj = {
            "string": "test string with special chars: äöü",
            "number": 42.5,
            "list": [1, 2, 3, {"nested": "value"}],
            "dict": {"key1": "value1", "key2": [4, 5, 6]},
            "none": None,
            "bool": True,
        }
        ref = put_object(session_id, original_obj)

        for i in range(20):
            obj = create_large_object(size_kb=100)
            put_object(session_id, obj)

        retrieved = get_object(ref)
        assert retrieved["string"] == original_obj["string"]
        assert retrieved["number"] == original_obj["number"]
        assert retrieved["list"] == original_obj["list"]
        assert retrieved["dict"] == original_obj["dict"]
        assert retrieved["none"] is None
        assert retrieved["bool"] is True

    def test_retrieval_after_multiple_write_cycles(self):
        """Test that objects remain retrievable across multiple write cycles."""
        session_id = generate_session_id()

        target_obj = {"target": True, "id": str(uuid.uuid4())}
        target_ref = put_object(session_id, target_obj)

        for cycle in range(3):
            for i in range(10):
                obj = create_large_object(size_kb=100)
                obj["cycle"] = cycle
                obj["index"] = i
                put_object(session_id, obj)

            retrieved = get_object(target_ref)
            assert retrieved["target"] is True, f"Target object corrupted in cycle {cycle}"
            assert retrieved["id"] == target_obj["id"], f"Target ID mismatch in cycle {cycle}"

    def test_mixed_access_pattern_preserves_data(self):
        """Test that mixed access and write patterns preserve data."""
        session_id = generate_session_id()

        markers = ["alpha", "beta", "gamma", "delta", "epsilon"]
        refs = {}

        for marker in markers:
            obj = {"marker": marker, "padding": "x" * 10000}
            ref = put_object(session_id, obj)
            refs[marker] = ref

        for marker in ["gamma", "alpha", "epsilon"]:
            _ = get_object(refs[marker])

        for i in range(15):
            obj = create_large_object(size_kb=100)
            put_object(session_id, obj)

        for marker in markers:
            retrieved = get_object(refs[marker])
            assert retrieved["marker"] == marker, f"Object {marker} data mismatch"

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

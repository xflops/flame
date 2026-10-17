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
from contextlib import contextmanager
from urllib.parse import urlparse

import flamepy.core.cache as cache_module
import grpc
import pyarrow as pa
import pytest
from flamepy.core import (
    FlameContext,
    ObjectRef,
    delete_objects,
    delete_package,
    download_package,
    get_object,
    patch_object,
    put_object,
    upload_package,
)
from flamepy.proto import cache_pb2, cache_pb2_grpc


@contextmanager
def _remote_cache_client(ref: ObjectRef):
    """Use a separate gRPC client so remote writes do not touch the SDK cache."""
    endpoint, target = cache_module._resolve_cache_endpoint(ref.endpoint)
    parsed = urlparse(endpoint)
    options = list(cache_module.GRPC_OPTIONS)
    metadata = (("x-flame-object-cache", target),) if target else ()
    if parsed.scheme in ("grpcs", "grpc+tls", "grpcs-proxy"):
        tls = cache_module._get_cache_tls_config()
        roots = None
        cert = key = None
        if tls and tls.ca_file:
            with open(tls.ca_file, "rb") as roots_file:
                roots = roots_file.read()
        if tls and tls.cert_file and tls.key_file:
            with open(tls.cert_file, "rb") as cert_file:
                cert = cert_file.read()
            with open(tls.key_file, "rb") as key_file:
                key = key_file.read()
        channel = grpc.secure_channel(parsed.netloc, grpc.ssl_channel_credentials(root_certificates=roots, private_key=key, certificate_chain=cert), options=options)
    else:
        channel = grpc.insecure_channel(parsed.netloc, options=options)
    with channel:
        yield cache_pb2_grpc.ObjectCacheServiceStub(channel), metadata


def _remote_metadata(ref: ObjectRef):
    with _remote_cache_client(ref) as (client, metadata):
        return client.GetMetadata(cache_pb2.CacheGetMetadataRequest(key=ref.key), metadata=metadata)


def _remote_write(rpc: str, ref: ObjectRef, data_type: str, data: bytes) -> ObjectRef:
    def requests():
        yield cache_pb2.CacheWriteRequest(header=cache_pb2.CacheWriteHeader(key=ref.key, data_type=data_type))
        for offset in range(0, len(data), cache_module._UPLOAD_CHUNK_SIZE):
            yield cache_pb2.CacheWriteRequest(data=data[offset : offset + cache_module._UPLOAD_CHUNK_SIZE])

    with _remote_cache_client(ref) as (client, routing):
        response = getattr(client, rpc)(requests(), metadata=routing)
    return ObjectRef(endpoint=response.endpoint, key=response.key, version=response.version)


def _remote_delete(ref: ObjectRef) -> None:
    with _remote_cache_client(ref) as (client, metadata):
        client.Delete(cache_pb2.CacheDeleteRequest(key=ref.key), metadata=metadata)


def _remote_get(ref: ObjectRef, version: int):
    with _remote_cache_client(ref) as (client, metadata):
        responses = list(client.Get(cache_pb2.CacheGetRequest(key=ref.key, version=version), metadata=metadata))
    assert responses and responses[0].WhichOneof("payload") == "header"
    assert all(response.WhichOneof("payload") == "chunk" for response in responses[1:])
    return responses[0].header, [response.chunk for response in responses[1:]]


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


def test_cache_cloudpickle_object_and_appended_data_remain_raw():
    """Arbitrary Python objects remain uncompressed, including appended data."""
    key_prefix = f"test-app/test-cloudpickle-{uuid.uuid4().hex[:8]}"
    first = {"text": "repeat-me" * 2000}
    second = {"text": "another-repeat" * 2000}

    ref = put_object(key_prefix, first)
    metadata = _remote_metadata(ref)
    assert metadata.data_type == "cloudpickle"
    appended = patch_object(ref, second)

    assert get_object(appended, deserializer=_parts) == [first, second]


def test_cache_zstd_arrow_table():
    """The Python SDK compresses Arrow data and tags the stored type."""
    key_prefix = f"test-app/test-zstd-arrow-{uuid.uuid4().hex[:8]}"
    table = pa.table({"item": ["repeat-me"] * 1000})

    ref = put_object(key_prefix, table)
    metadata = _remote_metadata(ref)
    assert metadata.data_type == "arrow.table.zstd"

    assert get_object(ref).equals(table)


def test_cache_put_and_get_native_arrow_table():
    """Test that Arrow tables round-trip through the object cache."""
    key_prefix = f"test-app/test-native-arrow-{uuid.uuid4().hex[:8]}"
    table = pa.table({"col1": [1, 2, 3], "col2": ["a", "b", "c"]})

    ref = put_object(key_prefix, table)

    assert ref.version == 1

    result = get_object(ref)
    assert isinstance(result, pa.Table)
    assert result.to_pydict() == table.to_pydict()
    assert b"flame.cache.format" not in (result.schema.metadata or {})


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
# Appending data (patch_object) and versioned reads
# ============================================================================
#
# An object is a list of data parts: put_object creates it with one part
# (version 1), every patch_object appends a part (versions 2, 3, ...), and an
# object is never replaced. get_object returns the newest part, or a value made
# of all the parts, in the order they were written, by a deserializer.


def _parts(parts):
    """Deserializer that returns all the data parts of an object."""
    return parts


def _remote_patch_without_local_cache_invalidation(ref: ObjectRef, data):
    """Append through gRPC directly to emulate another client process."""
    data_type, payload = cache_module._serialize_object_data(data)
    return _remote_write("Patch", ref, data_type, payload)


def _cached_object(ref: ObjectRef):
    with cache_module._cache_lock:
        return cache_module._object_cache[(ref.endpoint, ref.key)]


def test_object_is_created_with_one_part_at_version_one():
    ref = put_object(f"test-app/test-parts-{uuid.uuid4().hex[:8]}", {"first": True})

    assert ref.version == 1
    assert get_object(ref, deserializer=_parts) == [{"first": True}]
    assert _remote_metadata(ref).version == 1


def test_put_cannot_replace_an_existing_object():
    """An object is created once: a put to an existing key fails and changes nothing."""
    ref = put_object(f"test-app/test-create-only-{uuid.uuid4().hex[:8]}", {"first": True})
    data_type, payload = cache_module._serialize_object_data({"second": True})

    with pytest.raises(grpc.RpcError) as error:
        _remote_write("Put", ref, data_type, payload)
    assert error.value.code() == grpc.StatusCode.ALREADY_EXISTS

    assert get_object(ref, deserializer=_parts) == [{"first": True}]
    assert _remote_metadata(ref).version == 1


def test_patch_appends_a_part_and_bumps_the_version():
    ref = put_object(f"test-app/test-append-{uuid.uuid4().hex[:8]}", {"logs": []})
    appended = patch_object(ref, {"worker": 1, "log": "started"})

    assert appended.key == ref.key
    assert appended.endpoint == ref.endpoint
    assert appended.version == ref.version + 1
    assert get_object(appended, deserializer=_parts) == [{"logs": []}, {"worker": 1, "log": "started"}]


def test_get_without_a_deserializer_returns_the_newest_part():
    ref = put_object(f"test-app/test-newest-{uuid.uuid4().hex[:8]}", {"n": 0})
    assert get_object(ref) == {"n": 0}

    for n in (1, 2, 3):
        ref = patch_object(ref, {"n": n})
        assert get_object(ref) == {"n": n}


def test_multiple_appends_keep_their_order_and_versions():
    ref = put_object(f"test-app/test-order-{uuid.uuid4().hex[:8]}", {"sequence": "start"})
    versions = [ref.version]

    for i in range(5):
        ref = patch_object(ref, {"index": i, "value": f"part_{i}"})
        versions.append(ref.version)

    assert versions == [1, 2, 3, 4, 5, 6]
    parts = get_object(ref, deserializer=_parts)
    assert parts[0] == {"sequence": "start"}
    assert parts[1:] == [{"index": i, "value": f"part_{i}"} for i in range(5)]


def test_a_deserializer_combines_the_parts():
    ref = put_object(f"test-app/test-combine-{uuid.uuid4().hex[:8]}", [1, 2])
    ref = patch_object(ref, [3])
    ref = patch_object(ref, [4, 5])

    merged = get_object(ref, deserializer=lambda parts: [item for part in parts for item in part])
    assert merged == [1, 2, 3, 4, 5]


def test_get_returns_only_the_data_after_a_version():
    """A Get with a version returns the parts after it, and nothing at the current version."""
    ref = put_object(f"test-app/test-after-{uuid.uuid4().hex[:8]}", {"n": 1})
    for n in (2, 3, 4):
        ref = patch_object(ref, {"n": n})
    assert ref.version == 4

    for version, expected in [(0, [1, 2, 3, 4]), (1, [2, 3, 4]), (2, [3, 4]), (3, [4]), (4, [])]:
        header, chunks = _remote_get(ref, version)
        assert header.version == 4
        assert [chunk.version for chunk in chunks] == expected
        if chunks:
            assert header.data_type == "cloudpickle"

    # A version the object does not have is an error, not another object.
    with pytest.raises(grpc.RpcError) as error:
        _remote_get(ref, 9)
    assert error.value.code() == grpc.StatusCode.INVALID_ARGUMENT


def test_incremental_get_applies_only_the_data_another_client_appended():
    """A cached client fetches and applies only the parts appended by another client."""
    ref = put_object(f"test-app/test-incremental-{uuid.uuid4().hex[:8]}", {"items": ["first"]})
    assert get_object(ref, deserializer=_parts) == [{"items": ["first"]}]
    assert _cached_object(ref).version == 1

    second = _remote_patch_without_local_cache_invalidation(ref, {"items": ["second"]})
    header, chunks = _remote_get(ref, ref.version)
    assert header.version == second.version
    assert [chunk.version for chunk in chunks] == [second.version]

    assert get_object(ref, deserializer=_parts) == [{"items": ["first"]}, {"items": ["second"]}]
    cached = _cached_object(ref)
    assert cached.version == second.version
    assert cached.data == [{"items": ["first"]}, {"items": ["second"]}]

    third = _remote_patch_without_local_cache_invalidation(ref, {"items": ["third"]})
    header, chunks = _remote_get(ref, cached.version)
    assert header.version == third.version
    assert [chunk.version for chunk in chunks] == [third.version]

    assert get_object(ref, deserializer=_parts)[-1] == {"items": ["third"]}
    assert _cached_object(ref).version == third.version
    assert len(_cached_object(ref).data) == 3


def test_version_zero_gets_all_the_parts_even_with_a_cached_object():
    ref = put_object(f"test-app/test-version-zero-{uuid.uuid4().hex[:8]}", {"items": ["first"]})
    assert get_object(ref, deserializer=_parts) == [{"items": ["first"]}]
    appended = _remote_patch_without_local_cache_invalidation(ref, {"items": ["second"]})

    forced = ObjectRef(endpoint=ref.endpoint, key=ref.key, version=0)
    header, chunks = _remote_get(forced, 0)
    assert header.version == appended.version
    assert [chunk.version for chunk in chunks] == [1, 2]

    assert get_object(forced, deserializer=_parts) == [{"items": ["first"]}, {"items": ["second"]}]


def test_get_after_the_object_was_created_again():
    """A client whose cached version is ahead of a new object of the same key starts over."""
    ref = put_object(f"test-app/test-again-{uuid.uuid4().hex[:8]}", {"n": 1})
    ref = patch_object(ref, {"n": 2})
    ref = patch_object(ref, {"n": 3})
    assert get_object(ref, deserializer=_parts) == [{"n": 1}, {"n": 2}, {"n": 3}]
    assert _cached_object(ref).version == 3

    # Another client deletes the object and creates it again, shorter.
    _remote_delete(ref)
    data_type, payload = cache_module._serialize_object_data({"again": True})
    _remote_write("Put", ref, data_type, payload)

    assert get_object(ref, deserializer=_parts) == [{"again": True}]
    assert _cached_object(ref).version == 1


def test_appending_keeps_the_cached_parts_of_the_object():
    """The cached parts stay valid after an append: the next get adds only the new data."""
    ref = put_object(f"test-app/test-keep-{uuid.uuid4().hex[:8]}", {"n": 1})
    assert get_object(ref, deserializer=_parts) == [{"n": 1}]
    cached = _cached_object(ref)

    ref = patch_object(ref, {"n": 2})

    assert _cached_object(ref) is cached
    assert get_object(ref, deserializer=_parts) == [{"n": 1}, {"n": 2}]
    assert _cached_object(ref) is cached
    assert cached.version == 2


def test_patch_with_complex_python_objects():
    class LogEntry:
        def __init__(self, level, message):
            self.level = level
            self.message = message

        def __eq__(self, other):
            return self.level == other.level and self.message == other.message

    entries = [LogEntry("INFO", "Application started"), LogEntry("WARNING", "Low memory"), LogEntry("ERROR", "Connection failed")]
    ref = put_object(f"test-app/test-complex-{uuid.uuid4().hex[:8]}", LogEntry("DEBUG", "created"))
    for entry in entries:
        ref = patch_object(ref, entry)

    parts = get_object(ref, deserializer=_parts)
    assert parts == [LogEntry("DEBUG", "created"), *entries]


def test_patch_with_nested_binary_and_none_data():
    ref = put_object(f"test-app/test-shapes-{uuid.uuid4().hex[:8]}", {"root": {"level1": {"level2": []}}})
    nested = {"nested": {"deep": {"data": [1, 2, 3], "more": {"key": "value"}}}}
    binary = {"binary": b"\x00\x01\x02\x03\xff\xfe\xfd"}
    nones = {"key": None, "list": [None, 1, None]}
    for data in (nested, binary, nones):
        ref = patch_object(ref, data)

    assert get_object(ref, deserializer=_parts)[1:] == [nested, binary, nones]


def test_many_appends():
    ref = put_object(f"test-app/test-many-{uuid.uuid4().hex[:8]}", {"counter": 0})
    for i in range(50):
        ref = patch_object(ref, {"increment": i + 1})

    assert ref.version == 51
    parts = get_object(ref, deserializer=_parts)
    assert [part.get("increment") for part in parts[1:]] == list(range(1, 51))


def test_patch_rejects_data_of_another_type_and_changes_nothing():
    ref = put_object(f"test-app/test-type-{uuid.uuid4().hex[:8]}", {"kind": "pickle"})

    with pytest.raises(Exception):
        patch_object(ref, pa.table({"a": [1]}))

    assert _remote_metadata(ref).version == 1
    assert get_object(ref, deserializer=_parts) == [{"kind": "pickle"}]


def test_patch_nonexistent_object():
    """Appending to an object that does not exist is an error."""
    ctx = FlameContext()
    cache_config = ctx.cache
    cache_endpoint = cache_config.get("endpoint") if isinstance(cache_config, dict) else cache_config

    fake_ref = ObjectRef(endpoint=cache_endpoint, key="test-app/nonexistent-session/nonexistent-object", version=0)

    with pytest.raises(Exception):
        patch_object(fake_ref, {"data": "data"})


def test_delete_objects_removes_the_object():
    ref = put_object(f"test-app/test-delete-{uuid.uuid4().hex[:8]}", {"first": True})
    ref = patch_object(ref, {"second": True})
    assert get_object(ref, deserializer=_parts) == [{"first": True}, {"second": True}]

    delete_objects(ref.key)

    with pytest.raises(Exception):
        get_object(ObjectRef(endpoint=ref.endpoint, key=ref.key, version=0))


def test_put_after_delete_creates_a_new_object_at_version_one():
    prefix = f"test-app/test-recreate-{uuid.uuid4().hex[:8]}"
    ref = put_object(prefix, {"first": True})
    ref = patch_object(ref, {"second": True})
    delete_objects(ref.key)

    new_ref = put_object(prefix, {"new": True})

    assert new_ref.key != ref.key
    assert new_ref.version == 1
    assert get_object(new_ref, deserializer=_parts) == [{"new": True}]


@pytest.mark.skip(reason="MAX_DELTAS_PER_OBJECT=1000 is too slow for E2E; tested via unit tests")
def test_patch_limit():
    """Appending beyond the limit of 1000 parts raises an error."""
    pass


# ============================================================================
# Application packages
# ============================================================================


def test_package_upload_download_replace_and_delete(tmp_path):
    app_name = f"test-pkg-{uuid.uuid4().hex[:8]}"
    filename = "app.tar.gz"
    source = tmp_path / filename
    source.write_bytes(b"first package" * 1000)

    ref = upload_package(app_name, str(source))
    assert ref.key == f"{app_name}/pkg/{filename}"
    assert ref.version == 1

    downloaded = tmp_path / "downloaded.tar.gz"
    download_package(app_name, filename, str(downloaded))
    assert downloaded.read_bytes() == source.read_bytes()

    # A package with the same name is replaced.
    source.write_bytes(b"second package" * 1000)
    replaced = upload_package(app_name, str(source))
    assert replaced.key == ref.key
    assert replaced.version == 1
    download_package(app_name, filename, str(downloaded))
    assert downloaded.read_bytes() == source.read_bytes()

    # A name can be given for the package, which is how it is kept.
    renamed = upload_package(app_name, str(source), "other.tar.gz")
    assert renamed.key == f"{app_name}/pkg/other.tar.gz"

    delete_package(app_name, filename)
    with pytest.raises(Exception):
        download_package(app_name, filename, str(tmp_path / "gone.tar.gz"))
    download_package(app_name, "other.tar.gz", str(downloaded))
    assert downloaded.read_bytes() == source.read_bytes()
    delete_package(app_name, "other.tar.gz")


def test_package_with_appended_data_cannot_be_downloaded(tmp_path):
    """A package is a file: an object with more than one part is not."""
    app_name = f"test-pkg-{uuid.uuid4().hex[:8]}"
    source = tmp_path / "app.tar.gz"
    source.write_bytes(b"package")
    ref = upload_package(app_name, str(source))
    # A package has the raw type, so the data appended must be raw too.
    _remote_write("Patch", ref, "raw", b"appended")
    assert _remote_metadata(ref).version == 2

    with pytest.raises(Exception):
        download_package(app_name, "app.tar.gz", str(tmp_path / "out.tar.gz"))
    assert not (tmp_path / "out.tar.gz").exists()
    delete_package(app_name, "app.tar.gz")


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

    def test_object_patch_remains_retrievable_after_more_writes(self):
        """Test that a patched object remains retrievable after more writes."""
        session_id = generate_session_id()

        refs = []
        for i in range(5):
            obj = {"sequence": i, "version": 1}
            ref = put_object(session_id, obj)
            refs.append(ref)

        patched_obj = {"sequence": 0, "version": 2}
        refs[0] = patch_object(refs[0], patched_obj)

        for i in range(5, 15):
            obj = create_large_object(size_kb=100)
            obj["sequence"] = i
            put_object(session_id, obj)

        retrieved = get_object(refs[0])
        assert retrieved["sequence"] == 0
        assert retrieved["version"] == 2, "Patched object should have the patch's data"

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

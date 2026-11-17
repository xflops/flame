import asyncio
import os
import subprocess
import sys
import textwrap
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pyarrow as pa
import pytest

from flamepy.core import cache as cache_module
from flamepy.core.cache import (
    _TYPE_ARROW_ARRAY,
    _TYPE_ARROW_BATCH,
    _TYPE_ARROW_TABLE,
    _TYPE_CLOUDPICKLE,
    _TYPE_NUMPY,
    _TYPE_PANDAS_DATAFRAME,
    _TYPE_POLARS_DATAFRAME,
    FetchResult,
    Object,
    ObjectKey,
    ObjectRef,
    _cache_lock,
    _deserialize_object_data,
    _materialize_object,
    _serialize_object_data,
)


class TestSerialization:
    @pytest.mark.parametrize("original", [{"nested": {"a": 1}}, [1, 2, 3], "text", 42, None])
    def test_cloudpickle_roundtrip(self, original):
        data_type, payload = _serialize_object_data(original)
        assert data_type == _TYPE_CLOUDPICKLE
        assert _deserialize_object_data(data_type, payload) == original
        assert not payload.startswith(b"FLM")

    def test_numpy_array_roundtrip(self):
        array = np.array([[1.0, 2.0], [3.0, 4.0]])
        data_type, payload = _serialize_object_data(array)
        assert data_type == _TYPE_NUMPY
        np.testing.assert_array_equal(_deserialize_object_data(data_type, payload), array)

    def test_non_contiguous_numpy_uses_cloudpickle(self):
        array = np.arange(9).reshape(3, 3)[:, ::2]
        data_type, payload = _serialize_object_data(array)
        assert data_type == _TYPE_CLOUDPICKLE
        np.testing.assert_array_equal(_deserialize_object_data(data_type, payload), array)

    def test_arrow_table_batch_array_roundtrip(self):
        values = [(pa.table({"x": [1, 2]}), _TYPE_ARROW_TABLE), (pa.RecordBatch.from_pydict({"x": [1, 2]}), _TYPE_ARROW_BATCH), (pa.array([1, 2]), _TYPE_ARROW_ARRAY)]
        for value, expected_type in values:
            data_type, payload = _serialize_object_data(value)
            assert data_type == expected_type
            assert _deserialize_object_data(data_type, payload).equals(value)

    def test_pandas_dataframe_roundtrip(self):
        from flamepy.core import cache as cache_module

        pd = pytest.importorskip("pandas")
        frame = pd.DataFrame({"x": [1, 2]}, index=pd.Index([4, 5], name="row"))
        data_type, payload = _serialize_object_data(frame)
        assert data_type == _TYPE_PANDAS_DATAFRAME
        pd.testing.assert_frame_equal(_deserialize_object_data(data_type, payload), frame)
        wire_type, wire_payload = cache_module._encode_object_data(frame)
        assert wire_type == f"{_TYPE_PANDAS_DATAFRAME}.zstd"
        pd.testing.assert_frame_equal(_deserialize_object_data(data_type, cache_module._decompress_data(wire_payload, "zstd")), frame)

    def test_polars_dataframe_and_lazyframe_roundtrip(self):
        from flamepy.core import cache as cache_module

        pl = pytest.importorskip("polars")
        frame = pl.DataFrame({"x": [1, 2]})
        for value in (frame, frame.lazy()):
            data_type, payload = _serialize_object_data(value)
            assert data_type == _TYPE_POLARS_DATAFRAME
            assert _deserialize_object_data(data_type, payload).equals(frame)
            wire_type, wire_payload = cache_module._encode_object_data(value)
            assert wire_type == f"{_TYPE_POLARS_DATAFRAME}.zstd"
            assert _deserialize_object_data(data_type, cache_module._decompress_data(wire_payload, "zstd")).equals(frame)

    def test_optional_torch_tensor_is_compressed_without_importing_torch(self, monkeypatch):
        import sys

        from flamepy.core import cache as cache_module

        class Tensor:
            def __init__(self, value):
                self.value = value

        monkeypatch.setitem(sys.modules, "torch", SimpleNamespace(Tensor=Tensor))
        wire_type, wire_payload = cache_module._encode_object_data(Tensor(7))
        assert wire_type == "cloudpickle.zstd"
        assert cache_module._deserialize_object_data(_TYPE_CLOUDPICKLE, cache_module._decompress_data(wire_payload, "zstd")).value == 7

    def test_unknown_data_type_is_rejected(self):
        with pytest.raises(ValueError, match="Unsupported cache data type"):
            _deserialize_object_data("unknown", b"data")

    @pytest.mark.parametrize(
        "value,expected_type",
        [
            (np.zeros(10000, dtype=np.int64), _TYPE_NUMPY),
            (pa.table({"x": ["repeat" * 20] * 1000}), _TYPE_ARROW_TABLE),
            (pa.RecordBatch.from_pydict({"x": ["repeat" * 20] * 1000}), _TYPE_ARROW_BATCH),
            (pa.array(["repeat" * 20] * 1000), _TYPE_ARROW_ARRAY),
        ],
    )
    def test_auto_compression_preserves_arrow_and_numpy_types(self, value, expected_type):
        from flamepy.core import cache as cache_module

        data_type, data = cache_module._encode_object_data(value)
        assert data_type == f"{expected_type}.zstd"
        decoded = _deserialize_object_data(expected_type, cache_module._decompress_data(data, "zstd"))
        if isinstance(value, np.ndarray):
            np.testing.assert_array_equal(decoded, value)
        else:
            assert decoded.equals(value)


class TestSyncCacheFacade:
    def test_sync_facade_uses_aio(self, monkeypatch):
        from flamepy.core.aio import cache as aio_cache

        async def fake_put(prefix, obj):
            await asyncio.sleep(0)
            return ObjectRef("grpc://cache:1", prefix + "/key", 1)

        monkeypatch.setattr(aio_cache, "put_object", fake_put)
        assert cache_module.put_object("app/session", 42).key == "app/session/key"
        cache_module._close_aio_bridge()


class TestMaterializedCache:
    def test_deserializer_hash_and_equality_do_not_run_under_cache_lock(self):
        class ReentrantDeserializer:
            def __init__(self, suffix):
                self.suffix = suffix

            def __call__(self, parts):
                return parts[0] + self.suffix

            def _check_lock(self):
                assert _cache_lock.acquire(blocking=False), "deserializer method called under cache lock"
                _cache_lock.release()

            def __hash__(self):
                self._check_lock()
                return 1

            def __eq__(self, other):
                self._check_lock()
                return isinstance(other, ReentrantDeserializer) and self.suffix == other.suffix

        obj = Object(version=1, data=["first"])
        first = ReentrantDeserializer("-first")
        second = ReentrantDeserializer("-second")

        assert _materialize_object(obj, first) == "first-first"
        assert _materialize_object(obj, first) == "first-first"
        assert _materialize_object(obj, second) == "first-second"


class TestMaterializedObject:
    def test_value_is_the_newest_part_without_a_deserializer(self):
        obj = Object(version=3, data=["first", "second", "third"])
        assert _materialize_object(obj) == "third"

    def test_deserializer_gets_every_part_in_order(self):
        obj = Object(version=3, data=[[1], [2, 3], [4]])
        seen = []

        def merge(parts):
            seen.append(parts)
            return [item for part in parts for item in part]

        assert _materialize_object(obj, merge) == [1, 2, 3, 4]
        assert seen == [[[1], [2, 3], [4]]]
        # The value is kept until the object changes.
        assert _materialize_object(obj, merge) == [1, 2, 3, 4]
        assert len(seen) == 1


class TestAppendParts:
    KEY = ("grpc://cache:1", "app/session/parts")

    def setup_method(self):
        cache_module._cache_remove(self.KEY)

    def test_the_object_is_created_from_the_data_of_version_zero(self):
        cached = cache_module._cache_append_parts(self.KEY, FetchResult(2, ["first", "second"]))

        assert (cached.version, cached.data) == (2, ["first", "second"])
        assert cache_module._cache_get(self.KEY) is cached

    def test_data_after_a_version_cannot_create_the_object(self):
        assert cache_module._cache_append_parts(self.KEY, FetchResult(3, ["third"]), 2) is None
        assert cache_module._cache_get(self.KEY) is None

    def test_data_after_the_cached_version_is_appended(self):
        cache_module._cache_append_parts(self.KEY, FetchResult(1, ["first"]))

        cached = cache_module._cache_append_parts(self.KEY, FetchResult(3, ["second", "third"]), 1)

        assert (cached.version, cached.data) == (3, ["first", "second", "third"])

    def test_appending_drops_the_values_made_of_the_old_parts(self):
        cached = cache_module._cache_append_parts(self.KEY, FetchResult(1, ["first"]))
        assert _materialize_object(cached, lambda parts: list(parts)) == ["first"]

        cache_module._cache_append_parts(self.KEY, FetchResult(2, ["second"]), 1)

        assert _materialize_object(cached, lambda parts: list(parts)) == ["first", "second"]

    def test_parts_the_cache_got_after_the_request_are_not_added_twice(self):
        # Asked for after version 1, while another call took the cache to version 2.
        cache_module._cache_append_parts(self.KEY, FetchResult(2, ["first", "second"]))

        cached = cache_module._cache_append_parts(self.KEY, FetchResult(3, ["second", "third"]), 1)

        assert (cached.version, cached.data) == (3, ["first", "second", "third"])

    def test_a_result_the_cache_already_has_leaves_it_as_it_is(self):
        cache_module._cache_append_parts(self.KEY, FetchResult(2, ["first", "second"]))

        same = cache_module._cache_append_parts(self.KEY, FetchResult(2, ["other"]), 1)
        older = cache_module._cache_append_parts(self.KEY, FetchResult(1, ["older"]))

        assert same is older
        assert (same.version, same.data) == (2, ["first", "second"])

    def test_a_cache_behind_the_request_is_refused(self):
        cache_module._cache_append_parts(self.KEY, FetchResult(1, ["first"]))

        # Asked for after version 3 but the cache only has version 1.
        assert cache_module._cache_append_parts(self.KEY, FetchResult(4, ["fourth"]), 3) is None
        assert cache_module._cache_get(self.KEY).data == ["first"]


class TestObjectKey:
    def test_from_prefix_valid(self):
        key = ObjectKey.from_prefix("myapp/pkg")
        assert key.app_name == "myapp"
        assert key.session_id == "pkg"
        assert key.object_id is None

    def test_from_key_valid(self):
        key = ObjectKey.from_key("myapp/pkg/file.tar.gz")
        assert key.app_name == "myapp"
        assert key.session_id == "pkg"
        assert key.object_id == "file.tar.gz"

    def test_from_path_accepts_prefix_and_full_key(self):
        prefix = ObjectKey.from_path("myapp/pkg")
        full_key = ObjectKey.from_path("myapp/pkg/file.tar.gz")

        assert prefix == ObjectKey(app_name="myapp", session_id="pkg")
        assert full_key == ObjectKey(app_name="myapp", session_id="pkg", object_id="file.tar.gz")

    def test_from_prefix_invalid(self):
        with pytest.raises(ValueError):
            ObjectKey.from_prefix("invalid")

        with pytest.raises(ValueError):
            ObjectKey.from_prefix("a/b/c")

    def test_from_key_invalid(self):
        with pytest.raises(ValueError):
            ObjectKey.from_key("a/b")

        with pytest.raises(ValueError):
            ObjectKey.from_key("a/b/c/d")

    @pytest.mark.parametrize(
        "path",
        [
            "/session",
            "app/",
            "../session",
            "app/../obj",
            "app/session/",
            "*/session",
            "app/*/obj",
            "app/session/*",
        ],
    )
    def test_from_path_rejects_invalid_components(self, path):
        with pytest.raises(ValueError):
            ObjectKey.from_path(path)

    def test_matches_key(self):
        all_sessions = ObjectKey.for_all_sessions("myapp")
        session = ObjectKey.from_prefix("myapp/session")
        exact = ObjectKey.from_key("myapp/session/obj1")

        assert all_sessions.matches_key("myapp/session/obj1")
        assert all_sessions.matches_key("myapp/other/obj2")
        assert not all_sessions.matches_key("other/session/obj1")
        assert session.matches_key("myapp/session/obj1")
        assert not session.matches_key("myapp/session2/obj1")
        assert exact.matches_key("myapp/session/obj1")
        assert not exact.matches_key("myapp/session/obj2")
        assert not all_sessions.matches_key("myapp")
        assert not all_sessions.matches_key("myapp/session")
        assert not all_sessions.matches_key("myapp/session/obj1/extra")
        assert not session.matches_key("myapp/session")
        assert not session.matches_key("myapp/session/obj1/extra")


@pytest.mark.parametrize(
    "value",
    [
        np.array([1, 2], dtype=np.dtype("i4").newbyteorder("S")),
        np.array([True, False]),
        np.array(["alpha", "beta"]),
        np.array([{"nested": 1}], dtype=object),
        np.ma.array([1, 2], mask=[False, True]),
        np.array([1, 2], dtype=np.dtype("i4", metadata={"unit": "meters"})),
    ],
    ids=["non-native-endian", "boolean", "strings", "objects", "masked-subclass", "dtype-metadata"],
)
def test_numpy_fallback_preserves_values_types_and_metadata(value):
    wire_type, payload = cache_module._encode_object_data(value)
    data_type, compression = cache_module._type_and_compression(wire_type)
    restored = _deserialize_object_data(data_type, cache_module._decompress_data(payload, compression))
    assert data_type == _TYPE_CLOUDPICKLE
    assert type(restored) is type(value)
    assert restored.dtype == value.dtype
    assert restored.dtype.metadata == value.dtype.metadata
    np.testing.assert_array_equal(restored, value)
    if isinstance(value, np.ma.MaskedArray):
        np.testing.assert_array_equal(restored.mask, value.mask)


def _run_isolated_cache_probe(code):
    src = str(Path(__file__).resolve().parents[1] / "src")
    script = f"import sys\nsys.path.insert(0, {src!r})\n" + textwrap.dedent(code)
    return subprocess.run([sys.executable, "-I", "-c", script], capture_output=True, text=True, check=True, timeout=15)


def test_nested_sync_deserializers_do_not_occupy_the_transport_executor():
    _run_isolated_cache_probe("""
        import asyncio
        import concurrent.futures
        import threading
        from flamepy.core import cache
        from flamepy.core.aio import cache as aio
        async def unchanged(*args): return None
        aio._fetch_object_data = unchanged
        bridge = cache._get_aio_bridge()
        async def capacity():
            await asyncio.to_thread(lambda: None)
            return asyncio.get_running_loop()._default_executor._max_workers
        count = bridge.call(capacity())
        refs = [cache.ObjectRef('grpc://cache:1', 'app/session/' + name, 1) for name in ('outer', 'inner')]
        for ref in refs:
            cache._cache_append_parts((ref.endpoint, ref.key), cache.FetchResult(1, ['value'], uuid='object'))
        ready = threading.Barrier(count)
        def deserialize(parts):
            ready.wait(timeout=5)
            return cache.get_object(refs[1])
        with concurrent.futures.ThreadPoolExecutor(max_workers=count) as callers:
            futures = [callers.submit(cache.get_object, refs[0], deserialize) for _ in range(count)]
            assert [future.result(timeout=5) for future in futures] == ['value'] * count
        cache._close_aio_bridge()
    """)


@pytest.mark.skipif(not hasattr(os, "fork"), reason="fork is unavailable")
def test_fork_resets_all_cache_locks_and_inherited_state():
    _run_isolated_cache_probe("""
        import os
        import signal
        from flamepy.core import cache
        cache._cache_append_parts(('endpoint', 'key'), cache.FetchResult(1, ['value'], uuid='object'))
        cache._context_cache = object()
        with cache._cache_lock, cache._context_cache_lock, cache._aio_bridge_lock:
            child = os.fork()
            if child == 0:
                signal.alarm(3)
                assert cache._cache_get(('endpoint', 'key')) is None
                assert not cache._cache_tokens
                assert cache._context_cache is None
                assert cache._context_cache_lock.acquire(blocking=False)
                assert cache._aio_bridge_lock.acquire(blocking=False)
                os._exit(0)
            _, status = os.waitpid(child, 0)
        assert status == 0, status
        assert cache._cache_get(('endpoint', 'key')).data == ['value']
    """)

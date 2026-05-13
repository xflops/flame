import threading

import bson
import numpy as np
import pyarrow as pa
import pytest

from flamepy.core.cache import (
    _MAGIC_PREFIX_LEN,
    _TYPE_ARROW_ARRAY,
    _TYPE_ARROW_BATCH,
    _TYPE_ARROW_TABLE,
    _TYPE_CLOUDPICKLE,
    _TYPE_NUMPY,
    OBJECT_FIELD_DATA,
    OBJECT_FIELD_VERSION,
    OBJECT_RESPONSE_FIELD_KIND,
    FetchMode,
    FetchResult,
    Object,
    ObjectKey,
    ObjectRef,
    ObjectResponseKind,
    Patch,
    _cache_lock,
    _deserialize_object,
    _deserialize_object_data,
    _object_cache,
    _serialize_object,
    _serialize_object_data,
    delete_objects,
)


class TestSerialization:
    def test_serialize_deserialize_roundtrip(self):
        original = {"key": "value", "number": 42, "nested": {"a": 1}}
        batch = _serialize_object(original)

        assert batch.num_rows == 1
        assert batch.num_columns == 2
        assert batch.schema.names == ["version", "data"]

        result = _deserialize_object(batch)
        assert result == original

    def test_serialize_handles_various_types(self):
        test_cases = [
            [1, 2, 3],
            {"nested": {"deep": {"value": True}}},
            "simple string",
            42,
            3.14159,
            None,
        ]

        for original in test_cases:
            batch = _serialize_object(original)
            result = _deserialize_object(batch)
            assert result == original


class TestFastPathSerialization:
    def test_numpy_array_uses_fast_path(self):
        arr = np.array([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]], dtype=np.float64)
        data = _serialize_object_data(arr)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_NUMPY
        result = _deserialize_object_data(data)
        np.testing.assert_array_equal(result, arr)

    def test_numpy_array_various_dtypes(self):
        test_cases = [
            np.array([1, 2, 3], dtype=np.int32),
            np.array([1.5, 2.5, 3.5], dtype=np.float32),
            np.array([1, 2, 3], dtype=np.int64),
            np.array([[1, 2], [3, 4]], dtype=np.float64),
            np.zeros((10, 10, 3), dtype=np.uint8),
        ]

        for original in test_cases:
            data = _serialize_object_data(original)
            assert data[:_MAGIC_PREFIX_LEN] == _TYPE_NUMPY
            result = _deserialize_object_data(data)
            np.testing.assert_array_equal(result, original)
            assert result.dtype == original.dtype

    def test_numpy_large_array_performance(self):
        import time

        large_arr = np.random.rand(1000, 1000)

        start = time.perf_counter()
        data = _serialize_object_data(large_arr)
        serialize_time = time.perf_counter() - start

        start = time.perf_counter()
        result = _deserialize_object_data(data)
        deserialize_time = time.perf_counter() - start

        np.testing.assert_array_almost_equal(result, large_arr)
        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_NUMPY
        assert serialize_time < 0.5
        assert deserialize_time < 0.5

    def test_pyarrow_table_uses_fast_path(self):
        table = pa.table({"col1": [1, 2, 3], "col2": ["a", "b", "c"]})
        data = _serialize_object_data(table)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_ARROW_TABLE
        result = _deserialize_object_data(data)
        assert result.equals(table)

    def test_pyarrow_record_batch_uses_fast_path(self):
        batch = pa.RecordBatch.from_pydict({"x": [1, 2, 3], "y": [4.0, 5.0, 6.0]})
        data = _serialize_object_data(batch)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_ARROW_BATCH
        result = _deserialize_object_data(data)
        assert result.equals(batch)

    def test_pyarrow_array_uses_fast_path(self):
        arr = pa.array([1, 2, 3, 4, 5])
        data = _serialize_object_data(arr)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_ARROW_ARRAY
        result = _deserialize_object_data(data)
        assert result.equals(arr)

    def test_pyarrow_chunked_array_uses_cloudpickle(self):
        chunked = pa.chunked_array([[1, 2], [3, 4]])
        data = _serialize_object_data(chunked)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_CLOUDPICKLE
        result = _deserialize_object_data(data)
        assert result.equals(chunked)

    def test_dict_uses_cloudpickle(self):
        obj = {"key": "value", "number": 42}
        data = _serialize_object_data(obj)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_CLOUDPICKLE
        result = _deserialize_object_data(data)
        assert result == obj

    def test_list_uses_cloudpickle(self):
        obj = [1, 2, 3, "mixed", {"nested": True}]
        data = _serialize_object_data(obj)

        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_CLOUDPICKLE
        result = _deserialize_object_data(data)
        assert result == obj

    def test_full_roundtrip_via_record_batch(self):
        test_cases = [
            np.array([1.0, 2.0, 3.0]),
            pa.table({"a": [1, 2, 3]}),
            pa.array([10, 20, 30]),
            {"python": "dict"},
            [1, 2, 3],
        ]

        for original in test_cases:
            batch = _serialize_object(original)
            result = _deserialize_object(batch)

            if isinstance(original, np.ndarray):
                np.testing.assert_array_equal(result, original)
            elif isinstance(original, (pa.Table, pa.Array)):
                assert result.equals(original)
            else:
                assert result == original

    def test_non_contiguous_numpy_array_uses_cloudpickle(self):
        arr = np.array([[1, 2, 3], [4, 5, 6]])
        non_contiguous = arr[:, ::2]
        assert not non_contiguous.flags.c_contiguous
        assert not non_contiguous.flags.f_contiguous

        data = _serialize_object_data(non_contiguous)
        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_CLOUDPICKLE

        result = _deserialize_object_data(data)
        np.testing.assert_array_equal(result, non_contiguous)

    def test_fortran_contiguous_array_uses_fast_path(self):
        arr = np.asfortranarray([[1, 2], [3, 4], [5, 6]])
        assert arr.flags.f_contiguous

        data = _serialize_object_data(arr)
        assert data[:_MAGIC_PREFIX_LEN] == _TYPE_NUMPY

        result = _deserialize_object_data(data)
        np.testing.assert_array_equal(result, arr)


class TestClientSideCaching:
    def setup_method(self):
        with _cache_lock:
            _object_cache.clear()

    def teardown_method(self):
        with _cache_lock:
            _object_cache.clear()

    def _response_table(self, rows):
        return pa.table(
            {
                OBJECT_FIELD_VERSION: pa.array([row[0] for row in rows], type=pa.uint64()),
                OBJECT_RESPONSE_FIELD_KIND: pa.array(
                    [row[1].value for row in rows],
                    type=pa.string(),
                ),
                OBJECT_FIELD_DATA: pa.array(
                    [_serialize_object_data(row[2]) for row in rows],
                    type=pa.binary(),
                ),
            }
        )

    def _patch_fetch_client(self, monkeypatch, table):
        from flamepy.core import cache as cache_module

        class FakeReader:
            def read_all(self):
                return table

        class FakeClient:
            def do_get(self, ticket):
                self.ticket = ticket
                return FakeReader()

        fake_client = FakeClient()
        monkeypatch.setattr(cache_module, "_get_cache_tls_config", lambda: None)
        monkeypatch.setattr(
            cache_module,
            "_get_flight_client",
            lambda endpoint, tls_config: fake_client,
        )
        return fake_client

    def test_cache_hit_returns_cached_data(self, monkeypatch):
        from flamepy.core import cache as cache_module

        base_data = {"from": "cache"}
        cache_key = ("grpc://host:9090", "app/session/obj1")
        cached_obj = Object(version=5, data=base_data)

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        call_count = {"server": 0}

        def mock_fetch_object_data(ref, cached_version):
            call_count["server"] += 1
            return None

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj1", version=5)
        result = cache_module.get_object(ref)

        assert result == base_data
        assert call_count["server"] == 1

    def test_cache_miss_fetches_from_server(self, monkeypatch):
        from flamepy.core import cache as cache_module

        server_data = {"from": "server"}

        def mock_fetch_object_data(ref, cached_version):
            return FetchResult(mode=FetchMode.FULL, version=1, base=server_data)

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj2", version=0)
        result = cache_module.get_object(ref)

        assert result == server_data

        cache_key = ("grpc://host:9090", "app/session/obj2")
        with _cache_lock:
            assert cache_key in _object_cache
            assert _object_cache[cache_key].version == 1
            assert _object_cache[cache_key].data == server_data

    def test_version_mismatch_triggers_download(self, monkeypatch):
        from flamepy.core import cache as cache_module

        old_data = {"old": "data"}
        cache_key = ("grpc://host:9090", "app/session/obj3")
        cached_obj = Object(version=1, data=old_data)

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        new_data = {"new": "data"}

        def mock_fetch_object_data(ref, cached_version):
            return FetchResult(mode=FetchMode.FULL, version=2, base=new_data)

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj3", version=1)
        result = cache_module.get_object(ref)

        assert result == new_data
        with _cache_lock:
            assert _object_cache[cache_key].version == 2
            assert _object_cache[cache_key].data == new_data

    def test_patch_only_response_appends_to_cached_data(self, monkeypatch):
        from flamepy.core import cache as cache_module

        cache_key = ("grpc://host:9090", "app/session/obj-patch")
        cached_obj = Object(version=1, data=[1])

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        def mock_fetch_object_data(ref, cached_version):
            assert cached_version == 1
            return FetchResult(
                mode=FetchMode.PATCHES,
                version=3,
                patches=[
                    Patch(version=2, data=[2]),
                    Patch(version=3, data=[3]),
                ],
            )

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        def merge_lists(base_data, deltas):
            result = list(base_data)
            for delta in deltas:
                result.extend(delta)
            return result

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj-patch", version=1)
        result = cache_module.get_object(ref, deserializer=merge_lists)

        assert result == [1, 2, 3]
        with _cache_lock:
            cached = _object_cache[cache_key]
            assert cached.version == 3
            assert [patch.version for patch in cached.patches] == [2, 3]

    def test_concurrent_patch_only_fetches_do_not_duplicate_patches(self, monkeypatch):
        from flamepy.core import cache as cache_module

        cache_key = ("grpc://host:9090", "app/session/obj-concurrent-patch")
        cached_obj = Object(version=1, data=[1])

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        barrier = threading.Barrier(2)

        def mock_fetch_object_data(ref, cached_version):
            assert cached_version == 1
            barrier.wait(timeout=5)
            return FetchResult(
                mode=FetchMode.PATCHES,
                version=2,
                patches=[Patch(version=2, data=[2])],
            )

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        def merge_lists(base_data, deltas):
            result = list(base_data)
            for delta in deltas:
                result.extend(delta)
            return result

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj-concurrent-patch", version=1)
        results = []
        errors = []

        def worker():
            try:
                results.append(cache_module.get_object(ref, deserializer=merge_lists))
            except Exception as exc:
                errors.append(exc)

        threads = [threading.Thread(target=worker) for _ in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()

        assert errors == []
        assert results == [[1, 2], [1, 2]]
        with _cache_lock:
            cached = _object_cache[cache_key]
            assert cached.version == 2
            assert [patch.version for patch in cached.patches] == [2]

    def test_patch_only_response_without_cache_falls_back_to_full_fetch(self, monkeypatch):
        from flamepy.core import cache as cache_module

        calls = []

        def mock_fetch_object_data(ref, cached_version):
            calls.append(cached_version)
            if len(calls) == 1:
                return FetchResult(
                    mode=FetchMode.PATCHES,
                    version=2,
                    patches=[Patch(version=2, data=[2])],
                )
            return FetchResult(
                mode=FetchMode.FULL,
                version=2,
                base=[1],
                patches=[Patch(version=2, data=[2])],
            )

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        def merge_lists(base_data, deltas):
            result = list(base_data)
            for delta in deltas:
                result.extend(delta)
            return result

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj-patch-miss", version=2)
        result = cache_module.get_object(ref, deserializer=merge_lists)

        assert calls == [0, 0]
        assert result == [1, 2]
        with _cache_lock:
            cached = _object_cache[("grpc://host:9090", "app/session/obj-patch-miss")]
            assert cached.version == 2
            assert [patch.data for patch in cached.patches] == [[2]]

    def test_not_modified_reuses_bound_method_materialized_result(self, monkeypatch):
        from flamepy.core import cache as cache_module

        cache_key = ("grpc://host:9090", "app/session/obj-not-modified")
        cached_obj = Object(
            version=2,
            data=[1],
            patches=[Patch(version=2, data=[2])],
        )

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        fetch_calls = {"count": 0}

        def mock_fetch_object_data(ref, cached_version):
            fetch_calls["count"] += 1
            assert cached_version == 2
            return None

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        class Merger:
            def __init__(self):
                self.calls = 0

            def merge_lists(self, base_data, deltas):
                self.calls += 1
                result = list(base_data)
                for delta in deltas:
                    result.extend(delta)
                return result

        merger = Merger()

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj-not-modified", version=2)

        assert cache_module.get_object(ref, deserializer=merger.merge_lists) == [1, 2]
        assert cache_module.get_object(ref, deserializer=merger.merge_lists) == [1, 2]
        assert fetch_calls["count"] == 2
        assert merger.calls == 1
        assert len(cached_obj.materialized) == 1

    def test_not_modified_accepts_unhashable_callable_deserializer(self, monkeypatch):
        from flamepy.core import cache as cache_module

        cache_key = ("grpc://host:9090", "app/session/obj-unhashable")
        cached_obj = Object(
            version=2,
            data=[1],
            patches=[Patch(version=2, data=[2])],
        )

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        def mock_fetch_object_data(ref, cached_version):
            assert cached_version == 2
            return None

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        class UnhashableMerger:
            def __init__(self):
                self.calls = 0

            def __eq__(self, other):
                return self is other

            def __call__(self, base_data, deltas):
                self.calls += 1
                result = list(base_data)
                for delta in deltas:
                    result.extend(delta)
                return result

        merger = UnhashableMerger()
        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj-unhashable", version=2)

        assert cache_module.get_object(ref, deserializer=merger) == [1, 2]
        assert cache_module.get_object(ref, deserializer=merger) == [1, 2]
        assert merger.calls == 1

    def test_version_zero_bypasses_cache(self, monkeypatch):
        from flamepy.core import cache as cache_module

        cached_data = {"cached": "data"}
        cache_key = ("grpc://host:9090", "app/session/obj4")
        cached_obj = Object(version=5, data=cached_data)

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        server_data = {"fresh": "data"}

        def mock_fetch_object_data(ref, cached_version):
            assert cached_version == 0
            return FetchResult(mode=FetchMode.FULL, version=6, base=server_data)

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj4", version=0)
        result = cache_module.get_object(ref)

        assert result == server_data

    def test_deserializer_combines_base_and_deltas(self, monkeypatch):
        from flamepy.core import cache as cache_module

        def mock_fetch_object_data(ref, cached_version):
            return FetchResult(
                mode=FetchMode.FULL,
                version=3,
                base=[1, 2, 3],
                patches=[
                    Patch(version=2, data=[4, 5]),
                    Patch(version=3, data=[6]),
                ],
            )

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        def merge_lists(base_data, deltas):
            result = list(base_data)
            for d in deltas:
                result.extend(d)
            return result

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj5", version=0)
        result = cache_module.get_object(ref, deserializer=merge_lists)

        assert result == [1, 2, 3, 4, 5, 6]

    def test_fetch_object_data_parses_full_response_rows(self, monkeypatch):
        from flamepy.core import cache as cache_module

        table = self._response_table(
            [
                (1, ObjectResponseKind.BASE, [1]),
                (2, ObjectResponseKind.PATCH, [2]),
                (3, ObjectResponseKind.PATCH, [3]),
            ]
        )
        self._patch_fetch_client(monkeypatch, table)

        result = cache_module._fetch_object_data(
            ObjectRef(endpoint="grpc://host:9090", key="app/session/obj6", version=1),
            0,
        )

        assert result.mode == FetchMode.FULL
        assert result.version == 3
        assert result.base == [1]
        assert [patch.data for patch in result.patches] == [[2], [3]]

    def test_fetch_object_data_rejects_base_after_patch(self, monkeypatch):
        from flamepy.core import cache as cache_module

        table = self._response_table(
            [
                (2, ObjectResponseKind.PATCH, [2]),
                (1, ObjectResponseKind.BASE, [1]),
            ]
        )
        self._patch_fetch_client(monkeypatch, table)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj7", version=1)
        try:
            cache_module._fetch_object_data(ref, 1)
        except ValueError as exc:
            assert "base row" in str(exc)
        else:
            raise AssertionError("expected ValueError for malformed full response")

    def test_thread_safety(self, monkeypatch):
        from flamepy.core import cache as cache_module

        results = []
        errors = []

        def mock_fetch_object_data(ref, cached_version):
            return FetchResult(mode=FetchMode.FULL, version=1, base={"thread": ref.key})

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        def worker(i):
            try:
                ref = ObjectRef(
                    endpoint="grpc://host:9090",
                    key=f"app/session/thread-{i}",
                    version=0,
                )
                result = cache_module.get_object(ref)
                results.append(result)
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(10)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert len(errors) == 0
        assert len(results) == 10


class TestDeleteObjects:
    def setup_method(self):
        with _cache_lock:
            _object_cache.clear()

    def teardown_method(self):
        with _cache_lock:
            _object_cache.clear()

    def test_delete_objects_clears_client_cache(self, monkeypatch):
        from flamepy.core import cache as cache_module
        from flamepy.core.types import FlameClientCache

        cache_key1 = ("grpc://host:9090", "myapp/session1/obj1")
        cache_key2 = ("grpc://host:9090", "myapp/session1/obj2")
        cache_key3 = ("grpc://host:9090", "myapp/session2/obj1")
        cache_key4 = ("grpc://host:9090", "other/session/obj1")

        with _cache_lock:
            _object_cache[cache_key1] = Object(version=1, data="data1")
            _object_cache[cache_key2] = Object(version=1, data="data2")
            _object_cache[cache_key3] = Object(version=1, data="data3")
            _object_cache[cache_key4] = Object(version=1, data="data4")

        action_bodies = []

        class MockFlightClient:
            def do_action(self, action):
                action_bodies.append(action.body.to_pybytes().decode("utf-8"))
                return [type("Result", (), {"body": type("Body", (), {"to_pybytes": lambda: b"OK"})()})()]

        class MockContext:
            cache = FlameClientCache(endpoint="grpc://host:9090")

        monkeypatch.setattr(cache_module, "FlameContext", lambda: MockContext())
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        delete_objects("myapp/*")

        with _cache_lock:
            assert cache_key1 not in _object_cache
            assert cache_key2 not in _object_cache
            assert cache_key3 not in _object_cache
            assert cache_key4 in _object_cache

        assert action_bodies == ["myapp/*"]

    def test_delete_objects_clears_session_without_prefix_bleed(self, monkeypatch):
        from flamepy.core import cache as cache_module
        from flamepy.core.types import FlameClientCache

        cache_key1 = ("grpc://host:9090", "myapp/session/obj1")
        cache_key2 = ("grpc://host:9090", "myapp/session2/obj1")
        cache_key3 = ("grpc://host:9090", "myapp/session-extra/obj1")

        with _cache_lock:
            _object_cache[cache_key1] = Object(version=1, data="data1")
            _object_cache[cache_key2] = Object(version=1, data="data2")
            _object_cache[cache_key3] = Object(version=1, data="data3")

        action_bodies = []

        class MockFlightClient:
            def do_action(self, action):
                action_bodies.append(action.body.to_pybytes().decode("utf-8"))
                return [type("Result", (), {"body": type("Body", (), {"to_pybytes": lambda: b"OK"})()})()]

        class MockContext:
            cache = FlameClientCache(endpoint="grpc://host:9090")

        monkeypatch.setattr(cache_module, "FlameContext", lambda: MockContext())
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        delete_objects("myapp/session")

        with _cache_lock:
            assert cache_key1 not in _object_cache
            assert cache_key2 in _object_cache
            assert cache_key3 in _object_cache

        assert action_bodies == ["myapp/session"]

    def test_delete_objects_clears_exact_full_key(self, monkeypatch):
        from flamepy.core import cache as cache_module
        from flamepy.core.types import FlameClientCache

        cache_key1 = ("grpc://host:9090", "myapp/pkg/file1.tar.gz")
        cache_key2 = ("grpc://host:9090", "myapp/pkg/file2.tar.gz")

        with _cache_lock:
            _object_cache[cache_key1] = Object(version=1, data="data1")
            _object_cache[cache_key2] = Object(version=1, data="data2")

        action_bodies = []

        class MockFlightClient:
            def do_action(self, action):
                action_bodies.append(action.body.to_pybytes().decode("utf-8"))
                return [type("Result", (), {"body": type("Body", (), {"to_pybytes": lambda: b"OK"})()})()]

        class MockContext:
            cache = FlameClientCache(endpoint="grpc://host:9090")

        monkeypatch.setattr(cache_module, "FlameContext", lambda: MockContext())
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        delete_objects("myapp/pkg/file1.tar.gz")

        with _cache_lock:
            assert cache_key1 not in _object_cache
            assert cache_key2 in _object_cache

        assert action_bodies == ["myapp/pkg/file1.tar.gz"]

    def test_delete_objects_rejects_invalid_path_before_remote_action(self, monkeypatch):
        from flamepy.core import cache as cache_module

        def fail_get_flight_client(endpoint, tls=None):
            raise AssertionError("remote client should not be created for invalid paths")

        monkeypatch.setattr(cache_module, "_get_flight_client", fail_get_flight_client)

        with pytest.raises(ValueError):
            delete_objects("myapp")

        with pytest.raises(ValueError):
            delete_objects("a/b/c/d")


class TestUploadDownloadObject:
    def test_upload_object_with_full_key(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module
        from flamepy.core.types import FlameClientCache

        test_file = tmp_path / "test.tar.gz"
        test_file.write_bytes(b"test content")

        uploaded_key = None

        class MockWriter:
            def write_batch(self, batch):
                pass

            def done_writing(self):
                pass

            def close(self):
                pass

        class MockReader:
            def __init__(self):
                self._read_count = 0

            def read(self):
                if self._read_count == 0:
                    self._read_count += 1
                    return bson.encode({"endpoint": "grpc://host:9090", "key": "myapp/pkg/test.tar.gz", "version": 1})
                return None

        class MockFlightClient:
            def do_put(self, descriptor, schema, options=None):
                nonlocal uploaded_key
                uploaded_key = "/".join(p.decode() if isinstance(p, bytes) else p for p in descriptor.path)
                return MockWriter(), MockReader()

        class MockContext:
            cache = FlameClientCache(endpoint="grpc://host:9090")

        monkeypatch.setattr(cache_module, "FlameContext", lambda: MockContext())
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        ref = cache_module.upload_object("myapp/pkg/test.tar.gz", str(test_file))

        assert ref.key == "myapp/pkg/test.tar.gz"
        assert ref.endpoint == "grpc://host:9090"
        assert ref.version == 1
        assert uploaded_key == "myapp/pkg/test.tar.gz"

    def test_upload_object_with_prefix(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module
        from flamepy.core.types import FlameClientCache

        test_file = tmp_path / "test.tar.gz"
        test_file.write_bytes(b"test content")

        uploaded_key = None

        class MockWriter:
            def write_batch(self, batch):
                pass

            def done_writing(self):
                pass

            def close(self):
                pass

        class MockReader:
            def __init__(self):
                self._read_count = 0

            def read(self):
                if self._read_count == 0:
                    self._read_count += 1
                    return bson.encode({"endpoint": "grpc://host:9090", "key": "myapp/pkg/generated-uuid", "version": 1})
                return None

        class MockFlightClient:
            def do_put(self, descriptor, schema, options=None):
                nonlocal uploaded_key
                uploaded_key = "/".join(p.decode() if isinstance(p, bytes) else p for p in descriptor.path)
                return MockWriter(), MockReader()

        class MockContext:
            cache = FlameClientCache(endpoint="grpc://host:9090")

        monkeypatch.setattr(cache_module, "FlameContext", lambda: MockContext())
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        ref = cache_module.upload_object("myapp/pkg", str(test_file))

        assert ref.key == "myapp/pkg/generated-uuid"
        assert uploaded_key == "myapp/pkg"

    def test_upload_object_file_not_found(self):
        import pytest

        from flamepy.core import cache as cache_module

        with pytest.raises(FileNotFoundError):
            cache_module.upload_object("myapp/pkg/test.tar.gz", "/nonexistent/file.tar.gz")

    def test_upload_object_invalid_key_format(self, tmp_path):
        from flamepy.core import cache as cache_module

        test_file = tmp_path / "test.tar.gz"
        test_file.write_bytes(b"test content")

        with pytest.raises(ValueError):
            cache_module.upload_object("invalid", str(test_file))

        with pytest.raises(ValueError):
            cache_module.upload_object("a/b/c/d", str(test_file))

    @pytest.mark.parametrize(
        "key_or_prefix",
        [
            "/session",
            "app/",
            "../session",
            "app/../obj",
            "app/session/",
            "*/session",
            "app/*",
            "app/*/obj",
            "app/session/*",
        ],
    )
    def test_upload_object_validation_comes_from_object_key(self, key_or_prefix, tmp_path):
        from flamepy.core import cache as cache_module

        test_file = tmp_path / "test.tar.gz"
        test_file.write_bytes(b"test content")

        with pytest.raises(ValueError):
            cache_module.upload_object(key_or_prefix, str(test_file))

    def test_download_object(self, monkeypatch, tmp_path):
        import pyarrow as pa

        from flamepy.core import cache as cache_module

        dest_file = tmp_path / "downloaded.tar.gz"
        test_content = b"downloaded content"

        class MockBatch:
            def column(self, name):
                if name == "data":
                    return pa.array([test_content], type=pa.binary())
                return pa.array([0], type=pa.uint64())

        class MockReader:
            def __iter__(self):
                return iter([MockBatch()])

        class MockFlightClient:
            def do_get(self, ticket):
                return MockReader()

        monkeypatch.setattr(cache_module, "_get_cache_tls_config", lambda: None)
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        ref = ObjectRef(endpoint="grpc://host:9090", key="myapp/pkg/test.tar.gz", version=0)
        cache_module.download_object(ref, str(dest_file))

        assert dest_file.exists()
        assert dest_file.read_bytes() == test_content

    def test_download_object_not_found(self, monkeypatch, tmp_path):
        import pytest

        from flamepy.core import cache as cache_module

        dest_file = tmp_path / "downloaded.tar.gz"

        class MockReader:
            def __iter__(self):
                return iter([])

        class MockFlightClient:
            def do_get(self, ticket):
                return MockReader()

        monkeypatch.setattr(cache_module, "_get_cache_tls_config", lambda: None)
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        ref = ObjectRef(endpoint="grpc://host:9090", key="myapp/pkg/notfound.tar.gz", version=0)

        with pytest.raises(ValueError, match="Failed to download"):
            cache_module.download_object(ref, str(dest_file))

        assert not dest_file.exists()


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

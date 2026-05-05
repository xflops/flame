import threading

import bson

from flamepy.core.cache import (
    Object,
    ObjectKey,
    ObjectRef,
    _cache_lock,
    _deserialize_object,
    _object_cache,
    _serialize_object,
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


class TestClientSideCaching:
    def setup_method(self):
        with _cache_lock:
            _object_cache.clear()

    def teardown_method(self):
        with _cache_lock:
            _object_cache.clear()

    def test_cache_hit_returns_cached_data(self, monkeypatch):
        from flamepy.core import cache as cache_module

        base_data = {"from": "cache"}
        cache_key = ("grpc://host:9090", "app/session/obj1")
        cached_obj = Object(version=5, data=base_data)

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        call_count = {"server": 0}

        def mock_fetch_object_data(ref, cached_version, deserializer=None):
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

        def mock_fetch_object_data(ref, cached_version, deserializer=None):
            return server_data, 1

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

        def mock_fetch_object_data(ref, cached_version, deserializer=None):
            return new_data, 2

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj3", version=1)
        result = cache_module.get_object(ref)

        assert result == new_data
        with _cache_lock:
            assert _object_cache[cache_key].version == 2
            assert _object_cache[cache_key].data == new_data

    def test_version_zero_bypasses_cache(self, monkeypatch):
        from flamepy.core import cache as cache_module

        cached_data = {"cached": "data"}
        cache_key = ("grpc://host:9090", "app/session/obj4")
        cached_obj = Object(version=5, data=cached_data)

        with _cache_lock:
            _object_cache[cache_key] = cached_obj

        server_data = {"fresh": "data"}

        def mock_fetch_object_data(ref, cached_version, deserializer=None):
            assert cached_version == 0
            return server_data, 6

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj4", version=0)
        result = cache_module.get_object(ref)

        assert result == server_data

    def test_deserializer_combines_base_and_deltas(self, monkeypatch):
        from flamepy.core import cache as cache_module

        def mock_fetch_object_data(ref, cached_version, deserializer=None):
            base = [1, 2, 3]
            delta1 = [4, 5]
            delta2 = [6]
            if deserializer is not None:
                return deserializer(base, [delta1, delta2]), 1
            return base, 1

        monkeypatch.setattr(cache_module, "_fetch_object_data", mock_fetch_object_data)

        def merge_lists(base_data, deltas):
            result = list(base_data)
            for d in deltas:
                result.extend(d)
            return result

        ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/obj5", version=0)
        result = cache_module.get_object(ref, deserializer=merge_lists)

        assert result == [1, 2, 3, 4, 5, 6]

    def test_thread_safety(self, monkeypatch):
        from flamepy.core import cache as cache_module

        results = []
        errors = []

        def mock_fetch_object_data(ref, cached_version, deserializer=None):
            return {"thread": ref.key}, 1

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

        class MockFlightClient:
            def do_action(self, action):
                return [type("Result", (), {"body": type("Body", (), {"to_pybytes": lambda: b"OK"})()})()]

        class MockContext:
            cache = FlameClientCache(endpoint="grpc://host:9090")

        monkeypatch.setattr(cache_module, "FlameContext", lambda: MockContext())
        monkeypatch.setattr(cache_module, "_get_flight_client", lambda ep, tls=None: MockFlightClient())

        delete_objects("myapp")

        with _cache_lock:
            assert cache_key1 not in _object_cache
            assert cache_key2 not in _object_cache
            assert cache_key3 not in _object_cache
            assert cache_key4 in _object_cache


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
            def do_put(self, descriptor, schema):
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
            def do_put(self, descriptor, schema):
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
        import pytest

        from flamepy.core import cache as cache_module

        test_file = tmp_path / "test.tar.gz"
        test_file.write_bytes(b"test content")

        with pytest.raises(ValueError, match="Invalid key format"):
            cache_module.upload_object("invalid", str(test_file))

        with pytest.raises(ValueError, match="Invalid key format"):
            cache_module.upload_object("a/b/c/d", str(test_file))

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

    def test_from_prefix_invalid(self):
        import pytest

        with pytest.raises(ValueError):
            ObjectKey.from_prefix("invalid")

        with pytest.raises(ValueError):
            ObjectKey.from_prefix("a/b/c")

    def test_from_key_invalid(self):
        import pytest

        with pytest.raises(ValueError):
            ObjectKey.from_key("a/b")

        with pytest.raises(ValueError):
            ObjectKey.from_key("a/b/c/d")

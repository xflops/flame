import threading
from types import SimpleNamespace

import numpy as np
import pyarrow as pa
import pytest

from flamepy.core.cache import (
    _TYPE_ARROW_ARRAY,
    _TYPE_ARROW_BATCH,
    _TYPE_ARROW_TABLE,
    _TYPE_CLOUDPICKLE,
    _TYPE_NUMPY,
    _TYPE_PANDAS_DATAFRAME,
    _TYPE_POLARS_DATAFRAME,
    FetchMode,
    FetchResult,
    Object,
    ObjectKey,
    ObjectRef,
    Patch,
    _cache_lock,
    _deserialize_object_data,
    _object_cache,
    _serialize_object_data,
)


class TestGrpcClientEndpoints:
    def setup_method(self):
        from flamepy.core import cache as cache_module

        with cache_module._client_pool_lock:
            cache_module._client_pool.clear()

    def teardown_method(self):
        from flamepy.core import cache as cache_module

        with cache_module._client_pool_lock:
            cache_module._client_pool.clear()

    def test_direct_plaintext_endpoint(self, monkeypatch):
        from flamepy.core import cache as cache_module

        calls = []
        monkeypatch.setattr(cache_module.grpc, "insecure_channel", lambda target, options: calls.append((target, options)) or object())
        monkeypatch.setattr(cache_module.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: channel)
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=None))
        cache_module._get_cache_client("grpc://cache:9090")
        assert calls == [("cache:9090", cache_module.GRPC_OPTIONS)]

    def test_proxy_preserves_object_authority_and_custom_roots(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module

        ca_file = tmp_path / "ca.pem"
        ca_file.write_bytes(b"proxy CA")
        calls = []
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint="grpcs-proxy://gateway.example:443")))
        monkeypatch.setattr(cache_module.grpc, "ssl_channel_credentials", lambda root_certificates: calls.append(("roots", root_certificates)) or object())
        monkeypatch.setattr(cache_module.grpc, "secure_channel", lambda target, credentials, options: calls.append((target, options)) or object())
        monkeypatch.setattr(cache_module.cache_pb2_grpc, "ObjectCacheServiceStub", lambda channel: channel)
        client = cache_module._get_cache_client("grpc://object-cache:9090", cache_module.FlameClientTls(ca_file=str(ca_file)))
        assert client is cache_module._get_cache_client("grpc://object-cache:9090")
        assert calls[0] == ("roots", b"proxy CA")
        assert calls[1][0] == "gateway.example:443"
        assert ("grpc.default_authority", "object-cache:9090") in calls[1][1]

    @pytest.mark.parametrize("endpoint", ["grpcs-proxy://gateway:443/path", "grpcs-proxy://gateway"])
    def test_invalid_proxy(self, endpoint):
        from flamepy.core import cache as cache_module

        with pytest.raises(ValueError, match="proxy endpoint"):
            cache_module._resolve_cache_endpoint(endpoint)


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


class TestClientSideCaching:
    def setup_method(self):
        with _cache_lock:
            _object_cache.clear()

    def teardown_method(self):
        with _cache_lock:
            _object_cache.clear()

    def _patch_fetch_client(self, monkeypatch, responses):
        from flamepy.core import cache as cache_module

        class FakeClient:
            def Get(self, request):  # noqa: N802 - mirrors the generated gRPC stub
                self.request = request
                return iter(responses)

        fake_client = FakeClient()
        monkeypatch.setattr(cache_module, "_get_cache_client", lambda endpoint, tls: fake_client)
        monkeypatch.setattr(cache_module, "_get_cache_tls_config", lambda: None)
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

    def test_fetch_object_data_parses_full_response_chunks(self, monkeypatch):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        responses = [pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=3, data_type=_TYPE_CLOUDPICKLE))]
        for kind, version, value in [(pb.CACHE_CHUNK_KIND_BASE, 1, [1]), (pb.CACHE_CHUNK_KIND_PATCH, 2, [2]), (pb.CACHE_CHUNK_KIND_PATCH, 3, [3])]:
            _, data = _serialize_object_data(value)
            responses.append(pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=kind, version=version, data=data[:2])))
            responses.append(pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=kind, version=version, data=data[2:])))
        client = self._patch_fetch_client(monkeypatch, responses)
        result = cache_module._fetch_object_data(ObjectRef("grpc://host:9090", "app/session/obj6"), 0)
        assert client.request.key == "app/session/obj6"
        assert result.mode == FetchMode.FULL
        assert result.version == 3
        assert result.base == [1]
        assert [patch.data for patch in result.patches] == [[2], [3]]

    def test_fetch_object_data_rejects_base_after_patch(self, monkeypatch):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        responses = [
            pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=2, data_type=_TYPE_CLOUDPICKLE)),
            pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_PATCH, version=2, data=_serialize_object_data([2])[1])),
            pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=1, data=_serialize_object_data([1])[1])),
        ]
        self._patch_fetch_client(monkeypatch, responses)
        with pytest.raises(ValueError, match="start with a base"):
            cache_module._fetch_object_data(ObjectRef("grpc://host:9090", "app/session/obj7"), 1)

    def test_fetch_object_data_rejects_unknown_data_type(self, monkeypatch):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        self._patch_fetch_client(
            monkeypatch,
            [
                pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=1, data_type="unknown.zstd")),
                pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=1, data=cache_module._compress_data(b"data", "zstd"))),
            ],
        )
        with pytest.raises(ValueError, match="Unsupported cache data type"):
            cache_module._fetch_object_data(ObjectRef("grpc://host:9090", "app/session/obj7"), 0)

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


class TestGrpcOperations:
    def setup_method(self):
        with _cache_lock:
            _object_cache.clear()

    def teardown_method(self):
        with _cache_lock:
            _object_cache.clear()

    def _client(self, monkeypatch):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2

        class FakeClient:
            puts = []
            patches = []
            deletes = []
            responses = []
            metadata_data_type = _TYPE_CLOUDPICKLE
            metadata_requests = []

            def Put(self, requests, timeout=None):  # noqa: N802 - mirrors the generated gRPC stub
                self.puts.append((list(requests), timeout))
                return pb.CacheObjectMetadata(endpoint="grpc://host:9090", key="app/session/obj", version=2)

            def Patch(self, requests, timeout=None):  # noqa: N802 - mirrors the generated gRPC stub
                self.patches.append((list(requests), timeout))
                return pb.CacheObjectMetadata(endpoint="grpc://host:9090", key="app/session/obj", version=2)

            def Delete(self, request):  # noqa: N802 - mirrors the generated gRPC stub
                self.deletes.append(request.key)
                return pb.CacheDeleteResponse()

            def Get(self, request):  # noqa: N802 - mirrors the generated gRPC stub
                return iter(self.responses)

            def GetMetadata(self, request):  # noqa: N802 - mirrors the generated gRPC stub
                self.metadata_requests.append(request.key)
                return pb.CacheObjectMetadata(data_type=self.metadata_data_type)

        client = FakeClient()
        monkeypatch.setattr(cache_module, "_get_cache_client", lambda endpoint, tls=None: client)
        monkeypatch.setattr(cache_module, "_get_cache_tls_config", lambda: None)
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=cache_module.FlameClientCache(endpoint="grpc://host:9090", storage="/ignored")))
        return client

    def test_put_arrow_uses_opaque_serialized_chunks(self, monkeypatch):
        from flamepy.core import cache as cache_module

        client = self._client(monkeypatch)
        table = pa.table({"x": [1, 2, 3]})
        ref = cache_module.put_object("app/session", table)
        requests, _ = client.puts[0]
        assert ref.version == 2
        assert requests[0].header.key == "app/session"
        assert requests[0].header.data_type == f"{_TYPE_ARROW_TABLE}.zstd"
        assert requests[0].WhichOneof("payload") == "header"
        assert _deserialize_object_data(_TYPE_ARROW_TABLE, cache_module._decompress_data(b"".join(request.data for request in requests[1:]), "zstd")).equals(table)

    def test_update_and_patch(self, monkeypatch):
        from flamepy.core import cache as cache_module

        client = self._client(monkeypatch)
        ref = ObjectRef("grpc://host:9090", "app/session/obj", 1)
        cache_module.update_object(ref, {"new": True})
        cache_module.patch_object(ref, [1, 2])
        assert len(client.puts) == len(client.patches) == 1
        assert client.puts[0][0][0].header.key == ref.key
        assert client.patches[0][0][0].header.key == ref.key
        assert client.patches[0][0][0].header.data_type == _TYPE_CLOUDPICKLE
        assert _deserialize_object_data(_TYPE_CLOUDPICKLE, b"".join(r.data for r in client.patches[0][0][1:])) == [1, 2]

    def test_zstd_object_write_read_and_patch_inherits_codec(self, monkeypatch):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        client = self._client(monkeypatch)
        client.metadata_data_type = "arrow.table.zstd"
        value = pa.table({"repeated": ["x" * 100] * 100})
        ref = cache_module.put_object("app/session", value)
        requests, _ = client.puts[0]
        assert requests[0].header.data_type == "arrow.table.zstd"
        encoded = b"".join(request.data for request in requests[1:])
        assert len(encoded) < 1000
        assert cache_module._deserialize_object_data(_TYPE_ARROW_TABLE, cache_module._decompress_data(encoded, "zstd")).equals(value)

        patch = pa.table({"patch": ["y"]})
        cache_module.patch_object(ref, patch)
        assert client.metadata_requests == [ref.key]
        patch_requests, _ = client.patches[0]
        assert patch_requests[0].header.data_type == "arrow.table.zstd"
        patch_payload = b"".join(request.data for request in patch_requests[1:])
        assert cache_module._deserialize_object_data(_TYPE_ARROW_TABLE, cache_module._decompress_data(patch_payload, "zstd")).equals(patch)

        client.responses = [
            pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=2, data_type="arrow.table.zstd")),
            pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=1, data=encoded)),
            pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_PATCH, version=2, data=patch_payload)),
        ]
        result = cache_module._fetch_object_data(ref, 0)
        assert result.base.equals(value)
        assert result.patches[0].data.equals(patch)

        replacement = pa.table({"replacement": ["z"]})
        cache_module.update_object(ref, replacement)
        update_requests, _ = client.puts[1]
        assert update_requests[0].header.data_type == "arrow.table.zstd"
        update_payload = b"".join(request.data for request in update_requests[1:])
        assert cache_module._deserialize_object_data(_TYPE_ARROW_TABLE, cache_module._decompress_data(update_payload, "zstd")).equals(replacement)

    def test_patch_uses_stored_type_and_rejects_mismatch_before_write(self, monkeypatch):
        from flamepy.core import cache as cache_module

        client = self._client(monkeypatch)
        ref = ObjectRef("grpc://host:9090", "app/session/obj", 1)
        cache_module.patch_object(ref, [1])
        assert client.metadata_requests == [ref.key]
        client.metadata_data_type = _TYPE_ARROW_TABLE
        with pytest.raises(ValueError, match="does not match cached object type"):
            cache_module.patch_object(ref, [2])
        assert len(client.puts) == 0
        assert len(client.patches) == 1

    def test_automatic_compression_depends_on_type_not_size_or_ratio(self):
        import os

        from flamepy.core import cache as cache_module

        for value in ("x", "x" * 100000, os.urandom(100000)):
            data_type, data = cache_module._encode_object_data(value)
            assert data_type == _TYPE_CLOUDPICKLE
            assert _deserialize_object_data(data_type, data) == value

        small = np.array([1], dtype=np.int64)
        small_type, small_data = cache_module._encode_object_data(small)
        assert small_type == "numpy.zstd"
        np.testing.assert_array_equal(_deserialize_object_data(_TYPE_NUMPY, cache_module._decompress_data(small_data, "zstd")), small)

        random = np.frombuffer(os.urandom(128 * 1024), dtype=np.uint8)
        random_type, random_data = cache_module._encode_object_data(random)
        assert random_type == "numpy.zstd"
        _, uncompressed = _serialize_object_data(random)
        assert len(random_data) > len(uncompressed)
        np.testing.assert_array_equal(_deserialize_object_data(_TYPE_NUMPY, cache_module._decompress_data(random_data, "zstd")), random)

    def test_delete_clears_matching_local_entries(self, monkeypatch):
        from flamepy.core import cache as cache_module

        client = self._client(monkeypatch)
        with _cache_lock:
            _object_cache[("grpc://host:9090", "app/session/obj")] = Object(1, "old")
        cache_module.delete_objects("app/session")
        assert client.deletes == ["app/session"]
        with _cache_lock:
            assert not _object_cache

    def test_file_upload_download_streams_raw_bytes(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        client = self._client(monkeypatch)
        source = tmp_path / "source.bin"
        data = b"x" * (cache_module._UPLOAD_CHUNK_SIZE + 3)
        source.write_bytes(data)
        ref = cache_module.upload_object("app/session/obj", str(source))
        requests, timeout = client.puts[0]
        assert timeout == 300
        assert requests[0].header.key == "app/session/obj"
        assert requests[0].header.data_type == "raw"
        assert b"".join(request.data for request in requests[1:]) == data
        assert len(requests[1].data) == cache_module._UPLOAD_CHUNK_SIZE
        client.responses = [
            pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=2, data_type="raw")),
            pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=2, data=data[:3])),
            pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=2, data=data[3:])),
        ]
        destination = tmp_path / "dest.bin"
        cache_module.download_object(ref, str(destination))
        assert destination.read_bytes() == data

    def test_download_decodes_raw_zstd_when_present(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        client = self._client(monkeypatch)
        data = b"abc" * 10000
        encoded = cache_module._compress_data(data, "zstd")
        client.responses = [pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=2, data_type="raw.zstd"))]
        for start in range(0, len(encoded), 7):
            client.responses.append(pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=2, data=encoded[start : start + 7])))
        destination = tmp_path / "downloaded.bin"
        cache_module.download_object(ObjectRef("grpc://host:9090", "app/session/obj"), str(destination))
        assert destination.read_bytes() == data

    def test_compressed_archive_upload_keeps_original_bytes(self, monkeypatch, tmp_path):
        import gzip

        from flamepy.core import cache as cache_module

        client = self._client(monkeypatch)
        source = tmp_path / "package.tar.gz"
        archive = gzip.compress(b"package contents" * 10000)
        source.write_bytes(archive)
        cache_module.upload_object("app/session/package.tar.gz", str(source))
        requests, _ = client.puts[0]
        assert requests[0].header.data_type == "raw"
        assert b"".join(request.data for request in requests[1:]) == archive

    def test_download_rejects_patch(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        client = self._client(monkeypatch)
        client.responses = [pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=2, data_type="raw")), pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_PATCH, version=2, data=b"bad"))]
        with pytest.raises(ValueError, match="base chunks only"):
            cache_module.download_object(ObjectRef("grpc://host:9090", "app/session/obj"), str(tmp_path / "bad"))

    def test_empty_file_round_trip(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module

        pb = cache_module.cache_pb2
        client = self._client(monkeypatch)
        source = tmp_path / "empty.bin"
        source.write_bytes(b"")
        ref = cache_module.upload_object("app/session/obj", str(source))
        assert len(client.puts[0][0]) == 1
        client.responses = [pb.CacheGetResponse(header=pb.CacheGetHeader(mode=pb.CACHE_GET_MODE_FULL, version=2, data_type="raw")), pb.CacheGetResponse(chunk=pb.CacheGetChunk(kind=pb.CACHE_CHUNK_KIND_BASE, version=2))]
        destination = tmp_path / "downloaded.bin"
        cache_module.download_object(ref, str(destination))
        assert destination.read_bytes() == b""

    def test_upload_validates_path_before_rpc(self, monkeypatch, tmp_path):
        from flamepy.core import cache as cache_module

        client = self._client(monkeypatch)
        source = tmp_path / "source.bin"
        source.write_bytes(b"data")
        with pytest.raises(ValueError):
            cache_module.upload_object("invalid", str(source))
        with pytest.raises(FileNotFoundError):
            cache_module.upload_object("app/session/obj", str(tmp_path / "missing"))
        assert not client.puts


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

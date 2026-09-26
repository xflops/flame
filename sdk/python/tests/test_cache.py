import asyncio
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

            def __call__(self, base, patches):
                return base + self.suffix

            def _check_lock(self):
                assert _cache_lock.acquire(blocking=False), "deserializer method called under cache lock"
                _cache_lock.release()

            def __hash__(self):
                self._check_lock()
                return 1

            def __eq__(self, other):
                self._check_lock()
                return isinstance(other, ReentrantDeserializer) and self.suffix == other.suffix

        obj = Object(version=1, data="base")
        first = ReentrantDeserializer("-first")
        second = ReentrantDeserializer("-second")

        assert _materialize_object(obj, first) == "base-first"
        assert _materialize_object(obj, first) == "base-first"
        assert _materialize_object(obj, second) == "base-second"


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

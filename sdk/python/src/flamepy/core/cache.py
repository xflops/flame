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

import atexit
import importlib
import os
import sys
import threading
import types
import uuid
from collections import OrderedDict
from dataclasses import asdict, dataclass, field
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Optional
from urllib.parse import urlparse

import bson
import cloudpickle

from flamepy.core.types import FlameClientCache, FlameClientTls, FlameContext

# numpy, pandas, polars and pyarrow are heavy to import, so they are imported when
# a value needs them. A value of one of their types means the module is already
# imported, so serializing other values never loads them.
if TYPE_CHECKING:
    import numpy as np
    import pyarrow as pa

# Type identifiers live in cache metadata; payload bytes contain only the codec output.
_TYPE_CLOUDPICKLE = "cloudpickle"
_TYPE_NUMPY = "numpy"
_TYPE_ARROW_TABLE = "arrow.table"
_TYPE_ARROW_ARRAY = "arrow.array"
_TYPE_ARROW_BATCH = "arrow.record_batch"
_TYPE_PANDAS_DATAFRAME = "pandas.dataframe"
_TYPE_POLARS_DATAFRAME = "polars.dataframe"
_TYPE_RAW = "raw"
_COMPRESSION_NONE = "none"
_COMPRESSION_ZSTD = "zstd"
_ZSTD_OBJECT_TYPES = {
    _TYPE_NUMPY,
    _TYPE_ARROW_TABLE,
    _TYPE_ARROW_ARRAY,
    _TYPE_ARROW_BATCH,
    _TYPE_PANDAS_DATAFRAME,
    _TYPE_POLARS_DATAFRAME,
}


def _pyarrow() -> Any:
    import pyarrow as pa

    return pa


def _required_module(name: str) -> Any:
    try:
        return importlib.import_module(name)
    except ImportError as exc:
        raise ImportError(f"{name} is required to deserialize this object") from exc


# Turns the data parts of an object, in the order they were written, into a value.
Deserializer = Callable[[List[Any]], Any]

WILDCARD_SESSION = "*"


class Ref:
    """Base type for inline and cache-backed object references."""


@dataclass
class ValueRef(Ref):
    """A value carried directly in a task response or request."""

    value: Any


@dataclass
class ObjectRef(Ref):
    """Object reference for remote cached objects.

    Version semantics:
    - version=0: Force fresh download (bypass client cache)
    - version>=1: Normal versioned object from server
    Server always returns version >= 1 for stored objects.
    """

    endpoint: str
    key: str  # Object key in format "<app>/<ssn>/<uuid>"
    version: int = 0

    def encode(self) -> bytes:
        data = asdict(self)
        return bson.encode(data)

    @classmethod
    def decode(cls, json_data: bytes) -> "ObjectRef":
        data = bson.decode(json_data)
        return cls(**data)


@dataclass
class Object:
    """Cached object: its data parts in the order they were written, the first
    at version 1, and the values the deserializers made of them."""

    version: int
    data: List[Any]
    materialized: Dict[Any, Any] = field(default_factory=dict)


@dataclass
class FetchResult:
    """What a Get returned: the version of the object, and its data parts after
    the version the client asked for, one part per version (all of them for
    version 0)."""

    version: int
    data: List[Any]


class _IdentityKey:
    __slots__ = ("value", "function")

    def __init__(self, value: Any):
        # A fresh bound method object is created on every attribute access.
        # Keep those cache keys stable without invoking user-defined equality.
        if isinstance(value, types.MethodType):
            self.value = value.__self__
            self.function = value.__func__
        else:
            self.value = value
            self.function = None

    def __hash__(self) -> int:
        return hash((id(self.value), id(self.function)))

    def __eq__(self, other: Any) -> bool:
        return isinstance(other, _IdentityKey) and self.value is other.value and self.function is other.function


# Client-side LRU cache with max size limit (O(1) operations using OrderedDict)
_CACHE_MAX_SIZE = 1000
_object_cache: OrderedDict[tuple, Object] = OrderedDict()
_cache_lock = threading.Lock()


def _cache_get(key: tuple) -> Optional[Object]:
    """Get from cache and update LRU order (O(1) with OrderedDict)."""
    with _cache_lock:
        if key not in _object_cache:
            return None
        _object_cache.move_to_end(key)
        return _object_cache[key]


def _cache_remove(key: tuple) -> None:
    """Remove from cache."""
    with _cache_lock:
        _object_cache.pop(key, None)


def _cache_remove_matching(object_key: "ObjectKey") -> None:
    """Remove all entries matched by an ObjectKey."""
    with _cache_lock:
        keys_to_remove = [k for k in _object_cache if object_key.matches_key(k[1])]
        for key in keys_to_remove:
            _object_cache.pop(key, None)


def _materialized_cache_key(deserializer: Optional[Deserializer]) -> Any:
    if deserializer is None:
        return None
    return _IdentityKey(deserializer)


def _materialize_object(obj: Object, deserializer: Optional[Deserializer] = None) -> Any:
    """The value of an object: the newest data part, or what `deserializer`
    makes of all the parts."""
    materialized_key = _materialized_cache_key(deserializer)
    while True:
        with _cache_lock:
            if materialized_key in obj.materialized:
                return obj.materialized[materialized_key]
            version = obj.version
            data = list(obj.data)

        # A user deserializer may be slow or may call another cache operation.
        # Never hold the cache lock while invoking it.
        value = data[-1] if deserializer is None else deserializer(data)
        with _cache_lock:
            if obj.version != version:
                continue
            return obj.materialized.setdefault(materialized_key, value)


def _cache_append_parts(key: tuple, result: FetchResult, requested_version: int = 0) -> Optional[Object]:
    """Add the data parts of a Get result, asked for after `requested_version`,
    to the cache and return the cached object: it is created if it is not cached
    and the parts are appended to it otherwise, except for the ones it already
    has. Returns None if the object is not cached but the result is only the
    data after `requested_version`, or if the cache is behind it: the caller must
    get all the data.
    """
    with _cache_lock:
        cached = _object_cache.get(key)
        if cached is None:
            if requested_version:
                return None
            cached = Object(version=result.version, data=list(result.data))
            _object_cache[key] = cached
        else:
            # Parts the cache got after the request, from another call, are
            # already in it.
            have = cached.version - requested_version
            if have < 0:
                return None
            if cached.version < result.version:
                cached.data.extend(result.data[have:])
                cached.version = result.version
                cached.materialized.clear()

        _object_cache.move_to_end(key)
        while len(_object_cache) > _CACHE_MAX_SIZE:
            _object_cache.popitem(last=False)
        return cached


@dataclass(frozen=True)
class ObjectKey:
    """Parsed object key: <app_name>/<session_id>/<object_id>

    session_id can be WILDCARD_SESSION ('*') for all sessions, requires object_id to be None.
    """

    app_name: str
    session_id: str
    object_id: Optional[str] = None

    def __post_init__(self):
        if not self.app_name:
            raise ValueError("app_name cannot be empty")
        if self.app_name == WILDCARD_SESSION:
            raise ValueError("Wildcard '*' not allowed for app_name")
        if ".." in self.app_name or "\\" in self.app_name or "/" in self.app_name:
            raise ValueError(f"app_name contains invalid characters: '{self.app_name}'")

        if not self.session_id:
            raise ValueError("session_id cannot be empty")

        is_wildcard = self.session_id == WILDCARD_SESSION

        if not is_wildcard:
            if ".." in self.session_id or "\\" in self.session_id or "/" in self.session_id:
                raise ValueError(f"session_id contains invalid characters: '{self.session_id}'")

        if self.object_id is not None:
            if is_wildcard:
                raise ValueError("Wildcard session '*' cannot have object_id")
            if not self.object_id:
                raise ValueError("object_id cannot be empty string")
            if self.object_id == WILDCARD_SESSION:
                raise ValueError("Wildcard '*' not allowed for object_id")
            if ".." in self.object_id or "\\" in self.object_id or "/" in self.object_id:
                raise ValueError(f"object_id contains invalid characters: '{self.object_id}'")

    @classmethod
    def from_path(cls, path: str) -> "ObjectKey":
        """Parse from key prefix or full key.

        Accepts '<app>/<ssn>' prefixes and '<app>/<ssn>/<uuid>' full keys.
        """
        parts = path.split("/")
        if len(parts) == 2:
            return cls(app_name=parts[0], session_id=parts[1], object_id=None)
        if len(parts) == 3:
            return cls(app_name=parts[0], session_id=parts[1], object_id=parts[2])
        raise ValueError(f"Invalid object key path '{path}': expected '<app>/<ssn>' or '<app>/<ssn>/<uuid>' format")

    @classmethod
    def from_prefix(cls, prefix: str) -> "ObjectKey":
        """Parse from key prefix '<app>/<ssn>'."""
        key = cls.from_path(prefix)
        if key.object_id is not None:
            raise ValueError(f"Invalid key prefix '{prefix}': expected '<app>/<ssn>' format")
        return key

    @classmethod
    def from_key(cls, key: str) -> "ObjectKey":
        """Parse from full key '<app>/<ssn>/<uuid>'."""
        object_key = cls.from_path(key)
        if object_key.object_id is None:
            raise ValueError(f"Invalid object key '{key}': expected '<app>/<ssn>/<uuid>' format")
        return object_key

    @classmethod
    def for_shared(cls, app_name: str) -> "ObjectKey":
        """Create key for shared storage: '<app>/shared'."""
        return cls(app_name=app_name, session_id="shared", object_id=None)

    @classmethod
    def for_all_sessions(cls, app_name: str) -> "ObjectKey":
        """Create wildcard key for all sessions: '<app>/*'."""
        return cls(app_name=app_name, session_id=WILDCARD_SESSION, object_id=None)

    def is_all_sessions(self) -> bool:
        """Return True if this key represents all sessions (session_id == '*')."""
        return self.session_id == WILDCARD_SESSION

    def with_generated_id(self) -> "ObjectKey":
        """Return new ObjectKey with a generated UUID."""
        return ObjectKey(
            app_name=self.app_name,
            session_id=self.session_id,
            object_id=str(uuid.uuid4()),
        )

    def to_prefix(self) -> str:
        """Return key prefix '<app>/<ssn>'."""
        return f"{self.app_name}/{self.session_id}"

    def to_key(self) -> Optional[str]:
        """Return full key '<app>/<ssn>/<uuid>' or None if object_id not set."""
        if self.object_id is None:
            return None
        return f"{self.app_name}/{self.session_id}/{self.object_id}"

    def matches_key(self, key: str) -> bool:
        """Return True if this prefix/full ObjectKey matches a full object key."""
        if self.is_all_sessions():
            prefix = f"{self.app_name}/"
            if not key.startswith(prefix):
                return False
            suffix = key[len(prefix) :]
            session_id, separator, object_id = suffix.partition("/")
            return bool(session_id and separator and object_id) and "/" not in object_id
        if self.object_id is None:
            prefix = f"{self.app_name}/{self.session_id}/"
            if not key.startswith(prefix):
                return False
            object_id = key[len(prefix) :]
            return bool(object_id) and "/" not in object_id
        return key == self.to_key()

    def __str__(self) -> str:
        return self.to_key() or self.to_prefix()


def _serialize_numpy(arr: "np.ndarray") -> bytes:
    """Serialize numpy array using Arrow's zero-copy tensor format."""
    pa = _pyarrow()
    tensor = pa.Tensor.from_numpy(arr)
    sink = pa.BufferOutputStream()
    pa.ipc.write_tensor(tensor, sink)
    return sink.getvalue().to_pybytes()


def _deserialize_numpy(data: bytes) -> "np.ndarray":
    """Deserialize numpy array from Arrow tensor format."""
    pa = _pyarrow()
    reader = pa.BufferReader(data)
    tensor = pa.ipc.read_tensor(reader)
    return tensor.to_numpy()


def _serialize_arrow_table(table: "pa.Table") -> bytes:
    """Serialize PyArrow Table using IPC stream format."""
    pa = _pyarrow()
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    return sink.getvalue().to_pybytes()


def _deserialize_arrow_table(data: bytes) -> "pa.Table":
    """Deserialize PyArrow Table from IPC stream format."""
    pa = _pyarrow()
    reader = pa.ipc.open_stream(pa.BufferReader(data))
    return reader.read_all()


def _serialize_arrow_batch(batch: "pa.RecordBatch") -> bytes:
    """Serialize PyArrow RecordBatch using IPC stream format."""
    pa = _pyarrow()
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


def _deserialize_arrow_batch(data: bytes) -> "pa.RecordBatch":
    """Deserialize PyArrow RecordBatch from IPC stream format."""
    pa = _pyarrow()
    reader = pa.ipc.open_stream(pa.BufferReader(data))
    return reader.read_next_batch()


def _serialize_arrow_array(arr: "pa.Array") -> bytes:
    """Serialize PyArrow Array by wrapping in a RecordBatch."""
    pa = _pyarrow()
    batch = pa.RecordBatch.from_arrays([arr], names=["data"])
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


def _deserialize_arrow_array(data: bytes) -> "pa.Array":
    """Deserialize PyArrow Array from IPC stream format."""
    pa = _pyarrow()
    reader = pa.ipc.open_stream(pa.BufferReader(data))
    batch = reader.read_next_batch()
    return batch.column(0)


def _serialize_cloudpickle(obj: Any) -> bytes:
    """Serialize using cloudpickle (fallback for arbitrary Python objects)."""
    return cloudpickle.dumps(obj, protocol=cloudpickle.DEFAULT_PROTOCOL)


def _deserialize_cloudpickle(data: bytes) -> Any:
    """Deserialize using cloudpickle."""
    return cloudpickle.loads(data)


def _serialize_object_data(obj: Any) -> tuple[str, bytes]:
    """Encode a value and return its type tag separately from its payload."""
    # A value of a library's type means the library is already imported.
    np = sys.modules.get("numpy")
    if np is not None and isinstance(obj, np.ndarray):
        if obj.flags.c_contiguous or obj.flags.f_contiguous:
            return _TYPE_NUMPY, _serialize_numpy(obj)
    pd = sys.modules.get("pandas")
    if pd is not None and isinstance(obj, pd.DataFrame):
        return _TYPE_PANDAS_DATAFRAME, _serialize_arrow_table(_pyarrow().Table.from_pandas(obj))
    pl = sys.modules.get("polars")
    if pl is not None and isinstance(obj, (pl.DataFrame, pl.LazyFrame)):
        frame = obj.collect() if isinstance(obj, pl.LazyFrame) else obj
        return _TYPE_POLARS_DATAFRAME, _serialize_arrow_table(frame.to_arrow())
    pa = sys.modules.get("pyarrow")
    if pa is not None:
        if isinstance(obj, pa.Table):
            return _TYPE_ARROW_TABLE, _serialize_arrow_table(obj)
        if isinstance(obj, pa.RecordBatch):
            return _TYPE_ARROW_BATCH, _serialize_arrow_batch(obj)
        if isinstance(obj, pa.Array):
            return _TYPE_ARROW_ARRAY, _serialize_arrow_array(obj)
    if hasattr(obj, "to_arrow"):
        try:
            arrow_obj = obj.to_arrow()
        except TypeError:
            arrow_obj = None
        pa = _pyarrow()
        if isinstance(arrow_obj, pa.Table):
            return _TYPE_ARROW_TABLE, _serialize_arrow_table(arrow_obj)
        if isinstance(arrow_obj, pa.RecordBatch):
            return _TYPE_ARROW_BATCH, _serialize_arrow_batch(arrow_obj)
    return _TYPE_CLOUDPICKLE, _serialize_cloudpickle(obj)


def _deserialize_object_data(data_type: str, data: bytes) -> Any:
    """Decode a value using the data type supplied by object cache metadata."""
    if data_type == _TYPE_CLOUDPICKLE:
        return _deserialize_cloudpickle(data)
    if data_type == _TYPE_NUMPY:
        _required_module("numpy")
        return _deserialize_numpy(data)
    if data_type == _TYPE_ARROW_TABLE:
        return _deserialize_arrow_table(data)
    if data_type == _TYPE_ARROW_BATCH:
        return _deserialize_arrow_batch(data)
    if data_type == _TYPE_ARROW_ARRAY:
        return _deserialize_arrow_array(data)
    if data_type == _TYPE_PANDAS_DATAFRAME:
        _required_module("pandas")
        return _deserialize_arrow_table(data).to_pandas()
    if data_type == _TYPE_POLARS_DATAFRAME:
        return _required_module("polars").from_arrow(_deserialize_arrow_table(data))
    raise ValueError(f"Unsupported cache data type: {data_type}")


def _validate_compression(compression: str) -> str:
    if compression not in (_COMPRESSION_NONE, _COMPRESSION_ZSTD):
        raise ValueError(f"Unsupported cache compression: {compression}")
    return compression


def _type_and_compression(data_type: str) -> tuple[str, str]:
    if data_type.endswith(".zstd"):
        return data_type[: -len(".zstd")], _COMPRESSION_ZSTD
    return data_type, _COMPRESSION_NONE


def _compress_data(data: bytes, compression: str) -> bytes:
    if _validate_compression(compression) == _COMPRESSION_NONE:
        return data
    return _pyarrow().Codec(_COMPRESSION_ZSTD).compress(data, asbytes=True)


def _decompress_data(data: bytes, compression: str) -> bytes:
    if _validate_compression(compression) == _COMPRESSION_NONE:
        return data
    pa = _pyarrow()
    with pa.CompressedInputStream(pa.BufferReader(data), _COMPRESSION_ZSTD) as stream:
        return stream.read()


def _encode_object_data(obj: Any) -> tuple[str, bytes]:
    """Compress structured values and tensors; leave generic Python objects raw."""
    data_type, data = _serialize_object_data(obj)
    torch_module = sys.modules.get("torch")
    tensor_type = getattr(torch_module, "Tensor", None)
    is_tensor = isinstance(tensor_type, type) and isinstance(obj, tensor_type)
    numpy_module = sys.modules.get("numpy")
    is_numpy = numpy_module is not None and isinstance(obj, numpy_module.ndarray)
    if data_type in _ZSTD_OBJECT_TYPES or is_numpy or is_tensor:
        return f"{data_type}.zstd", _compress_data(data, _COMPRESSION_ZSTD)
    return data_type, data


_context_cache: Optional[FlameContext] = None
_context_cache_lock = threading.Lock()


def _get_cached_context() -> FlameContext:
    """Get cached FlameContext singleton to avoid repeated config file reads."""
    global _context_cache
    if _context_cache is not None:
        return _context_cache
    with _context_cache_lock:
        if _context_cache is None:
            _context_cache = FlameContext()
        return _context_cache


def _validate_proxy_endpoint(endpoint: str) -> None:
    parsed = urlparse(endpoint)
    try:
        port = parsed.port
    except ValueError as exc:
        raise ValueError(f"Invalid object cache proxy endpoint: {endpoint}") from exc
    if parsed.scheme != "grpcs-proxy" or not parsed.hostname or parsed.username or parsed.password or parsed.path not in ("", "/") or parsed.params or parsed.query or parsed.fragment or port is None:
        raise ValueError("Object cache proxy endpoint must be grpcs-proxy://<host>:<port> without credentials, a path, query, or fragment")


GRPC_OPTIONS = [
    ("grpc.max_send_message_length", -1),
    ("grpc.max_receive_message_length", -1),
]


def _cache_proxy_endpoint() -> Optional[str]:
    """Return the configured TLS gRPC proxy, if any."""
    cache_config = _get_cached_context().cache
    if isinstance(cache_config, str):
        endpoint = cache_config
    elif isinstance(cache_config, FlameClientCache):
        endpoint = cache_config.endpoint
    else:
        endpoint = cache_config.get("endpoint") if cache_config else None
    if endpoint and urlparse(endpoint).scheme == "grpcs-proxy":
        _validate_proxy_endpoint(endpoint)
        return endpoint
    return None


def _resolve_cache_endpoint(endpoint: str) -> tuple[str, Optional[str]]:
    """Resolve an object endpoint to its dial location and optional cache target."""
    parsed = urlparse(endpoint)
    if parsed.scheme == "grpc-proxy":
        raise ValueError("grpc-proxy:// is unsupported; use grpcs-proxy://")
    if parsed.scheme == "grpcs-proxy":
        _validate_proxy_endpoint(endpoint)
    proxy_endpoint = _cache_proxy_endpoint()
    if parsed.scheme in ("grpc", "grpcs", "grpc+tls") and proxy_endpoint:
        try:
            port = parsed.port
        except ValueError as exc:
            raise ValueError(f"Invalid object cache endpoint: {endpoint}") from exc
        if not parsed.hostname or port is None or parsed.username or parsed.password or parsed.path not in ("", "/") or parsed.params or parsed.query or parsed.fragment:
            raise ValueError(f"Invalid object cache endpoint: {endpoint}")
        return proxy_endpoint, parsed.netloc
    return endpoint, None


_UPLOAD_CHUNK_SIZE = 1024 * 1024  # 1MB


def _get_cache_tls_config() -> Optional[FlameClientTls]:
    """Get TLS configuration for cache from FlameContext.

    FlameContext automatically handles:
    1. Loading from ~/.flame/flame.yaml if it exists
    2. Building from environment variables (FLAME_CA_FILE, etc.) if no config file

    Returns:
        FlameClientTls if configured, None otherwise
    """
    context = _get_cached_context()
    cache_config = context.cache
    if isinstance(cache_config, FlameClientCache) and cache_config.tls:
        return cache_config.tls
    return None


# Public synchronous calls use the aio cache transport.
_aio_bridge = None
_aio_bridge_pid = None
_aio_bridge_lock = threading.Lock()


def _reset_aio_bridge_after_fork() -> None:
    global _aio_bridge, _aio_bridge_pid, _aio_bridge_lock
    _aio_bridge = None
    _aio_bridge_pid = None
    _aio_bridge_lock = threading.Lock()


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_reset_aio_bridge_after_fork)


def _get_aio_bridge():
    global _aio_bridge, _aio_bridge_pid
    from flamepy.core._bridge import LoopThread

    pid = os.getpid()
    with _aio_bridge_lock:
        if _aio_bridge is None or _aio_bridge_pid != pid:
            _aio_bridge = LoopThread(name="flamepy-cache-aio")
            _aio_bridge_pid = pid
        return _aio_bridge


def _call_aio_cache(name: str, *args: Any, **kwargs: Any) -> Any:
    from flamepy.core.aio import cache as aio_cache

    return _get_aio_bridge().call(getattr(aio_cache, name)(*args, **kwargs))


def _close_aio_bridge() -> None:
    global _aio_bridge, _aio_bridge_pid
    with _aio_bridge_lock:
        bridge = _aio_bridge
        _aio_bridge = None
        _aio_bridge_pid = None
    if bridge is not None:
        from flamepy.core.aio import cache as aio_cache

        try:
            bridge.call(aio_cache.close())
        finally:
            bridge.close()


atexit.register(_close_aio_bridge)


def put_object(key_prefix: str, obj: Any) -> ObjectRef:
    return _call_aio_cache("put_object", key_prefix, obj)


def get_object(ref: Ref, deserializer: Optional[Deserializer] = None) -> Any:
    if isinstance(ref, ValueRef):
        return ref.value if deserializer is None else deserializer([ref.value])
    if not isinstance(ref, ObjectRef):
        raise TypeError(f"Expected ValueRef or ObjectRef, got {type(ref).__name__}")
    return _call_aio_cache("get_object", ref, deserializer)


def patch_object(ref: ObjectRef, data: Any) -> ObjectRef:
    return _call_aio_cache("patch_object", ref, data)


def delete_objects(key_prefix: str, endpoint: Optional[str] = None) -> None:
    _call_aio_cache("delete_objects", key_prefix, endpoint)


def upload_package(app_name: str, file_path: str, filename: Optional[str] = None, endpoint: Optional[str] = None) -> ObjectRef:
    return _call_aio_cache("upload_package", app_name, file_path, filename, endpoint)


def download_package(app_name: str, filename: str, dest_path: str, endpoint: Optional[str] = None) -> None:
    _call_aio_cache("download_package", app_name, filename, dest_path, endpoint)


def delete_package(app_name: str, filename: str, endpoint: Optional[str] = None) -> None:
    _call_aio_cache("delete_package", app_name, filename, endpoint)

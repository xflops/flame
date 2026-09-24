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

import io
import logging
import sys
import threading
import uuid
from collections import OrderedDict
from dataclasses import asdict, dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Optional
from urllib.parse import urlparse

import bson
import cloudpickle
import grpc
import pyarrow as pa

from flamepy.core.types import FlameClientCache, FlameClientTls, FlameContext
from flamepy.proto import cache_pb2, cache_pb2_grpc

if TYPE_CHECKING:
    import numpy as np

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


try:
    import numpy as np

    _HAS_NUMPY = True
except ImportError:
    np = None  # type: ignore[assignment]
    _HAS_NUMPY = False

try:
    import pandas as pd

    _HAS_PANDAS = True
except ImportError:
    pd = None  # type: ignore[assignment]
    _HAS_PANDAS = False

try:
    import polars as pl

    _HAS_POLARS = True
except ImportError:
    pl = None  # type: ignore[assignment]
    _HAS_POLARS = False

logger = logging.getLogger(__name__)

Deserializer = Callable[[Any, List[Any]], Any]

WILDCARD_SESSION = "*"


class FetchMode(str, Enum):
    FULL = "full"
    PATCHES = "patches"


@dataclass
class ObjectRef:
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
class Patch:
    version: int
    data: Any


@dataclass
class Object:
    """Cached object with base data, versioned patches, and materialized views.

    The `data` field stores the base object to preserve the existing public
    behavior where get_object(..., deserializer=None) returns only the base.
    """

    version: int
    data: Any
    patches: List[Patch] = field(default_factory=list)
    materialized: Dict[Any, Any] = field(default_factory=dict)


@dataclass
class FetchResult:
    mode: FetchMode
    version: int
    base: Any = None
    patches: List[Patch] = field(default_factory=list)


class _IdentityKey:
    __slots__ = ("value",)

    def __init__(self, value: Any):
        self.value = value

    def __hash__(self) -> int:
        return id(self.value)

    def __eq__(self, other: Any) -> bool:
        return isinstance(other, _IdentityKey) and self.value is other.value


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


def _cache_put(key: tuple, obj: Object) -> None:
    """Put to cache with LRU eviction (O(1) with OrderedDict)."""
    with _cache_lock:
        if key in _object_cache:
            _object_cache.move_to_end(key)
        _object_cache[key] = obj

        while len(_object_cache) > _CACHE_MAX_SIZE:
            _object_cache.popitem(last=False)


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

    try:
        hash(deserializer)
    except TypeError:
        return _IdentityKey(deserializer)
    return deserializer


def _materialize_object(obj: Object, deserializer: Optional[Deserializer] = None) -> Any:
    materialized_key = _materialized_cache_key(deserializer)
    if materialized_key in obj.materialized:
        return obj.materialized[materialized_key]

    if deserializer is None:
        data = obj.data
    else:
        data = deserializer(obj.data, [patch.data for patch in obj.patches])

    obj.materialized[materialized_key] = data
    return data


def _cache_apply_patches(
    key: tuple,
    expected_version: int,
    new_version: int,
    patches: List[Patch],
) -> Optional[Object]:
    """Apply patch rows only if the cache is still at the requested version."""
    with _cache_lock:
        cached = _object_cache.get(key)
        if cached is None:
            return None

        if cached.version == expected_version:
            if new_version <= cached.version:
                return None
            cached.patches.extend(patches)
            cached.version = new_version
            cached.materialized.clear()
        elif cached.version < new_version:
            return None

        _object_cache.move_to_end(key)
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
    tensor = pa.Tensor.from_numpy(arr)
    sink = pa.BufferOutputStream()
    pa.ipc.write_tensor(tensor, sink)
    return sink.getvalue().to_pybytes()


def _deserialize_numpy(data: bytes) -> "np.ndarray":
    """Deserialize numpy array from Arrow tensor format."""
    reader = pa.BufferReader(data)
    tensor = pa.ipc.read_tensor(reader)
    return tensor.to_numpy()


def _serialize_arrow_table(table: pa.Table) -> bytes:
    """Serialize PyArrow Table using IPC stream format."""
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    return sink.getvalue().to_pybytes()


def _deserialize_arrow_table(data: bytes) -> pa.Table:
    """Deserialize PyArrow Table from IPC stream format."""
    reader = pa.ipc.open_stream(pa.BufferReader(data))
    return reader.read_all()


def _serialize_arrow_batch(batch: pa.RecordBatch) -> bytes:
    """Serialize PyArrow RecordBatch using IPC stream format."""
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


def _deserialize_arrow_batch(data: bytes) -> pa.RecordBatch:
    """Deserialize PyArrow RecordBatch from IPC stream format."""
    reader = pa.ipc.open_stream(pa.BufferReader(data))
    return reader.read_next_batch()


def _serialize_arrow_array(arr: pa.Array) -> bytes:
    """Serialize PyArrow Array by wrapping in a RecordBatch."""
    batch = pa.RecordBatch.from_arrays([arr], names=["data"])
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


def _deserialize_arrow_array(data: bytes) -> pa.Array:
    """Deserialize PyArrow Array from IPC stream format."""
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
    if _HAS_NUMPY and isinstance(obj, np.ndarray):
        if obj.flags.c_contiguous or obj.flags.f_contiguous:
            return _TYPE_NUMPY, _serialize_numpy(obj)
    if _HAS_PANDAS and isinstance(obj, pd.DataFrame):
        return _TYPE_PANDAS_DATAFRAME, _serialize_arrow_table(pa.Table.from_pandas(obj))
    if _HAS_POLARS and isinstance(obj, (pl.DataFrame, pl.LazyFrame)):
        frame = obj.collect() if isinstance(obj, pl.LazyFrame) else obj
        return _TYPE_POLARS_DATAFRAME, _serialize_arrow_table(frame.to_arrow())
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
        if not _HAS_NUMPY:
            raise ImportError("numpy is required to deserialize this object")
        return _deserialize_numpy(data)
    if data_type == _TYPE_ARROW_TABLE:
        return _deserialize_arrow_table(data)
    if data_type == _TYPE_ARROW_BATCH:
        return _deserialize_arrow_batch(data)
    if data_type == _TYPE_ARROW_ARRAY:
        return _deserialize_arrow_array(data)
    if data_type == _TYPE_PANDAS_DATAFRAME:
        if not _HAS_PANDAS:
            raise ImportError("pandas is required to deserialize this object")
        return _deserialize_arrow_table(data).to_pandas()
    if data_type == _TYPE_POLARS_DATAFRAME:
        if not _HAS_POLARS:
            raise ImportError("polars is required to deserialize this object")
        return pl.from_arrow(_deserialize_arrow_table(data))
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
    return pa.Codec(_COMPRESSION_ZSTD).compress(data, asbytes=True)


def _decompress_data(data: bytes, compression: str) -> bytes:
    if _validate_compression(compression) == _COMPRESSION_NONE:
        return data
    with pa.CompressedInputStream(pa.BufferReader(data), _COMPRESSION_ZSTD) as stream:
        return stream.read()


def _encode_object_data(obj: Any) -> tuple[str, bytes]:
    """Compress structured values and tensors; leave generic Python objects raw."""
    data_type, data = _serialize_object_data(obj)
    torch_module = sys.modules.get("torch")
    tensor_type = getattr(torch_module, "Tensor", None)
    is_tensor = isinstance(tensor_type, type) and isinstance(obj, tensor_type)
    is_numpy = _HAS_NUMPY and isinstance(obj, np.ndarray)
    if data_type in _ZSTD_OBJECT_TYPES or is_numpy or is_tensor:
        return f"{data_type}.zstd", _compress_data(data, _COMPRESSION_ZSTD)
    return data_type, data


_client_pool: Dict[tuple[str, Optional[str]], cache_pb2_grpc.ObjectCacheServiceStub] = {}
_client_pool_lock = threading.Lock()

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


def _create_cache_client(
    location: str,
    tls_config: Optional[FlameClientTls] = None,
    authority: Optional[str] = None,
) -> cache_pb2_grpc.ObjectCacheServiceStub:
    parsed = urlparse(location)
    if parsed.scheme not in ("grpc", "grpcs", "grpc+tls", "grpcs-proxy") or not parsed.netloc:
        raise ValueError(f"Invalid object cache endpoint: {location}")
    options = list(GRPC_OPTIONS)
    if authority:
        options.append(("grpc.default_authority", authority))
    if parsed.scheme in ("grpcs", "grpc+tls", "grpcs-proxy"):
        roots = None
        if tls_config and tls_config.ca_file:
            with open(tls_config.ca_file, "rb") as f:
                roots = f.read()
        credentials = grpc.ssl_channel_credentials(root_certificates=roots)
        channel = grpc.secure_channel(parsed.netloc, credentials, options=options)
    else:
        channel = grpc.insecure_channel(parsed.netloc, options=options)
    return cache_pb2_grpc.ObjectCacheServiceStub(channel)


def _cache_proxy_endpoint() -> Optional[str]:
    """Return the configured TLS gRPC proxy, if any."""
    try:
        cache_config = _get_cached_context().cache
    except Exception:
        return None
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
    """Resolve an object endpoint to its dial location and gRPC authority."""
    parsed = urlparse(endpoint)
    if parsed.scheme == "grpc-proxy":
        raise ValueError("grpc-proxy:// is unsupported; use grpcs-proxy://")
    if parsed.scheme == "grpcs-proxy":
        _validate_proxy_endpoint(endpoint)
    proxy_endpoint = _cache_proxy_endpoint()
    if parsed.scheme in ("grpc", "grpcs", "grpc+tls") and proxy_endpoint:
        if not parsed.netloc:
            raise ValueError(f"Invalid object cache endpoint: {endpoint}")
        return proxy_endpoint, parsed.netloc
    return endpoint, None


def _get_cache_client(endpoint: str, tls_config: Optional[FlameClientTls] = None) -> cache_pb2_grpc.ObjectCacheServiceStub:
    location, authority = _resolve_cache_endpoint(endpoint)
    pool_key = (location, authority)
    with _client_pool_lock:
        if pool_key not in _client_pool:
            _client_pool[pool_key] = _create_cache_client(location, tls_config, authority)
        return _client_pool[pool_key]


def _write_requests(key: str, data_type: str, chunks: Any):
    yield cache_pb2.CacheWriteRequest(header=cache_pb2.CacheWriteHeader(key=key, data_type=data_type))
    for chunk in chunks:
        yield cache_pb2.CacheWriteRequest(data=chunk)


def _write_remote(client: cache_pb2_grpc.ObjectCacheServiceStub, key: str, data_type: str, chunks: Any, patch: bool = False, timeout: Optional[int] = None) -> ObjectRef:
    rpc = client.Patch if patch else client.Put
    metadata = rpc(_write_requests(key, data_type, chunks), timeout=timeout)
    return ObjectRef(endpoint=metadata.endpoint, key=metadata.key, version=metadata.version)


def _byte_chunks(data: bytes):
    for start in range(0, len(data), _UPLOAD_CHUNK_SIZE):
        yield data[start : start + _UPLOAD_CHUNK_SIZE]


def _get_cache_tls_config() -> Optional[FlameClientTls]:
    """Get TLS configuration for cache from FlameContext.

    FlameContext automatically handles:
    1. Loading from ~/.flame/flame.yaml if it exists
    2. Building from environment variables (FLAME_CA_FILE, etc.) if no config file

    Returns:
        FlameClientTls if configured, None otherwise
    """
    try:
        context = _get_cached_context()
        cache_config = context.cache
        if isinstance(cache_config, FlameClientCache) and cache_config.tls:
            return cache_config.tls
    except Exception:
        pass
    return None


def put_object(key_prefix: str, obj: Any) -> "ObjectRef":
    """Put an object into the cache.

    Args:
        key_prefix: Key prefix in format "<app>/<session>"
        obj: The object to cache

    Returns:
        ObjectRef pointing to the cached object

    Raises:
        ValueError: If key_prefix format is invalid or cache not configured
    """

    object_key = ObjectKey.from_prefix(key_prefix)

    context = _get_cached_context()
    cache_config = context.cache

    if cache_config is None:
        raise ValueError("Cache configuration not found")

    if isinstance(cache_config, str):
        cache_endpoint, cache_tls = cache_config, None
    elif isinstance(cache_config, FlameClientCache):
        cache_endpoint, cache_tls = cache_config.endpoint, cache_config.tls
    else:
        cache_endpoint, cache_tls = cache_config.get("endpoint"), None
    if not cache_endpoint:
        raise ValueError("Cache endpoint not configured")

    client = _get_cache_client(cache_endpoint, cache_tls)
    data_type, data = _encode_object_data(obj)
    ref = _write_remote(client, object_key.to_prefix(), data_type, _byte_chunks(data))
    logger.debug("put_object: key=%s, version=%s", ref.key, ref.version)
    return ref


def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    """Get an object from the cache.

    Uses client-side caching with version checking to avoid unnecessary downloads.
    To force a fresh download, set ref.version = 0 before calling.

    Args:
        ref: ObjectRef pointing to the cached object
        deserializer: Optional function to combine base and deltas.
            Signature: (base: Any, deltas: List[Any]) -> Any
            If None, returns just the base object (backward compatible).

    Returns:
        The deserialized object. If deserializer is provided, returns
        deserializer(base, deltas). Otherwise returns the base object.

    Raises:
        ValueError: If key format is invalid or request fails
    """
    ObjectKey.from_key(ref.key)
    cache_key = (ref.endpoint, ref.key)

    if ref.version == 0:
        cached_version = 0
    else:
        cached = _cache_get(cache_key)
        cached_version = cached.version if cached else 0

    logger.debug(f"get_object: key={ref.key}, cached_version={cached_version}")
    result = _fetch_object_data(ref, cached_version)

    if result is None:
        if cached_version > 0:
            cached = _cache_get(cache_key)
            if cached is not None:
                logger.debug(f"get_object: not_modified, returning cached for key={ref.key}")
                return _materialize_object(cached, deserializer)
        logger.error(f"get_object: cache miss after not_modified! key={ref.key}, cached_version={cached_version}")
        raise ValueError(f"Object not found: {ref.key}")

    if result.mode == FetchMode.FULL:
        cached = Object(
            version=result.version,
            data=result.base,
            patches=result.patches,
        )
        _cache_put(cache_key, cached)
    elif result.mode == FetchMode.PATCHES:
        cached = _cache_apply_patches(
            cache_key,
            expected_version=cached_version,
            new_version=result.version,
            patches=result.patches,
        )
        if cached is None:
            full_result = _fetch_object_data(ref, 0)
            if full_result is None or full_result.mode != FetchMode.FULL:
                raise ValueError(f"Object not found: {ref.key}")
            cached = Object(
                version=full_result.version,
                data=full_result.base,
                patches=full_result.patches,
            )
            _cache_put(cache_key, cached)
    else:
        raise ValueError(f"Unexpected object fetch mode: {result.mode}")

    logger.debug(f"get_object: key={ref.key}, version={cached.version}")
    return _materialize_object(cached, deserializer)


def _read_get_parts(responses: Any):
    """Yield a validated header and completed base/patch payloads."""
    iterator = iter(responses)
    try:
        first = next(iterator)
    except StopIteration as exc:
        raise ValueError("Cache Get returned no header") from exc
    if first.WhichOneof("payload") != "header":
        raise ValueError("Cache Get must start with a header")
    header = first.header
    parts = []
    kind = None
    version = None
    data = bytearray()
    for response in iterator:
        if response.WhichOneof("payload") != "chunk":
            raise ValueError("Unexpected Cache Get header after data")
        chunk = response.chunk
        if kind is None:
            kind, version = chunk.kind, chunk.version
        elif (chunk.kind, chunk.version) != (kind, version):
            parts.append((kind, version, bytes(data)))
            kind, version = chunk.kind, chunk.version
            data.clear()
        data.extend(chunk.data)
    if kind is not None:
        parts.append((kind, version, bytes(data)))
    return header, parts


def _fetch_object_data(ref: ObjectRef, cached_version: int) -> Optional[FetchResult]:
    client = _get_cache_client(ref.endpoint, _get_cache_tls_config())
    header, parts = _read_get_parts(client.Get(cache_pb2.CacheGetRequest(key=ref.key, client_version=cached_version)))
    if header.mode == cache_pb2.CACHE_GET_MODE_NOT_MODIFIED:
        if parts:
            raise ValueError("NOT_MODIFIED response included data")
        return None
    if header.mode not in (cache_pb2.CACHE_GET_MODE_FULL, cache_pb2.CACHE_GET_MODE_PATCHES):
        raise ValueError(f"Invalid Cache Get mode: {header.mode}")
    data_type, compression = _type_and_compression(header.data_type)
    if header.mode == cache_pb2.CACHE_GET_MODE_FULL:
        if not parts or parts[0][0] != cache_pb2.CACHE_CHUNK_KIND_BASE:
            raise ValueError("Full object response must start with a base")
        base = _deserialize_object_data(data_type, _decompress_data(parts[0][2], compression))
        patch_parts = parts[1:]
        mode = FetchMode.FULL
    else:
        base = None
        patch_parts = parts
        mode = FetchMode.PATCHES
    if any(kind != cache_pb2.CACHE_CHUNK_KIND_PATCH for kind, _, _ in patch_parts):
        raise ValueError("Cache Get response has an unexpected part kind")
    patch_versions = [version for _, version, _ in patch_parts]
    if patch_versions != sorted(set(patch_versions)):
        raise ValueError("Patch response versions must be unique and increasing")
    patches = [Patch(version=version, data=_deserialize_object_data(data_type, _decompress_data(data, compression))) for _, version, data in patch_parts]
    return FetchResult(mode=mode, version=header.version, base=base, patches=patches)


def update_object(ref: ObjectRef, new_obj: Any) -> "ObjectRef":
    """Update an object in the cache.

    This replaces the entire object (base + all deltas) with the new object as base.

    Args:
        ref: ObjectRef pointing to the cached object to update
        new_obj: The new object to encode and store

    Returns:
        Updated ObjectRef with new version from server

    Raises:
        ValueError: If key format is invalid or request fails
    """
    ObjectKey.from_key(ref.key)

    client = _get_cache_client(ref.endpoint, _get_cache_tls_config())
    data_type, data = _encode_object_data(new_obj)
    new_ref = _write_remote(client, ref.key, data_type, _byte_chunks(data))
    _cache_remove((ref.endpoint, ref.key))
    return new_ref


def patch_object(ref: ObjectRef, delta: Any) -> "ObjectRef":
    """Append delta data to an existing cached object.

    This appends the delta to the object's delta list without modifying the base.
    The delta will be included in subsequent get_object() calls.

    Args:
        ref: ObjectRef pointing to the cached object to patch
        delta: The delta data to encode and append

    Returns:
        Updated ObjectRef with new version from server

    Raises:
        ValueError: If key format invalid or object doesn't exist
    """
    ObjectKey.from_key(ref.key)

    client = _get_cache_client(ref.endpoint, _get_cache_tls_config())
    metadata = client.GetMetadata(cache_pb2.CacheGetMetadataRequest(key=ref.key))
    stored_type, compression = _type_and_compression(metadata.data_type)
    data_type, data = _serialize_object_data(delta)
    if data_type != stored_type:
        raise ValueError(f"Patch data type {data_type!r} does not match cached object type {stored_type!r}")
    new_ref = _write_remote(client, ref.key, metadata.data_type, _byte_chunks(_compress_data(data, compression)), patch=True)
    _cache_remove((ref.endpoint, ref.key))
    return new_ref


def delete_objects(key_prefix: str) -> None:
    """Delete objects matching a key or key prefix from the cache.

    This deletes all objects matching the key or prefix pattern from the server.
    Also clears any matching entries from the client-side cache.

    Args:
        key_prefix: Key in format "<app>/<session>/<object>" or key prefix in
            format "<app>/*" (all sessions) or "<app>/<session>"

    Raises:
        ValueError: If key_prefix format is invalid or cache not configured
    """
    object_key = ObjectKey.from_path(key_prefix)

    context = _get_cached_context()
    cache_config = context.cache

    if cache_config is None:
        raise ValueError("Cache configuration not found")

    if isinstance(cache_config, str):
        cache_endpoint = cache_config
        cache_tls = None
    elif isinstance(cache_config, FlameClientCache):
        cache_endpoint = cache_config.endpoint
        cache_tls = cache_config.tls
    else:
        cache_endpoint = cache_config.get("endpoint")
        cache_tls = None

    if not cache_endpoint:
        raise ValueError("Cache endpoint not configured")

    client = _get_cache_client(cache_endpoint, cache_tls)
    client.Delete(cache_pb2.CacheDeleteRequest(key=str(object_key)))
    _cache_remove_matching(object_key)


_UPLOAD_CHUNK_SIZE = 1024 * 1024  # 1MB


class _ChunkReader(io.RawIOBase):
    """Expose a validated gRPC chunk iterator to Arrow's streaming decoder."""

    def __init__(self, chunks: Any):
        self._chunks = iter(chunks)
        self._pending = memoryview(b"")

    def readable(self) -> bool:
        return True

    def readinto(self, buffer: Any) -> int:
        target = memoryview(buffer)
        if not target:
            return 0
        while not self._pending:
            try:
                self._pending = memoryview(next(self._chunks))
            except StopIteration:
                return 0
        size = min(len(target), len(self._pending))
        target[:size] = self._pending[:size]
        self._pending = self._pending[size:]
        return size


def upload_object(key_or_prefix: str, file_path: str, endpoint: Optional[str] = None) -> ObjectRef:
    """Upload a file to the cache using streaming gRPC.

    Args:
        key_or_prefix: Either full key (e.g., "myapp/pkg/myapp-1.0.0.tar.gz")
                       or key prefix (e.g., "myapp/pkg"). If prefix, server
                       generates a UUID for the object_id.
        file_path: Path to the local file to upload
        endpoint: Optional cache endpoint override. When omitted, the endpoint
                  is loaded from the current Flame context.

    Returns:
        ObjectRef pointing to the uploaded file

    Raises:
        ValueError: If cache not configured or upload fails
        FileNotFoundError: If file_path does not exist
    """
    import os

    if not os.path.exists(file_path):
        raise FileNotFoundError(f"File not found: {file_path}")

    object_key = ObjectKey.from_path(key_or_prefix)
    if object_key.is_all_sessions():
        raise ValueError(f"Invalid key format: {key_or_prefix}")

    if endpoint is None:
        cache_config = _get_cached_context().cache
    else:
        # An explicit endpoint is sufficient for public-root TLS and plaintext
        # connections. Reuse context TLS settings when available, but do not
        # make an otherwise self-contained endpoint depend on global config.
        try:
            cache_config = _get_cached_context().cache
        except Exception:
            cache_config = None

    if cache_config is None and endpoint is None:
        raise ValueError("Cache configuration not found")

    cache_tls = cache_config.tls if isinstance(cache_config, FlameClientCache) else None
    if endpoint is not None:
        cache_endpoint = endpoint
    elif isinstance(cache_config, str):
        cache_endpoint = cache_config
    elif isinstance(cache_config, FlameClientCache):
        cache_endpoint = cache_config.endpoint
    else:
        cache_endpoint = cache_config.get("endpoint")

    if not cache_endpoint:
        raise ValueError("Cache endpoint not configured")

    client = _get_cache_client(cache_endpoint, cache_tls)
    file_size = os.path.getsize(file_path)

    def chunks():
        with open(file_path, "rb") as source:
            while chunk := source.read(_UPLOAD_CHUNK_SIZE):
                yield chunk

    try:
        ref = _write_remote(client, str(object_key), _TYPE_RAW, chunks(), timeout=300)
        logger.debug("upload_object: key=%s, version=%s, size=%s", ref.key, ref.version, file_size)
        return ref
    except Exception as exc:
        raise ValueError(f"Failed to upload file to cache server: {exc}") from exc


def download_object(ref: ObjectRef, dest_path: str) -> None:
    """Download a file from the cache using streaming gRPC.

    Args:
        ref: ObjectRef pointing to the cached file
        dest_path: Local path to save the downloaded file

    Raises:
        ValueError: If object not found or download fails
    """
    import os

    ObjectKey.from_key(ref.key)
    client = _get_cache_client(ref.endpoint, _get_cache_tls_config())
    dest_dir = os.path.dirname(dest_path)
    if dest_dir:
        os.makedirs(dest_dir, exist_ok=True)

    try:
        responses = iter(client.Get(cache_pb2.CacheGetRequest(key=ref.key, client_version=0)))
        first = next(responses)
        if first.WhichOneof("payload") != "header" or first.header.mode != cache_pb2.CACHE_GET_MODE_FULL:
            raise ValueError("Expected full object response")
        data_type, compression = _type_and_compression(first.header.data_type)
        saw_base = False

        def chunks():
            nonlocal saw_base
            for response in responses:
                if response.WhichOneof("payload") != "chunk":
                    raise ValueError("Unexpected Cache Get header")
                chunk = response.chunk
                if chunk.kind != cache_pb2.CACHE_CHUNK_KIND_BASE or data_type != _TYPE_RAW:
                    raise ValueError("download_object expected raw base chunks only")
                saw_base = True
                yield chunk.data

        with open(dest_path, "wb") as output:
            if compression == _COMPRESSION_NONE:
                for chunk in chunks():
                    output.write(chunk)
            else:
                with pa.CompressedInputStream(pa.input_stream(_ChunkReader(chunks())), _COMPRESSION_ZSTD) as stream:
                    while data := stream.read(_UPLOAD_CHUNK_SIZE):
                        output.write(data)
        if not saw_base:
            raise ValueError("Cache Get response omitted the base")
    except Exception as exc:
        if os.path.exists(dest_path):
            os.remove(dest_path)
        raise ValueError(f"Failed to download file from cache server: {exc}") from exc

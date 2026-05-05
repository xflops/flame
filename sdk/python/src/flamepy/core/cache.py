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

import logging
import threading
import uuid
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

import bson
import cloudpickle
import pyarrow as pa
import pyarrow.flight as flight

from flamepy.core.types import FlameClientCache, FlameClientTls, FlameContext

logger = logging.getLogger(__name__)

Deserializer = Callable[[Any, List[Any]], Any]

WILDCARD_SESSION = "*"


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
class Object:
    """Cached object with version and deserialized data.

    Note: Stores deserialized data to avoid repeated deserialization.
    Different deserializers on the same cached object are not supported.
    """

    version: int
    data: Any


# Client-side LRU cache with max size limit
_CACHE_MAX_SIZE = 1000  # Max number of cached objects
_object_cache: Dict[tuple, Object] = {}
_cache_access_order: List[tuple] = []  # LRU tracking: most recent at end
_cache_lock = threading.Lock()


def _cache_get(key: tuple) -> Optional[Object]:
    """Get from cache and update LRU order."""
    with _cache_lock:
        obj = _object_cache.get(key)
        if obj is not None and key in _cache_access_order:
            _cache_access_order.remove(key)
            _cache_access_order.append(key)
        return obj


def _cache_put(key: tuple, obj: Object) -> None:
    """Put to cache with LRU eviction."""
    with _cache_lock:
        if key in _object_cache:
            _cache_access_order.remove(key)
        _object_cache[key] = obj
        _cache_access_order.append(key)

        while len(_object_cache) > _CACHE_MAX_SIZE:
            oldest_key = _cache_access_order.pop(0)
            _object_cache.pop(oldest_key, None)


def _cache_remove(key: tuple) -> None:
    """Remove from cache."""
    with _cache_lock:
        _object_cache.pop(key, None)
        if key in _cache_access_order:
            _cache_access_order.remove(key)


def _cache_remove_prefix(prefix: str) -> None:
    """Remove all entries matching prefix."""
    with _cache_lock:
        keys_to_remove = [k for k in _object_cache if k[1].startswith(prefix)]
        for key in keys_to_remove:
            _object_cache.pop(key, None)
            if key in _cache_access_order:
                _cache_access_order.remove(key)


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
            if ".." in self.object_id or "\\" in self.object_id:
                raise ValueError(f"object_id contains invalid characters: '{self.object_id}'")

    @classmethod
    def from_prefix(cls, prefix: str) -> "ObjectKey":
        """Parse from key prefix '<app>/<ssn>'."""
        parts = prefix.split("/")
        if len(parts) != 2:
            raise ValueError(f"Invalid key prefix '{prefix}': expected '<app>/<ssn>' format")
        return cls(app_name=parts[0], session_id=parts[1], object_id=None)

    @classmethod
    def from_key(cls, key: str) -> "ObjectKey":
        """Parse from full key '<app>/<ssn>/<uuid>'."""
        parts = key.split("/")
        if len(parts) != 3:
            raise ValueError(f"Invalid object key '{key}': expected '<app>/<ssn>/<uuid>' format")
        return cls(app_name=parts[0], session_id=parts[1], object_id=parts[2])

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

    def __str__(self) -> str:
        return self.to_key() or self.to_prefix()


def _serialize_object(obj: Any) -> pa.RecordBatch:
    """Serialize a Python object to an Arrow RecordBatch.

    Args:
        obj: The object to serialize

    Returns:
        RecordBatch with schema {version: uint64, data: binary}
    """
    # Serialize the object using cloudpickle
    data_bytes = cloudpickle.dumps(obj, protocol=cloudpickle.DEFAULT_PROTOCOL)

    # Create Arrow schema
    schema = pa.schema(
        [
            pa.field("version", pa.uint64()),
            pa.field("data", pa.binary()),
        ]
    )

    # Create RecordBatch
    version_array = pa.array([0], type=pa.uint64())
    data_array = pa.array([data_bytes], type=pa.binary())

    batch = pa.RecordBatch.from_arrays([version_array, data_array], schema=schema)

    return batch


def _deserialize_object(batch: pa.RecordBatch) -> Any:
    """Deserialize a Python object from an Arrow RecordBatch.

    Args:
        batch: RecordBatch with schema {version: uint64, data: binary}

    Returns:
        The deserialized object
    """
    # Extract data from the batch
    data_array = batch.column("data")
    data_bytes = data_array[0].as_py()

    # Deserialize using cloudpickle
    return cloudpickle.loads(data_bytes)


_client_pool: Dict[str, flight.FlightClient] = {}
_client_pool_lock = threading.Lock()


def _get_flight_client(endpoint: str, tls_config: Optional[FlameClientTls] = None) -> flight.FlightClient:
    if endpoint.startswith("grpcs://"):
        location = endpoint.replace("grpcs://", "grpc+tls://")
    else:
        location = endpoint

    with _client_pool_lock:
        if location in _client_pool:
            return _client_pool[location]

        if location.startswith("grpc+tls://"):
            if tls_config and tls_config.ca_file:
                with open(tls_config.ca_file, "rb") as f:
                    root_certs = f.read()
                client = flight.FlightClient(location, tls_root_certs=root_certs)
            else:
                client = flight.FlightClient(location)
        else:
            client = flight.FlightClient(location)

        _client_pool[location] = client
        return client


def _do_put_remote(client: flight.FlightClient, descriptor: flight.FlightDescriptor, batch: pa.RecordBatch) -> "ObjectRef":
    """Perform a remote do_put operation and read the result metadata.

    Args:
        client: Arrow Flight client
        descriptor: Flight descriptor for the put operation
        batch: RecordBatch to upload

    Returns:
        ObjectRef received from the server

    Raises:
        ValueError: If metadata cannot be read from server
    """
    writer, reader = client.do_put(descriptor, batch.schema)

    # Write batch
    writer.write_batch(batch)

    # Signal we're done writing
    writer.done_writing()

    # Read result metadata from PutResult stream before closing
    # Read metadata messages using read() method (returns Buffer/bytes)
    try:
        while True:
            metadata_buffer = reader.read()
            if metadata_buffer is None:
                break
            # Extract ObjectRef from metadata buffer (BSON format)
            obj_ref_data = bson.decode(bytes(metadata_buffer))
            writer.close()
            return ObjectRef(
                endpoint=obj_ref_data["endpoint"],
                key=obj_ref_data["key"],
                version=obj_ref_data["version"],
            )
    except Exception as e:
        writer.close()
        raise ValueError(f"Failed to read metadata from cache server: {e}")

    # If we get here, no PutResult was received
    writer.close()
    raise ValueError("No result metadata received from cache server")


def _get_cache_tls_config() -> Optional[FlameClientTls]:
    """Get TLS configuration for cache from FlameContext.

    FlameContext automatically handles:
    1. Loading from ~/.flame/flame.yaml if it exists
    2. Building from environment variables (FLAME_CA_FILE, etc.) if no config file

    Returns:
        FlameClientTls if configured, None otherwise
    """
    try:
        context = FlameContext()
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
        obj: The object to cache (will be pickled)

    Returns:
        ObjectRef pointing to the cached object

    Raises:
        ValueError: If key_prefix format is invalid or cache not configured
    """

    object_key = ObjectKey.from_prefix(key_prefix)

    context = FlameContext()
    cache_config = context.cache

    if cache_config is None:
        raise ValueError("Cache configuration not found")

    if isinstance(cache_config, str):
        cache_endpoint = cache_config
        cache_storage = None
        cache_tls = None
    elif isinstance(cache_config, FlameClientCache):
        cache_endpoint = cache_config.endpoint
        cache_storage = cache_config.storage
        cache_tls = cache_config.tls
    else:
        cache_endpoint = cache_config.get("endpoint")
        cache_storage = cache_config.get("storage")
        cache_tls = None

    if not cache_endpoint:
        raise ValueError("Cache endpoint not configured")

    batch = _serialize_object(obj)

    storage_path: Optional[Path] = None
    use_local_storage = False

    if cache_storage:
        storage_path = Path(cache_storage)
        try:
            storage_path.mkdir(parents=True, exist_ok=True)
            use_local_storage = storage_path.exists() and storage_path.is_dir()
        except (PermissionError, OSError):
            use_local_storage = False

    if use_local_storage and storage_path is not None:
        object_key_with_id = object_key.with_generated_id()
        key = object_key_with_id.to_key()
        if key is None:
            raise ValueError("Failed to generate object key")

        app_session_dir = storage_path / object_key.app_name / object_key.session_id
        app_session_dir.mkdir(parents=True, exist_ok=True)

        object_path = app_session_dir / f"{object_key_with_id.object_id}.arrow"
        writer = pa.ipc.new_file(object_path, batch.schema)
        writer.write_batch(batch)
        writer.close()

        client = _get_flight_client(cache_endpoint, cache_tls)
        descriptor = flight.FlightDescriptor.for_path(key)
        flight_info = client.get_flight_info(descriptor)

        if flight_info.endpoints:
            remote_endpoint = flight_info.endpoints[0].locations[0]
            endpoint_str = remote_endpoint.uri.decode("utf-8") if isinstance(remote_endpoint.uri, bytes) else str(remote_endpoint.uri)
        else:
            endpoint_str = cache_endpoint

        logger.debug(f"put_object local_storage: key={key}, endpoint={endpoint_str}")
        return ObjectRef(endpoint=endpoint_str, key=key, version=0)
    else:
        client = _get_flight_client(cache_endpoint, cache_tls)
        upload_descriptor = flight.FlightDescriptor.for_path(object_key.to_prefix())
        ref = _do_put_remote(client, upload_descriptor, batch)
        logger.debug(f"put_object remote: key={ref.key}, version={ref.version}")
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
    result = _fetch_object_data(ref, cached_version, deserializer)

    if result is None:
        if cached_version > 0:
            cached = _cache_get(cache_key)
            if cached is not None:
                logger.debug(f"get_object: not_modified, returning cached for key={ref.key}")
                return cached.data
        logger.error(f"get_object: cache miss after not_modified! key={ref.key}, cached_version={cached_version}")
        raise ValueError(f"Object not found: {ref.key}")

    data, version = result
    _cache_put(cache_key, Object(version=version, data=data))

    logger.debug(f"get_object: key={ref.key}, version={version}")
    return data


def _fetch_object_data(ref: ObjectRef, cached_version: int, deserializer: Optional[Deserializer] = None) -> Optional[tuple[Any, int]]:
    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)

    ticket_str = f"{ref.key}:{cached_version}"
    ticket = flight.Ticket(ticket_str.encode())
    reader = client.do_get(ticket)

    table = reader.read_all()
    if table.num_rows == 0:
        return None

    batches = table.to_batches()
    base = _deserialize_object(batches[0])
    version = batches[0].column("version")[0].as_py()

    if deserializer is not None:
        deltas = [_deserialize_object(batch) for batch in batches[1:]]
        data = deserializer(base, deltas)
    else:
        data = base

    return data, version


def update_object(ref: ObjectRef, new_obj: Any) -> "ObjectRef":
    """Update an object in the cache.

    This replaces the entire object (base + all deltas) with the new object as base.

    Args:
        ref: ObjectRef pointing to the cached object to update
        new_obj: The new object to store (will be pickled)

    Returns:
        Updated ObjectRef with new version from server

    Raises:
        ValueError: If key format is invalid or request fails
    """
    ObjectKey.from_key(ref.key)

    batch = _serialize_object(new_obj)

    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)

    upload_descriptor = flight.FlightDescriptor.for_path(ref.key)
    new_ref = _do_put_remote(client, upload_descriptor, batch)

    _cache_remove((ref.endpoint, ref.key))

    return new_ref


def patch_object(ref: ObjectRef, delta: Any) -> "ObjectRef":
    """Append delta data to an existing cached object.

    This appends the delta to the object's delta list without modifying the base.
    The delta will be included in subsequent get_object() calls.

    Args:
        ref: ObjectRef pointing to the cached object to patch
        delta: The delta data to append (will be pickled)

    Returns:
        Updated ObjectRef with new version from server

    Raises:
        ValueError: If key format invalid or object doesn't exist
    """
    ObjectKey.from_key(ref.key)

    batch = _serialize_object(delta)

    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)

    upload_descriptor = flight.FlightDescriptor.for_command(f"PATCH:{ref.key}".encode())
    new_ref = _do_put_remote(client, upload_descriptor, batch)

    _cache_remove((ref.endpoint, ref.key))

    return new_ref


def delete_objects(key_prefix: str) -> None:
    """Delete all objects under a key prefix from the cache.

    This deletes all objects matching the prefix pattern from the server.
    Also clears any matching entries from the client-side cache.

    Args:
        key_prefix: Key prefix in format "<app>/*" (all sessions) or "<app>/<session>"

    Raises:
        ValueError: If key_prefix format is invalid or cache not configured
    """
    context = FlameContext()
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

    client = _get_flight_client(cache_endpoint, cache_tls)

    action = flight.Action("DELETE", key_prefix.encode("utf-8"))
    results = list(client.do_action(action))

    if not results:
        raise ValueError("No result received from DELETE action")

    if key_prefix.endswith("/*"):
        app_prefix = key_prefix[:-1]
    else:
        app_prefix = f"{key_prefix}/"

    _cache_remove_prefix(app_prefix)


_UPLOAD_CHUNK_SIZE = 1024 * 1024  # 1MB


def upload_object(key_or_prefix: str, file_path: str) -> ObjectRef:
    """Upload a file to the cache using do_put with streaming.

    Args:
        key_or_prefix: Either full key (e.g., "myapp/pkg/myapp-1.0.0.tar.gz")
                       or key prefix (e.g., "myapp/pkg"). If prefix, server
                       generates a UUID for the object_id.
        file_path: Path to the local file to upload

    Returns:
        ObjectRef pointing to the uploaded file

    Raises:
        ValueError: If cache not configured or upload fails
        FileNotFoundError: If file_path does not exist
    """
    import os

    if not os.path.exists(file_path):
        raise FileNotFoundError(f"File not found: {file_path}")

    parts = key_or_prefix.split("/")
    if len(parts) == 2:
        ObjectKey.from_prefix(key_or_prefix)
    elif len(parts) == 3:
        ObjectKey.from_key(key_or_prefix)
    else:
        raise ValueError(f"Invalid key format: {key_or_prefix}")

    context = FlameContext()
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

    schema = pa.schema(
        [
            pa.field("version", pa.uint64()),
            pa.field("data", pa.binary()),
        ]
    )

    client = _get_flight_client(cache_endpoint, cache_tls)
    descriptor = flight.FlightDescriptor.for_path(key_or_prefix)
    writer, reader = client.do_put(descriptor, schema)

    file_size = os.path.getsize(file_path)
    try:
        with open(file_path, "rb") as f:
            while True:
                chunk = f.read(_UPLOAD_CHUNK_SIZE)
                if not chunk:
                    break

                batch = pa.RecordBatch.from_arrays(
                    [pa.array([0], type=pa.uint64()), pa.array([chunk], type=pa.binary())],
                    schema=schema,
                )
                writer.write_batch(batch)

        writer.done_writing()

        while True:
            metadata_buffer = reader.read()
            if metadata_buffer is None:
                break
            obj_ref_data = bson.decode(bytes(metadata_buffer))
            writer.close()
            ref = ObjectRef(
                endpoint=obj_ref_data["endpoint"],
                key=obj_ref_data["key"],
                version=obj_ref_data["version"],
            )
            logger.debug(f"upload_object: key={ref.key}, version={ref.version}, size={file_size}")
            return ref
    except Exception as e:
        writer.close()
        raise ValueError(f"Failed to upload file to cache server: {e}")

    writer.close()
    raise ValueError("No result metadata received from cache server")


def download_object(ref: ObjectRef, dest_path: str) -> None:
    """Download a file from the cache using do_get with streaming.

    Args:
        ref: ObjectRef pointing to the cached file
        dest_path: Local path to save the downloaded file

    Raises:
        ValueError: If object not found or download fails
    """
    import os

    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)

    ticket = flight.Ticket(f"{ref.key}:0".encode())
    reader = client.do_get(ticket)

    dest_dir = os.path.dirname(dest_path)
    if dest_dir and not os.path.exists(dest_dir):
        os.makedirs(dest_dir, exist_ok=True)

    total_size = 0
    try:
        with open(dest_path, "wb") as f:
            for batch in reader:
                data_array = batch.column("data")
                for i in range(len(data_array)):
                    chunk = data_array[i].as_py()
                    if chunk:
                        f.write(chunk)
                        total_size += len(chunk)

        if total_size == 0:
            os.remove(dest_path)
            raise ValueError(f"Object not found: {ref.key}")

        logger.debug(f"download_object: key={ref.key} -> {dest_path}, size={total_size}")
    except Exception as e:
        if os.path.exists(dest_path):
            os.remove(dest_path)
        raise ValueError(f"Failed to download file from cache server: {e}")

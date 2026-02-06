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

import uuid
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import bson
import cloudpickle
import pyarrow as pa
import pyarrow.flight as flight

from flamepy.core.types import FlameContext


@dataclass
class ObjectRef:
    """Object reference for remote cached objects."""

    endpoint: str  # Cache server endpoint (e.g., "grpc://127.0.0.1:9090")
    key: str  # Object key in format "application_id/session_id/object_id"
    version: int = 0

    def encode(self) -> bytes:
        data = asdict(self)
        return bson.dumps(data)

    @classmethod
    def decode(cls, json_data: bytes) -> "ObjectRef":
        data = bson.loads(json_data)
        return cls(**data)


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


def _get_flight_client(endpoint: str) -> flight.FlightClient:
    """Create a Flight client from endpoint URL.

    Args:
        endpoint: Cache endpoint (e.g., "grpc://127.0.0.1:9090")

    Returns:
        FlightClient instance
    """
    # Parse endpoint to get location
    # Format: grpc://host:port or grpc+tls://host:port
    location = endpoint.replace("grpc://", "grpc://")
    return flight.FlightClient(location)


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
            obj_ref_data = bson.loads(bytes(metadata_buffer))
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


def put_object(application_id: str, session_id: str, obj: Any) -> "ObjectRef":
    """Put an object into the cache.

    Args:
        application_id: Application ID (required). Uses format "application_id/session_id/object_id".
        session_id: The session ID for the object
        obj: The object to cache (will be pickled)

    Returns:
        ObjectRef pointing to the cached object

    Raises:
        Exception: If cache endpoint is not configured or request fails
    """
    context = FlameContext()
    cache_config = context.cache

    if cache_config is None:
        raise ValueError("Cache configuration not found")

    # Get endpoint and storage from config
    if isinstance(cache_config, str):
        # Legacy format - just endpoint string
        cache_endpoint = cache_config
        cache_storage = None
    else:
        # New format - dict with endpoint and optional storage
        cache_endpoint = cache_config.get("endpoint")
        cache_storage = cache_config.get("storage")

    if not cache_endpoint:
        raise ValueError("Cache endpoint not configured")

    # Serialize object to Arrow RecordBatch
    batch = _serialize_object(obj)

    # Generate object_id
    object_id = str(uuid.uuid4())

    # Check if local storage is configured and accessible
    if cache_storage:
        storage_path = Path(cache_storage)
        # Only use local storage if the path exists or can be created
        try:
            storage_path.mkdir(parents=True, exist_ok=True)
            use_local_storage = storage_path.exists() and storage_path.is_dir()
        except (PermissionError, OSError):
            # Path not accessible, fall back to remote cache
            use_local_storage = False
    else:
        use_local_storage = False

    if use_local_storage:
        # Write to local storage (optimization when client has access to cache filesystem)
        # Format: application_id/session_id/object_id
        key = f"{application_id}/{session_id}/{object_id}"
        # Create application/session directory structure
        application_dir = storage_path / application_id
        session_dir = application_dir / session_id
        session_dir.mkdir(parents=True, exist_ok=True)

        # Write Arrow IPC file
        object_path = session_dir / f"{object_id}.arrow"
        writer = pa.ipc.new_file(object_path, batch.schema)
        writer.write_batch(batch)
        writer.close()

        # Get flight info to construct ObjectRef with cache server's endpoint
        client = _get_flight_client(cache_endpoint)
        descriptor = flight.FlightDescriptor.for_path(key)
        flight_info = client.get_flight_info(descriptor)

        # Extract endpoint from flight info
        if flight_info.endpoints:
            remote_endpoint = flight_info.endpoints[0].locations[0]
            # Extract URI string from Location object
            endpoint_str = remote_endpoint.uri.decode("utf-8") if isinstance(remote_endpoint.uri, bytes) else str(remote_endpoint.uri)
        else:
            endpoint_str = cache_endpoint

        return ObjectRef(endpoint=endpoint_str, key=key, version=0)
    else:
        # Use remote cache via Arrow Flight
        client = _get_flight_client(cache_endpoint)

        # Format: application_id/session_id/object_id
        key_path = f"{application_id}/{session_id}/{object_id}"
        upload_descriptor = flight.FlightDescriptor.for_path(key_path)

        return _do_put_remote(client, upload_descriptor, batch)


def get_object(ref: ObjectRef) -> Any:
    """Get an object from the cache.

    Args:
        ref: ObjectRef pointing to the cached object

    Returns:
        The deserialized object

    Raises:
        Exception: If request fails
    """
    # Connect to cache server using ref.endpoint
    client = _get_flight_client(ref.endpoint)

    # Create ticket from key
    ticket = flight.Ticket(ref.key.encode())

    # Get object data
    reader = client.do_get(ticket)

    table = reader.read_all()
    if table.num_rows == 0:
        raise ValueError(f"No data received for object {ref.key}")

    # The object is stored as a single RecordBatch
    batch = table.to_batches()[0]

    # Deserialize object
    return _deserialize_object(batch)


def update_object(ref: ObjectRef, new_obj: Any) -> "ObjectRef":
    """Update an object in the cache.

    Args:
        ref: ObjectRef pointing to the cached object to update
        new_obj: The new object to store (will be pickled)

    Returns:
        Updated ObjectRef (same as input for now, since version is always 0)

    Raises:
        Exception: If request fails
    """
    # Serialize new object to Arrow RecordBatch
    batch = _serialize_object(new_obj)

    # Connect to cache server
    client = _get_flight_client(ref.endpoint)

    # Parse key to validate format
    # Format: application_id/session_id/object_id
    parts = ref.key.split("/")
    if len(parts) != 3:
        raise ValueError(f"Invalid key format (expected application_id/session_id/object_id): {ref.key}")

    # Use full key in FlightDescriptor to update existing object
    upload_descriptor = flight.FlightDescriptor.for_path(ref.key)
    return _do_put_remote(client, upload_descriptor, batch)

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

Async object-cache transport and operations.

The wire format and client-side version cache are shared with the synchronous
namespace. Channels are owned by the event loop that created them.
"""

import asyncio
import contextlib
import logging
import os
import stat
import tempfile
import threading
from typing import Any, AsyncIterator, Optional
from urllib.parse import urlparse

import grpc

from flamepy.core import cache as common
from flamepy.core.cache import PACKAGE_SESSION, Deserializer, FetchResult, ObjectKey, ObjectRef, Ref, ValueRef
from flamepy.core.types import FlameClientCache, FlameClientTls
from flamepy.proto import cache_pb2, cache_pb2_grpc

logger = logging.getLogger(__name__)

# Explicit close() is required before the owning loop exits. A channel can
# retain its loop, so weak loop keys would not make forgotten channels safe.
_clients: dict[asyncio.AbstractEventLoop, dict[tuple[str, Optional[str], Optional[str]], tuple[grpc.aio.Channel, Any]]] = {}
_clients_lock = threading.Lock()


def _reset_clients_after_fork() -> None:
    global _clients, _clients_lock
    _clients = {}
    _clients_lock = threading.Lock()


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_reset_clients_after_fork)


def _create_client(location: str, tls_config: Optional[FlameClientTls], target: Optional[str]):
    parsed = urlparse(location)
    if parsed.scheme not in ("grpc", "grpcs", "grpc+tls", "grpc-proxy", "grpcs-proxy") or not parsed.netloc:
        raise ValueError(f"Invalid object cache endpoint: {location}")
    if parsed.scheme in ("grpc-proxy", "grpcs-proxy"):
        common._validate_proxy_endpoint(location)
    options = list(common.GRPC_OPTIONS)
    if parsed.scheme in ("grpcs", "grpc+tls", "grpcs-proxy"):
        from flamepy.core.tls import channel_credentials

        channel = grpc.aio.secure_channel(parsed.netloc, channel_credentials(tls_config), options=options)
    else:
        channel = grpc.aio.insecure_channel(parsed.netloc, options=options)
    stub = cache_pb2_grpc.ObjectCacheServiceStub(channel)
    if target is not None:
        stub = _RoutedCacheStub(stub, target)
    return channel, stub


class _RoutedCacheStub:
    """Add the owner route to proxy RPCs without changing TLS authority."""

    def __init__(self, stub: Any, target: str):
        self._stub = stub
        self._target = target

    def __getattr__(self, name: str):
        rpc = getattr(self._stub, name)

        def invoke(*args, **kwargs):
            metadata = list(kwargs.pop("metadata", ()) or ())
            metadata.append(("x-flame-object-cache", self._target))
            return rpc(*args, metadata=metadata, **kwargs)

        return invoke


def _get_client(endpoint: str, tls_config: Optional[FlameClientTls] = None):
    location, target = common._resolve_cache_endpoint(endpoint)
    key = (location, target, tls_config.fingerprint() if tls_config is not None else None)
    with _clients_lock:
        pool = _clients.setdefault(asyncio.get_running_loop(), {})
        if key not in pool:
            pool[key] = _create_client(location, tls_config, target)
        return pool[key][1]


async def close() -> None:
    """Close this event loop's cache channels before the loop stops."""
    with _clients_lock:
        pool = _clients.pop(asyncio.get_running_loop(), {})
    for channel, _ in pool.values():
        await channel.close()


def _endpoint_and_tls(endpoint: Optional[str]):
    """The given cache endpoint, or the configured one, with its TLS settings."""
    if endpoint is None:
        return _configured_endpoint()
    try:
        config = common._get_cached_context().cache
    except Exception:
        config = None
    return endpoint, config.tls if isinstance(config, FlameClientCache) else None


def _configured_endpoint():
    cache_config = common._get_cached_context().cache
    if cache_config is None:
        raise ValueError("Cache configuration not found")
    if isinstance(cache_config, str):
        endpoint, tls = cache_config, None
    elif isinstance(cache_config, FlameClientCache):
        endpoint, tls = cache_config.endpoint, cache_config.tls
    else:
        endpoint, tls = cache_config.get("endpoint"), None
    if not endpoint:
        raise ValueError("Cache endpoint not configured")
    return endpoint, tls


async def _write_remote(client: Any, key: str, data_type: str, chunks: AsyncIterator[bytes], *, size: int, patch: bool = False, timeout: Optional[int] = None) -> ObjectRef:
    source_error = None

    async def requests():
        nonlocal source_error
        yield cache_pb2.CacheWriteRequest(header=cache_pb2.CacheWriteHeader(key=key, data_type=data_type, size=size))
        try:
            async for chunk in chunks:
                yield cache_pb2.CacheWriteRequest(data=chunk)
        except Exception as error:
            source_error = error
            raise

    rpc = client.Patch if patch else client.Put
    try:
        metadata = await rpc(requests(), timeout=timeout)
    except (asyncio.CancelledError, grpc.aio.AioRpcError):
        # gRPC cancels the request when its producer fails. Preserve that
        # producer's error instead of making a local read failure look like
        # cancellation of the caller's task.
        if source_error is not None:
            raise source_error
        raise
    return ObjectRef(endpoint=metadata.endpoint, key=metadata.key, version=metadata.version)


async def _byte_chunks(data: bytes):
    for start in range(0, len(data), common._UPLOAD_CHUNK_SIZE):
        yield data[start : start + common._UPLOAD_CHUNK_SIZE]


async def put_object(key_prefix: str, obj: Any) -> ObjectRef:
    object_key = ObjectKey.from_prefix(key_prefix)
    endpoint, tls = _configured_endpoint()
    data_type, data = await asyncio.to_thread(common._encode_object_data, obj)
    ref = await _write_remote(_get_client(endpoint, tls), object_key.to_prefix(), data_type, _byte_chunks(data), size=len(data))
    logger.debug("put_object: key=%s, version=%s", ref.key, ref.version)
    return ref


async def _read_get_parts(responses: Any):
    responses = responses.__aiter__()
    try:
        first = await responses.__anext__()
    except StopAsyncIteration:
        first = None
    if first is None:
        raise ValueError("Cache Get returned no header")
    if first.WhichOneof("payload") != "header":
        raise ValueError("Cache Get must start with a header")
    if not first.header.HasField("metadata"):
        raise ValueError("Cache Get header omitted metadata")
    metadata = first.header.metadata
    if not metadata.uuid or metadata.version < 1 or not metadata.data_type:
        raise ValueError("Cache Get metadata must identify an object and its committed parts")
    parts = []
    part = None
    data = bytearray()
    async for response in responses:
        if response.WhichOneof("payload") != "chunk":
            raise ValueError("Unexpected Cache Get header after data")
        chunk = response.chunk
        if part is None:
            part = chunk.part
        elif chunk.part != part:
            parts.append((part, bytes(data)))
            part = chunk.part
            data.clear()
        data.extend(chunk.data)
    if part is not None:
        parts.append((part, bytes(data)))
    return metadata, parts


def _decode_parts(metadata: Any, parts: list, requested_version: int = 0, requested_uuid: str = "", mode: int = cache_pb2.OBJECT) -> Optional[FetchResult]:
    """Validate the requested part range before decoding any payload."""
    if mode == cache_pb2.METADATA:
        if parts:
            raise ValueError("Metadata Get must not contain data parts")
        return None
    if requested_uuid and metadata.uuid != requested_uuid:
        raise ValueError("Cache Get returned a different object UUID")
    if mode == cache_pb2.PATCHES:
        if requested_version and not requested_uuid:
            raise ValueError("Incremental Get requires an object UUID")
        if requested_version > metadata.version:
            raise ValueError("Cache Get cursor is ahead of the returned object")
        first = requested_version
    elif mode == cache_pb2.OBJECT:
        first = 0
        if requested_version and metadata.version != requested_version:
            raise ValueError("Object Get did not return the requested prefix")
    else:
        raise ValueError("Unknown Cache Get mode")
    expected = list(range(first, metadata.version))
    if [part for part, _ in parts] != expected:
        raise ValueError("Cache Get parts must be consecutive, start at the requested part, and end at the object version")
    if not parts:
        return None
    data_type, compression = common._type_and_compression(metadata.data_type)
    data = [common._deserialize_object_data(data_type, common._decompress_data(raw, compression)) for _, raw in parts]
    return FetchResult(version=metadata.version, data=data, uuid=metadata.uuid)


async def _fetch_object_data(ref: ObjectRef, cached_version: int, cached_uuid: str = "") -> Optional[FetchResult]:
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    mode = cache_pb2.PATCHES if cached_version else cache_pb2.OBJECT
    metadata, parts = await _read_get_parts(client.Get(cache_pb2.CacheGetRequest(key=ref.key, version=cached_version, uuid=cached_uuid, mode=mode)))
    if metadata.key != ref.key:
        raise ValueError("Cache Get returned metadata for a different key")
    return await asyncio.to_thread(_decode_parts, metadata, parts, cached_version, cached_uuid, mode)


async def _get_metadata(client: Any, key: str):
    metadata, parts = await _read_get_parts(client.Get(cache_pb2.CacheGetRequest(key=key, mode=cache_pb2.METADATA)))
    if metadata.key != key:
        raise ValueError("Cache Get returned metadata for a different key")
    _decode_parts(metadata, parts, mode=cache_pb2.METADATA)
    return metadata


async def get_object(ref: Ref, deserializer: Optional[Deserializer] = None) -> Any:
    if isinstance(ref, ValueRef):
        return ref.value if deserializer is None else await asyncio.to_thread(deserializer, [ref.value])
    if not isinstance(ref, ObjectRef):
        raise TypeError(f"Expected ValueRef or ObjectRef, got {type(ref).__name__}")
    ObjectKey.from_key(ref.key)
    cache_key = (ref.endpoint, ref.key)
    if ref.version == 0:
        common._cache_remove(cache_key)
    while True:
        cached, token = common._cache_snapshot(cache_key)
        cached_version = cached.version if cached else 0
        cached_uuid = cached.uuid if cached else ""
        try:
            result = await _fetch_object_data(ref, cached_version, cached_uuid)
        except grpc.aio.AioRpcError as error:
            if not cached_version or error.code() not in (grpc.StatusCode.INVALID_ARGUMENT, grpc.StatusCode.FAILED_PRECONDITION):
                raise
            # Invalidate only the entry used by this request; a newer fetch may
            # already have installed a replacement object.
            with common._cache_lock:
                if common._object_cache.get(cache_key) is cached and common._cache_tokens.get(cache_key) is token:
                    common._object_cache.pop(cache_key, None)
                    common._cache_tokens.pop(cache_key, None)
            continue
        if result is None:
            if cached is None:
                raise ValueError(f"Object not found: {ref.key}")
            if not common._cache_snapshot_matches(cache_key, cached, token):
                continue
            return await asyncio.to_thread(common._materialize_object, cached, deserializer)
        published = common._cache_append_parts(cache_key, result, cached_version, expected=cached, token=token)
        if published is not None:
            return await asyncio.to_thread(common._materialize_object, published, deserializer)


async def patch_object(ref: ObjectRef, data: Any) -> ObjectRef:
    ObjectKey.from_key(ref.key)
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    metadata = await _get_metadata(client, ref.key)
    stored_type, compression = common._type_and_compression(metadata.data_type)
    data_type, payload = await asyncio.to_thread(common._serialize_object_data, data)
    if data_type != stored_type:
        raise ValueError(f"Patch data type {data_type!r} does not match cached object type {stored_type!r}")
    payload = await asyncio.to_thread(common._compress_data, payload, compression)
    # The cached parts stay valid: the next get adds what was appended.
    return await _write_remote(client, ref.key, metadata.data_type, _byte_chunks(payload), size=len(payload), patch=True)


async def _upload_file(key: str, file_path: str, endpoint: Optional[str], source=None) -> ObjectRef:
    endpoint, tls = _endpoint_and_tls(endpoint)

    try:
        with contextlib.nullcontext(source) if source is not None else open(file_path, "rb") as handle:
            info = os.fstat(handle.fileno())
            if not stat.S_ISREG(info.st_mode):
                raise ValueError(f"Package source is not a regular file: {file_path}")
            size = info.st_size - handle.tell()
            if size < 0:
                raise ValueError("Package source position is beyond its size")

            async def chunks():
                while data := await asyncio.to_thread(handle.read, common._UPLOAD_CHUNK_SIZE):
                    yield data

            return await _write_remote(_get_client(endpoint, tls), key, common._TYPE_RAW, chunks(), size=size, timeout=300)
    except grpc.RpcError:
        raise
    except Exception as exc:
        raise ValueError(f"Failed to upload file to cache server: {exc}") from exc


async def _download_file(ref: ObjectRef, dest_path: str) -> None:
    ObjectKey.from_key(ref.key)
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    dest_dir = os.path.dirname(dest_path)
    if dest_dir:
        await asyncio.to_thread(os.makedirs, dest_dir, exist_ok=True)
    try:
        responses = client.Get(cache_pb2.CacheGetRequest(key=ref.key, mode=cache_pb2.OBJECT)).__aiter__()
        try:
            first = await responses.__anext__()
        except StopAsyncIteration:
            first = None
        if first is None or first.WhichOneof("payload") != "header":
            raise ValueError("Cache Get must start with a header")
        if not first.header.HasField("metadata") or first.header.metadata.version != 1 or not first.header.metadata.uuid:
            raise ValueError("expected raw data of the first version only")
        data_type, compression = common._type_and_compression(first.header.metadata.data_type)
        saw_base = False
        with open(dest_path, "wb") as output:
            if compression == common._COMPRESSION_NONE:
                async for response in responses:
                    if response.WhichOneof("payload") != "chunk" or response.chunk.part != 0 or data_type != common._TYPE_RAW:
                        raise ValueError("expected raw data of the first version only")
                    saw_base = True
                    await asyncio.to_thread(output.write, response.chunk.data)
            else:
                with tempfile.TemporaryFile() as compressed:
                    async for response in responses:
                        if response.WhichOneof("payload") != "chunk" or response.chunk.part != 0 or data_type != common._TYPE_RAW:
                            raise ValueError("expected raw data of the first version only")
                        saw_base = True
                        await asyncio.to_thread(compressed.write, response.chunk.data)

                    def decompress_file():
                        pa = common._pyarrow()
                        compressed.seek(0)
                        with pa.CompressedInputStream(pa.input_stream(compressed), common._COMPRESSION_ZSTD) as stream:
                            while data := stream.read(common._UPLOAD_CHUNK_SIZE):
                                output.write(data)

                    await asyncio.to_thread(decompress_file)
        if not saw_base:
            raise ValueError("Cache Get response omitted the object data")
    except BaseException as exc:
        if os.path.exists(dest_path):
            await asyncio.to_thread(os.remove, dest_path)
        if not isinstance(exc, Exception):
            raise
        raise ValueError(f"Failed to download file from cache server: {exc}") from exc


def _package_key(app_name: str, filename: str) -> str:
    key = str(ObjectKey(app_name=app_name, session_id=PACKAGE_SESSION, object_id=filename))
    ObjectKey.from_key(key)
    return key


async def upload_package(app_name: str, file_path: str, filename: Optional[str] = None, endpoint: Optional[str] = None) -> ObjectRef:
    """Upload the application package `file_path` as `<app_name>/pkg/<filename>`
    (the file's name by default). Uploading an existing name appends another
    part. Package downloads require one part, so use a new filename for each
    package revision."""
    key = _package_key(app_name, filename or os.path.basename(file_path))
    # Validate and reuse one handle so changing the path cannot switch the source
    # between validation and upload.
    with open(file_path, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError(f"Package source is not a regular file: {file_path}")
        return await _upload_file(key, file_path, endpoint, source)


async def download_package(app_name: str, filename: str, dest_path: str, endpoint: Optional[str] = None) -> None:
    """Download the single-part package `<app_name>/pkg/<filename>` to `dest_path`."""
    endpoint, _ = _endpoint_and_tls(endpoint)
    await _download_file(ObjectRef(endpoint=endpoint, key=_package_key(app_name, filename), version=0), dest_path)


__all__ = [
    "Ref",
    "ValueRef",
    "ObjectRef",
    "ObjectKey",
    "put_object",
    "get_object",
    "patch_object",
    "upload_package",
    "download_package",
    "close",
]

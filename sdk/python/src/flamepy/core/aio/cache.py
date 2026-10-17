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
import logging
import os
import tempfile
import threading
from typing import Any, AsyncIterator, Optional
from urllib.parse import urlparse

import grpc

from flamepy.core import cache as common
from flamepy.core.cache import Deserializer, FetchResult, ObjectKey, ObjectRef, Ref, ValueRef
from flamepy.core.types import FlameClientCache, FlameClientTls
from flamepy.proto import cache_pb2, cache_pb2_grpc

logger = logging.getLogger(__name__)

# Explicit close() is required before the owning loop exits. A channel can
# retain its loop, so weak loop keys would not make forgotten channels safe.
_clients: dict[asyncio.AbstractEventLoop, dict[tuple[str, Optional[str], Optional[str], Optional[str], Optional[str]], tuple[grpc.aio.Channel, Any]]] = {}
_clients_lock = threading.Lock()


def _reset_clients_after_fork() -> None:
    global _clients, _clients_lock
    _clients = {}
    _clients_lock = threading.Lock()


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_reset_clients_after_fork)


def _create_client(location: str, tls_config: Optional[FlameClientTls], target: Optional[str]):
    parsed = urlparse(location)
    if parsed.scheme not in ("grpc", "grpcs", "grpc+tls", "grpcs-proxy") or not parsed.netloc:
        raise ValueError(f"Invalid object cache endpoint: {location}")
    options = list(common.GRPC_OPTIONS)
    if parsed.scheme in ("grpcs", "grpc+tls", "grpcs-proxy"):
        if tls_config is not None:
            tls_config.validate_identity()
        if parsed.scheme == "grpcs-proxy":
            common._validate_proxy_endpoint(location)
        roots = None
        if tls_config and tls_config.ca_file:
            with open(tls_config.ca_file, "rb") as source:
                roots = source.read()
        private_key = certificate_chain = None
        if tls_config and tls_config.cert_file:
            with open(tls_config.key_file, "rb") as source:
                private_key = source.read()
            with open(tls_config.cert_file, "rb") as source:
                certificate_chain = source.read()
        credentials = grpc.ssl_channel_credentials(root_certificates=roots, private_key=private_key, certificate_chain=certificate_chain)
        channel = grpc.aio.secure_channel(parsed.netloc, credentials, options=options)
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
    key = (location, target, tls_config.ca_file if tls_config else None, tls_config.cert_file if tls_config else None, tls_config.key_file if tls_config else None)
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


async def _write_remote(client: Any, key: str, data_type: str, chunks: AsyncIterator[bytes], *, patch: bool = False, timeout: Optional[int] = None) -> ObjectRef:
    async def requests():
        yield cache_pb2.CacheWriteRequest(header=cache_pb2.CacheWriteHeader(key=key, data_type=data_type))
        async for chunk in chunks:
            yield cache_pb2.CacheWriteRequest(data=chunk)

    rpc = client.Patch if patch else client.Put
    metadata = await rpc(requests(), timeout=timeout)
    return ObjectRef(endpoint=metadata.endpoint, key=metadata.key, version=metadata.version)


async def _byte_chunks(data: bytes):
    for start in range(0, len(data), common._UPLOAD_CHUNK_SIZE):
        yield data[start : start + common._UPLOAD_CHUNK_SIZE]


async def put_object(key_prefix: str, obj: Any) -> ObjectRef:
    object_key = ObjectKey.from_prefix(key_prefix)
    endpoint, tls = _configured_endpoint()
    data_type, data = await asyncio.to_thread(common._encode_object_data, obj)
    ref = await _write_remote(_get_client(endpoint, tls), object_key.to_prefix(), data_type, _byte_chunks(data))
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
    header = first.header
    parts = []
    version = None
    data = bytearray()
    async for response in responses:
        if response.WhichOneof("payload") != "chunk":
            raise ValueError("Unexpected Cache Get header after data")
        chunk = response.chunk
        if version is None:
            version = chunk.version
        elif chunk.version != version:
            parts.append((version, bytes(data)))
            version = chunk.version
            data.clear()
        data.extend(chunk.data)
    if version is not None:
        parts.append((version, bytes(data)))
    return header, parts


def _decode_parts(header: Any, parts: list) -> Optional[FetchResult]:
    """Decode the parts of a Get response, one per version, in increasing order.

    No parts means the client already has the current version. Otherwise the
    parts are consecutive and the last one is the current version.
    """
    if not parts:
        return None
    versions = [version for version, _ in parts]
    if versions != list(range(versions[0], versions[0] + len(versions))) or versions[-1] != header.version:
        raise ValueError("Cache Get response versions must be consecutive and end at the version of the object")
    data_type, compression = common._type_and_compression(header.data_type)
    data = [common._deserialize_object_data(data_type, common._decompress_data(raw, compression)) for _, raw in parts]
    return FetchResult(version=header.version, data=data)


async def _fetch_object_data(ref: ObjectRef, cached_version: int) -> Optional[FetchResult]:
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    header, parts = await _read_get_parts(client.Get(cache_pb2.CacheGetRequest(key=ref.key, version=cached_version)))
    return await asyncio.to_thread(_decode_parts, header, parts)


async def get_object(ref: Ref, deserializer: Optional[Deserializer] = None) -> Any:
    if isinstance(ref, ValueRef):
        return ref.value if deserializer is None else await asyncio.to_thread(deserializer, [ref.value])
    if not isinstance(ref, ObjectRef):
        raise TypeError(f"Expected ValueRef or ObjectRef, got {type(ref).__name__}")
    ObjectKey.from_key(ref.key)
    cache_key = (ref.endpoint, ref.key)
    if ref.version == 0:
        # A forced refresh starts over.
        common._cache_remove(cache_key)
    cached = common._cache_get(cache_key)
    cached_version = cached.version if cached else 0
    try:
        result = await _fetch_object_data(ref, cached_version)
    except grpc.aio.AioRpcError as error:
        if not cached_version or error.code() != grpc.StatusCode.INVALID_ARGUMENT:
            raise
        # The cached version is ahead of the object (an invalid version): it is
        # another object of the same key.
        common._cache_remove(cache_key)
        cached_version = 0
        result = await _fetch_object_data(ref, 0)
    if result is None:
        cached = common._cache_get(cache_key) if cached_version else None
        if cached is None:
            raise ValueError(f"Object not found: {ref.key}")
    else:
        cached = common._cache_append_parts(cache_key, result, cached_version)
        if cached is None:
            # The cached object was dropped while fetching, so the data after
            # its version is not enough: get all the data.
            everything = await _fetch_object_data(ref, 0)
            if everything is None:
                raise ValueError(f"Object not found: {ref.key}")
            cached = common._cache_append_parts(cache_key, everything)
    return await asyncio.to_thread(common._materialize_object, cached, deserializer)


async def patch_object(ref: ObjectRef, data: Any) -> ObjectRef:
    ObjectKey.from_key(ref.key)
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    metadata = await client.GetMetadata(cache_pb2.CacheGetMetadataRequest(key=ref.key))
    stored_type, compression = common._type_and_compression(metadata.data_type)
    data_type, payload = await asyncio.to_thread(common._serialize_object_data, data)
    if data_type != stored_type:
        raise ValueError(f"Patch data type {data_type!r} does not match cached object type {stored_type!r}")
    payload = await asyncio.to_thread(common._compress_data, payload, compression)
    # The cached parts stay valid: the next get adds what was appended.
    return await _write_remote(client, ref.key, metadata.data_type, _byte_chunks(payload), patch=True)


async def delete_objects(key_prefix: str, endpoint: Optional[str] = None) -> None:
    object_key = ObjectKey.from_path(key_prefix)
    endpoint, tls = _endpoint_and_tls(endpoint)
    await _get_client(endpoint, tls).Delete(cache_pb2.CacheDeleteRequest(key=str(object_key)))
    common._cache_remove_matching(object_key)


async def _upload_file(key: str, file_path: str, endpoint: Optional[str]) -> ObjectRef:
    endpoint, tls = _endpoint_and_tls(endpoint)

    async def chunks():
        with open(file_path, "rb") as source:
            while data := await asyncio.to_thread(source.read, common._UPLOAD_CHUNK_SIZE):
                yield data

    try:
        return await _write_remote(_get_client(endpoint, tls), key, common._TYPE_RAW, chunks(), timeout=300)
    except Exception as exc:
        raise ValueError(f"Failed to upload file to cache server: {exc}") from exc


async def _download_file(ref: ObjectRef, dest_path: str) -> None:
    ObjectKey.from_key(ref.key)
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    dest_dir = os.path.dirname(dest_path)
    if dest_dir:
        await asyncio.to_thread(os.makedirs, dest_dir, exist_ok=True)
    try:
        responses = client.Get(cache_pb2.CacheGetRequest(key=ref.key, version=0)).__aiter__()
        try:
            first = await responses.__anext__()
        except StopAsyncIteration:
            first = None
        if first is None or first.WhichOneof("payload") != "header":
            raise ValueError("Cache Get must start with a header")
        data_type, compression = common._type_and_compression(first.header.data_type)
        saw_base = False
        with open(dest_path, "wb") as output:
            if compression == common._COMPRESSION_NONE:
                async for response in responses:
                    if response.WhichOneof("payload") != "chunk" or response.chunk.version != 1 or data_type != common._TYPE_RAW:
                        raise ValueError("expected raw data of the first version only")
                    saw_base = True
                    await asyncio.to_thread(output.write, response.chunk.data)
            else:
                with tempfile.TemporaryFile() as compressed:
                    async for response in responses:
                        if response.WhichOneof("payload") != "chunk" or response.chunk.version != 1 or data_type != common._TYPE_RAW:
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


# Application packages are objects of the "pkg" session of their application.
PACKAGE_SESSION = "pkg"


def _package_key(app_name: str, filename: str) -> str:
    key = str(ObjectKey(app_name=app_name, session_id=PACKAGE_SESSION, object_id=filename))
    ObjectKey.from_key(key)
    return key


async def upload_package(app_name: str, file_path: str, filename: Optional[str] = None, endpoint: Optional[str] = None) -> ObjectRef:
    """Upload the application package `file_path` as `<app_name>/pkg/<filename>`
    (the file's name by default). An object is never replaced, so a package of
    the same name is deleted first."""
    if not await asyncio.to_thread(os.path.exists, file_path):
        raise FileNotFoundError(f"File not found: {file_path}")
    key = _package_key(app_name, filename or os.path.basename(file_path))
    await delete_objects(key, endpoint)
    return await _upload_file(key, file_path, endpoint)


async def download_package(app_name: str, filename: str, dest_path: str, endpoint: Optional[str] = None) -> None:
    """Download the application package `<app_name>/pkg/<filename>` to `dest_path`."""
    endpoint, _ = _endpoint_and_tls(endpoint)
    await _download_file(ObjectRef(endpoint=endpoint, key=_package_key(app_name, filename), version=0), dest_path)


async def delete_package(app_name: str, filename: str, endpoint: Optional[str] = None) -> None:
    """Delete the application package `<app_name>/pkg/<filename>`."""
    await delete_objects(_package_key(app_name, filename), endpoint)


__all__ = [
    "Ref",
    "ValueRef",
    "ObjectRef",
    "ObjectKey",
    "put_object",
    "get_object",
    "patch_object",
    "delete_objects",
    "upload_package",
    "download_package",
    "delete_package",
    "close",
]

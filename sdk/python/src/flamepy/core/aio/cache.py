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
import pyarrow as pa

from flamepy.core import cache as common
from flamepy.core.cache import Deserializer, FetchMode, FetchResult, Object, ObjectKey, ObjectRef, Patch
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


def _create_client(location: str, tls_config: Optional[FlameClientTls], authority: Optional[str]):
    parsed = urlparse(location)
    if parsed.scheme not in ("grpc", "grpcs", "grpc+tls", "grpcs-proxy") or not parsed.netloc:
        raise ValueError(f"Invalid object cache endpoint: {location}")
    options = list(common.GRPC_OPTIONS)
    if authority:
        options.append(("grpc.default_authority", authority))
    if parsed.scheme in ("grpcs", "grpc+tls", "grpcs-proxy"):
        roots = None
        if tls_config and tls_config.ca_file:
            with open(tls_config.ca_file, "rb") as source:
                roots = source.read()
        credentials = grpc.ssl_channel_credentials(root_certificates=roots)
        channel = grpc.aio.secure_channel(parsed.netloc, credentials, options=options)
    else:
        channel = grpc.aio.insecure_channel(parsed.netloc, options=options)
    return channel, cache_pb2_grpc.ObjectCacheServiceStub(channel)


def _get_client(endpoint: str, tls_config: Optional[FlameClientTls] = None):
    location, authority = common._resolve_cache_endpoint(endpoint)
    key = (location, authority, tls_config.ca_file if tls_config else None)
    with _clients_lock:
        pool = _clients.setdefault(asyncio.get_running_loop(), {})
        if key not in pool:
            pool[key] = _create_client(location, tls_config, authority)
        return pool[key][1]


async def close() -> None:
    """Close this event loop's cache channels before the loop stops."""
    with _clients_lock:
        pool = _clients.pop(asyncio.get_running_loop(), {})
    for channel, _ in pool.values():
        await channel.close()


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
    kind = version = None
    data = bytearray()
    async for response in responses:
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


def _decode_parts(header: Any, parts: list) -> Optional[FetchResult]:
    if header.mode == cache_pb2.CACHE_GET_MODE_NOT_MODIFIED:
        if parts:
            raise ValueError("NOT_MODIFIED response included data")
        return None
    if header.mode not in (cache_pb2.CACHE_GET_MODE_FULL, cache_pb2.CACHE_GET_MODE_PATCHES):
        raise ValueError(f"Invalid Cache Get mode: {header.mode}")
    data_type, compression = common._type_and_compression(header.data_type)
    if header.mode == cache_pb2.CACHE_GET_MODE_FULL:
        if not parts or parts[0][0] != cache_pb2.CACHE_CHUNK_KIND_BASE:
            raise ValueError("Full object response must start with a base")
        base = common._deserialize_object_data(data_type, common._decompress_data(parts[0][2], compression))
        patch_parts = parts[1:]
        mode = FetchMode.FULL
    else:
        base = None
        patch_parts = parts
        mode = FetchMode.PATCHES
    if any(kind != cache_pb2.CACHE_CHUNK_KIND_PATCH for kind, _, _ in patch_parts):
        raise ValueError("Cache Get response has an unexpected part kind")
    versions = [version for _, version, _ in patch_parts]
    if versions != sorted(set(versions)):
        raise ValueError("Patch response versions must be unique and increasing")
    patches = [Patch(version=version, data=common._deserialize_object_data(data_type, common._decompress_data(data, compression))) for _, version, data in patch_parts]
    return FetchResult(mode=mode, version=header.version, base=base, patches=patches)


async def _fetch_object_data(ref: ObjectRef, cached_version: int) -> Optional[FetchResult]:
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    header, parts = await _read_get_parts(client.Get(cache_pb2.CacheGetRequest(key=ref.key, client_version=cached_version)))
    return await asyncio.to_thread(_decode_parts, header, parts)


async def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    ObjectKey.from_key(ref.key)
    cache_key = (ref.endpoint, ref.key)
    cached = None if ref.version == 0 else common._cache_get(cache_key)
    cached_version = cached.version if cached else 0
    result = await _fetch_object_data(ref, cached_version)
    if result is None:
        cached = common._cache_get(cache_key) if cached_version else None
        if cached is None:
            raise ValueError(f"Object not found: {ref.key}")
    elif result.mode == FetchMode.FULL:
        cached = Object(version=result.version, data=result.base, patches=result.patches)
        cached = common._cache_put(cache_key, cached)
    elif result.mode == FetchMode.PATCHES:
        cached = common._cache_apply_patches(cache_key, cached_version, result.version, result.patches)
        if cached is None:
            full = await _fetch_object_data(ref, 0)
            if full is None or full.mode != FetchMode.FULL:
                raise ValueError(f"Object not found: {ref.key}")
            cached = Object(version=full.version, data=full.base, patches=full.patches)
            cached = common._cache_put(cache_key, cached)
    else:
        raise ValueError(f"Unexpected object fetch mode: {result.mode}")
    if deserializer is None:
        return cached.data
    return await asyncio.to_thread(common._materialize_object, cached, deserializer)


async def update_object(ref: ObjectRef, new_obj: Any) -> ObjectRef:
    ObjectKey.from_key(ref.key)
    data_type, data = await asyncio.to_thread(common._encode_object_data, new_obj)
    updated = await _write_remote(_get_client(ref.endpoint, common._get_cache_tls_config()), ref.key, data_type, _byte_chunks(data))
    common._cache_remove((ref.endpoint, ref.key))
    return updated


async def patch_object(ref: ObjectRef, delta: Any) -> ObjectRef:
    ObjectKey.from_key(ref.key)
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    metadata = await client.GetMetadata(cache_pb2.CacheGetMetadataRequest(key=ref.key))
    stored_type, compression = common._type_and_compression(metadata.data_type)
    data_type, data = await asyncio.to_thread(common._serialize_object_data, delta)
    if data_type != stored_type:
        raise ValueError(f"Patch data type {data_type!r} does not match cached object type {stored_type!r}")
    data = await asyncio.to_thread(common._compress_data, data, compression)
    updated = await _write_remote(client, ref.key, metadata.data_type, _byte_chunks(data), patch=True)
    common._cache_remove((ref.endpoint, ref.key))
    return updated


async def delete_objects(key_prefix: str) -> None:
    object_key = ObjectKey.from_path(key_prefix)
    endpoint, tls = _configured_endpoint()
    await _get_client(endpoint, tls).Delete(cache_pb2.CacheDeleteRequest(key=str(object_key)))
    common._cache_remove_matching(object_key)


async def upload_object(key_or_prefix: str, file_path: str, endpoint: Optional[str] = None) -> ObjectRef:
    if not await asyncio.to_thread(os.path.exists, file_path):
        raise FileNotFoundError(f"File not found: {file_path}")
    object_key = ObjectKey.from_path(key_or_prefix)
    if object_key.is_all_sessions():
        raise ValueError(f"Invalid key format: {key_or_prefix}")
    if endpoint is None:
        endpoint, tls = _configured_endpoint()
    else:
        try:
            config = common._get_cached_context().cache
        except Exception:
            config = None
        tls = config.tls if isinstance(config, FlameClientCache) else None

    async def chunks():
        with open(file_path, "rb") as source:
            while data := await asyncio.to_thread(source.read, common._UPLOAD_CHUNK_SIZE):
                yield data

    try:
        return await _write_remote(_get_client(endpoint, tls), str(object_key), common._TYPE_RAW, chunks(), timeout=300)
    except Exception as exc:
        raise ValueError(f"Failed to upload file to cache server: {exc}") from exc


async def download_object(ref: ObjectRef, dest_path: str) -> None:
    ObjectKey.from_key(ref.key)
    client = _get_client(ref.endpoint, common._get_cache_tls_config())
    dest_dir = os.path.dirname(dest_path)
    if dest_dir:
        await asyncio.to_thread(os.makedirs, dest_dir, exist_ok=True)
    try:
        responses = client.Get(cache_pb2.CacheGetRequest(key=ref.key, client_version=0)).__aiter__()
        try:
            first = await responses.__anext__()
        except StopAsyncIteration:
            first = None
        if first is None or first.WhichOneof("payload") != "header" or first.header.mode != cache_pb2.CACHE_GET_MODE_FULL:
            raise ValueError("Expected full object response")
        data_type, compression = common._type_and_compression(first.header.data_type)
        saw_base = False
        with open(dest_path, "wb") as output:
            if compression == common._COMPRESSION_NONE:
                async for response in responses:
                    if response.WhichOneof("payload") != "chunk" or response.chunk.kind != cache_pb2.CACHE_CHUNK_KIND_BASE or data_type != common._TYPE_RAW:
                        raise ValueError("download_object expected raw base chunks only")
                    saw_base = True
                    await asyncio.to_thread(output.write, response.chunk.data)
            else:
                with tempfile.TemporaryFile() as compressed:
                    async for response in responses:
                        if response.WhichOneof("payload") != "chunk" or response.chunk.kind != cache_pb2.CACHE_CHUNK_KIND_BASE or data_type != common._TYPE_RAW:
                            raise ValueError("download_object expected raw base chunks only")
                        saw_base = True
                        await asyncio.to_thread(compressed.write, response.chunk.data)

                    def decompress_file():
                        compressed.seek(0)
                        with pa.CompressedInputStream(pa.input_stream(compressed), common._COMPRESSION_ZSTD) as stream:
                            while data := stream.read(common._UPLOAD_CHUNK_SIZE):
                                output.write(data)

                    await asyncio.to_thread(decompress_file)
        if not saw_base:
            raise ValueError("Cache Get response omitted the base")
    except BaseException as exc:
        if os.path.exists(dest_path):
            await asyncio.to_thread(os.remove, dest_path)
        if not isinstance(exc, Exception):
            raise
        raise ValueError(f"Failed to download file from cache server: {exc}") from exc


__all__ = ["ObjectRef", "ObjectKey", "put_object", "get_object", "update_object", "patch_object", "delete_objects", "upload_object", "download_object", "close"]

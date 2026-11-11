# Copyright 2026 The Flame Authors.
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""A late server integrity error must not publish partially received objects."""

import asyncio
from types import SimpleNamespace

import cloudpickle
import grpc
import pytest

from flamepy.core import cache as cache_module
from flamepy.core.aio import cache as aio_cache
from flamepy.core.cache import FetchResult, ObjectRef
from flamepy.proto import cache_pb2, cache_pb2_grpc


@pytest.mark.parametrize("cached_base", [False, True])
def test_late_data_loss_does_not_publish_partial_client_cache(monkeypatch, cached_base):
    requested_versions = []
    decoded = []
    final_version = 2 if cached_base else 1
    payload = cloudpickle.dumps([2])
    deserialize = cache_module._deserialize_object_data

    def track_deserialize(data_type, data):
        decoded.append(data)
        return deserialize(data_type, data)

    monkeypatch.setattr(cache_module, "_deserialize_object_data", track_deserialize)

    class Service(cache_pb2_grpc.ObjectCacheServiceServicer):
        fail = True

        async def Get(self, request, context):  # noqa: N802 - gRPC method
            requested_versions.append(request.version)
            yield cache_pb2.CacheGetResponse(header=cache_pb2.CacheGetHeader(metadata=cache_pb2.CacheObjectMetadata(key=request.key, uuid="object-uuid", version=final_version, data_type="cloudpickle")))
            middle = len(payload) // 2
            for chunk in (payload[:middle], payload[middle:]):
                yield cache_pb2.CacheGetResponse(chunk=cache_pb2.CacheGetChunk(part=(final_version) - 1, data=chunk))
            if self.fail:
                await context.abort(grpc.StatusCode.DATA_LOSS, "payload checksum mismatch")

    async def run():
        service = Service()
        server = grpc.aio.server()
        cache_pb2_grpc.add_ObjectCacheServiceServicer_to_server(service, server)
        port = server.add_insecure_port("127.0.0.1:0")
        endpoint = f"grpc://127.0.0.1:{port}"
        monkeypatch.setattr(cache_module, "_get_cached_context", lambda: SimpleNamespace(cache=endpoint))
        ref = ObjectRef(endpoint=endpoint, key="integrity/session/object", version=final_version)
        cache_key = (endpoint, ref.key)
        cache_module._cache_remove(cache_key)
        if cached_base:
            cache_module._cache_append_parts(cache_key, FetchResult(version=1, data=[[1]], uuid="object-uuid"))
        await server.start()
        try:
            with pytest.raises(grpc.aio.AioRpcError) as failure:
                await aio_cache.get_object(ref)
            assert failure.value.code() == grpc.StatusCode.DATA_LOSS
            assert decoded == []
            cached = cache_module._cache_get(cache_key)
            if cached_base:
                assert cached.version == 1
                assert cached.data == [[1]]
            else:
                assert cached is None

            # The failed attempt did not advance the client's version, so its
            # retry asks for the same missing parts and can complete normally.
            service.fail = False
            result = await aio_cache.get_object(ref, lambda parts: [value for part in parts for value in part])
            assert result == ([1, 2] if cached_base else [2])
            assert requested_versions == ([1, 1] if cached_base else [0, 0])
            assert len(decoded) == 1
            assert cache_module._cache_get(cache_key).version == final_version
        finally:
            cache_module._cache_remove(cache_key)
            await aio_cache.close()
            await server.stop(0)

    asyncio.run(run())

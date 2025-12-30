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

import grpc
import grpc.aio
from urllib.parse import urlparse

from .cache_pb2_grpc import ObjectCacheStub
from .cache_pb2 import (PutObjectRequest, GetObjectRequest, DeleteObjectRequest,
                        ObjectMetadata, Object)
from .types import DataExpr, DataLocation


class ObjectCacheClient:

    def __init__(self, url: str):
        """
        Initialize the ObjectCacheClient with an endpoint URL.

        Args:
            url (str): The gRPC server URL, e.g. "localhost:50051"
        """

        self._endpoint = ObjectEndpoint(url)
        self._channel = grpc.aio.insecure_channel(self._endpoint.endpoint())
        self._stub = ObjectCacheStub(self._channel)

    async def put_object(self, name: str, data: bytes) -> ObjectMetadata:
        request = PutObjectRequest(name=name, data=data)
        response = await self._stub.Put(request)
        return response

    async def get_object(self, uuid: str) -> Object:
        request = GetObjectRequest(uuid=uuid)
        response = await self._stub.Get(request)
        return response

    async def delete_object(self, uuid: str) -> bool:
        request = DeleteObjectRequest(uuid=uuid)
        response = await self._stub.Delete(request)
        return response.return_code == 0

    async def update_object(self, object: Object) -> ObjectMetadata:
        request = object
        response = await self._stub.Update(request)
        return response


class ObjectEndpoint:

    def __init__(self, endpoint: str):
        url = urlparse(endpoint)
        self._scheme = url.scheme
        self._host = url.hostname
        self._port = url.port
        self._uuid = url.path
        if self._uuid:
            self._uuid = self._uuid.lstrip("/")

    def endpoint(self) -> str:
        return f"{self._host}:{self._port}"

    def uuid(self) -> str:
        return self._uuid

    def __str__(self):
        return self.endpoint()

    def __repr__(self):
        return self.endpoint()


async def load_data(data_expr: DataExpr) -> bytes:
    if data_expr.location == DataLocation.LOCAL:
        return data_expr.data
    else:
        object_endpoint = ObjectEndpoint(data_expr.url)
        object_cache = ObjectCacheClient(object_endpoint.endpoint())
        object_instance = await object_cache.get_object(object_endpoint.uuid())
        return object_instance.data


async def update_data(data_expr: DataExpr, data: bytes) -> None:
    if data_expr.location == DataLocation.LOCAL:
        data_expr.data = data
    else:
        object_endpoint = ObjectEndpoint(data_expr.url)
        object_cache = ObjectCacheClient(object_endpoint.endpoint())
        object_instance = await object_cache.get_object(object_endpoint.uuid())
        object_instance.data = data
        await object_cache.update_object(object_instance)
        data_expr.data = data

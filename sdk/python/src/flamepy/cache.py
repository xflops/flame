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

import httpx
from pydantic import BaseModel

from .types import DataExpr, DataSource, FlameContext

class Object(BaseModel):
    """Object."""

    version: int
    data: list 

class ObjectMetadata(BaseModel):
    """Object metadata."""

    endpoint: str
    version: int
    size: int


async def put_object(session_id: str, data: bytes) -> "DataExpr":
    """Put an object into the cache."""
    context = FlameContext()
    if context._cache_endpoint is None or data is None:
        return DataExpr(source=DataSource.LOCAL, data=data)

    async with httpx.AsyncClient() as client:
        response = await client.post(
            f"{context._cache_endpoint}/objects/{session_id}", data=data)
        response.raise_for_status()

    metadata = ObjectMetadata.model_validate(response.json())

    return DataExpr(source=DataSource.REMOTE, url=metadata.endpoint, data=data, version=metadata.version)


async def get_object(de: DataExpr) -> "DataExpr":
    """Get an object from the cache."""
    if de.source != DataSource.REMOTE:
        return de

    async with httpx.AsyncClient() as client:
        response = await client.get(de.url)
        response.raise_for_status()

    obj = Object.model_validate(response.json())

    de.data = bytes(obj.data)
    de.version = obj.version

    return de


async def update_object(de: DataExpr) -> "DataExpr":
    """Update an object in the cache."""
    if de.source != DataSource.REMOTE:
        return de

    obj = Object(version = de.version, data = list(de.data))
    data = obj.model_dump_json()

    async with httpx.AsyncClient() as client:
        response = await client.put(de.url, data=data)
        response.raise_for_status()

    metadata = ObjectMetadata.model_validate(response.json())

    de.version = metadata.version

    return de

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

import asyncio
import inspect
import logging
import os
from typing import Any, Optional

import cloudpickle
import uvicorn
from fastapi import FastAPI
from fastapi import Request as FastAPIRequest
from fastapi import Response as FastAPIResponse

from flamepy.core import ObjectRef, get_object, update_object
from flamepy.core.service import (
    FLAME_INSTANCE_ENDPOINT,
    FlameService,
    SessionContext,
    TaskContext,
)
from flamepy.core.service import run as run_service
from flamepy.core.types import TaskOutput

debug_service = None


class FlameInstance(FlameService):
    def __init__(self):
        self._entrypoint = None
        self._parameter = None

        self._object_ref: ObjectRef = None

    def context(self) -> Any:
        """Get the current agent context.

        For agent module: use stored ObjectRef to get from cache, then deserialize.
        """
        if self._object_ref is None:
            return None

        # Get from cache using stored ObjectRef (this also updates the version)
        serialized_ctx = get_object(self._object_ref)
        # Deserialize using cloudpickle
        return cloudpickle.loads(serialized_ctx)

    def update_context(self, data: Any):
        """Update the agent context.

        Note: SessionContext no longer supports updating common_data directly.
        This method updates the ObjectRef in cache, but the update won't be reflected
        in SessionContext until the session is recreated. For persistent updates,
        recreate the session with new common_data on the client side.
        """
        if self._object_ref is None:
            return

        # Serialize the data using cloudpickle
        serialized_data = cloudpickle.dumps(data, protocol=cloudpickle.DEFAULT_PROTOCOL)

        # Update existing ObjectRef with latest version in cache
        self._object_ref = update_object(self._object_ref, serialized_data)

    def entrypoint(self, func):
        logger = logging.getLogger(__name__)
        logger.debug(f"entrypoint: {func.__name__}")

        sig = inspect.signature(func)
        self._entrypoint = func
        assert len(sig.parameters) == 1 or len(sig.parameters) == 0, "Entrypoint must have exactly zero or one parameter"
        for param in sig.parameters.values():
            assert param.kind == inspect.Parameter.POSITIONAL_OR_KEYWORD, "Parameter must be positional or keyword"
            self._parameter = param

    def on_session_enter(self, context: SessionContext):
        logger = logging.getLogger(__name__)
        logger.debug("on_session_enter")

        # Decode the common_data bytes to ObjectRef
        common_data_bytes = context.common_data()
        if common_data_bytes is not None:
            self._object_ref = ObjectRef.decode(common_data_bytes)
        else:
            self._object_ref = None

    def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
        logger = logging.getLogger(__name__)
        logger.debug("on_task_invoke")
        if self._entrypoint is None:
            logger.warning("No entrypoint function defined")
            return None

        # For agent module: receive bytes from core API, deserialize with cloudpickle
        input_data = None
        if context.input is not None:
            input_data = cloudpickle.loads(context.input)

        args = (input_data,) if self._parameter is not None else ()
        if inspect.iscoroutinefunction(self._entrypoint):
            res = asyncio.run(self._entrypoint(*args))
        else:
            res = self._entrypoint(*args)

        logger.debug(f"on_task_invoke: {res}")

        # For agent module: serialize output with cloudpickle, return bytes
        output_bytes = None
        if res is not None:
            output_bytes = cloudpickle.dumps(res, protocol=cloudpickle.DEFAULT_PROTOCOL)

        return TaskOutput(output_bytes)

    def on_session_leave(self):
        logger = logging.getLogger(__name__)
        logger.debug("on_session_leave")

        self._object_ref = None

    def run(self):
        logger = logging.getLogger(__name__)
        try:
            # Run the service
            endpoint = os.getenv(FLAME_INSTANCE_ENDPOINT)
            if endpoint is not None:
                # If the instance was started by executor, run the service.
                logger.info("🚀 Starting Flame Instance")
                logger.info("=" * 50)

                run_service(self)
            else:
                # If the instance was started manually, run the debug service.
                logger.info("🚀 Starting Flame Debug Instance")
                logger.info("=" * 50)

                run_debug_service(self)

        except KeyboardInterrupt:
            logger.info("\n🛑 Server stopped by user")
        except Exception as e:
            logger.error(f"\n❌ Error: {e}")


def run_debug_service(instance: FlameInstance):
    global debug_service
    debug_service = FastAPI()
    debug_service.state.instance = instance

    if instance._entrypoint is not None:
        entrypoint_name = instance._entrypoint.__name__
        debug_service.add_api_route(f"/{entrypoint_name}", entrypoint_local_api, methods=["POST"])

    uvicorn.run(debug_service, host="0.0.0.0", port=5050)


async def entrypoint_local_api(s: FastAPIRequest):
    instance = s.app.state.instance
    body_str = await s.body()

    output = instance.on_task_invoke(
        TaskContext(
            task_id=s.query_params.get("task_id") or "0",
            session_id=s.query_params.get("session_id") or "0",
            input=body_str,
        )
    )

    return FastAPIResponse(status_code=200, content=output.data)

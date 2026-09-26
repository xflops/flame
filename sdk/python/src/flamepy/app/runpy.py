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
from typing import Any, Dict, List, Optional, Tuple

import cloudpickle

import flamepy.core.aio as aio_core
from flamepy.app._context import (
    _bind_invocation_context,
    _stage_response_attributes,
    _take_response_attributes,
)
from flamepy.app.types import ServiceContext, ServiceRequest
from flamepy.core import ObjectRef
from flamepy.core.aio.service import FlameService as AioFlameService
from flamepy.core.service import SessionContext, TaskContext
from flamepy.core.types import TaskOutput
from flamepy.proto.types_pb2 import ExecutorAttributes

logger = logging.getLogger(__name__)

MAX_PARALLEL_RESOLVE = 8


class FlameRunpyService(AioFlameService):
    """
    Common Python service for Flame that executes customized Python applications.

    This service invokes App function and class services without requiring custom
    container images. It accepts positional arguments, keyword arguments, and
    object-cache references.
    """

    def __init__(self):
        """Initialize the FlameRunpyService."""
        self._ssn_ctx: Optional[SessionContext] = None
        self._execution_object: Any = None
        self._class_execution_objects: Dict[str, Any] = {}
        self._app_context: Optional[ServiceContext] = None

    def _take_attributes(self) -> ExecutorAttributes:
        """Take attributes belonging to the invocation assembling this response."""
        attributes = _take_response_attributes()
        if attributes is not None:
            return ExecutorAttributes(attr=list(attributes))
        return super()._take_attributes()

    def _set_execution_from_context(self, service_context: ServiceContext) -> None:
        """Bind a function or retained class object from a ServiceContext."""
        execution_object = service_context.execution_object
        if execution_object is None:
            raise ValueError("Execution object is None in ServiceContext")

        if service_context.constructor_args is not None:
            class_name = f"{execution_object.__module__}.{execution_object.__qualname__}"
            service_key = service_context.service_id or class_name
            if service_key not in self._class_execution_objects:
                logger.info(f"Instantiating class {class_name}")
                self._class_execution_objects[service_key] = execution_object(
                    *service_context.constructor_args,
                    **service_context.constructor_kwargs,
                )
            execution_object = self._class_execution_objects[service_key]

        self._app_context = service_context
        self._execution_object = execution_object

    def _collect_object_refs(self, args: Tuple, kwargs: Dict[str, Any]) -> List[Tuple[str, ObjectRef]]:
        """Collect all ObjectRefs from args and kwargs for parallel resolution.

        Returns (location, object_ref) pairs where location
        is either 'arg:N' for positional args or 'kwarg:key' for keyword args.
        """
        refs = []

        for i, value in enumerate(args):
            if isinstance(value, ObjectRef):
                refs.append((f"arg:{i}", value))
            elif isinstance(value, bytes):
                try:
                    object_ref = ObjectRef.decode(value)
                    refs.append((f"arg:{i}", object_ref))
                except Exception:
                    pass

        for key, value in kwargs.items():
            if isinstance(value, ObjectRef):
                refs.append((f"kwarg:{key}", value))
            elif isinstance(value, bytes):
                try:
                    object_ref = ObjectRef.decode(value)
                    refs.append((f"kwarg:{key}", object_ref))
                except Exception:
                    pass

        return refs

    def _prepare_task(self, context: TaskContext):
        """Validate a task and select its callable and unresolved arguments."""
        execution_object = self._execution_object
        if execution_object is None:
            raise ValueError("Execution object is None. Session may not have been entered properly.")

        logger.debug(f"Execution object type: {type(execution_object)}")

        # Decode the request sent by the App client.
        if context.input is None:
            raise ValueError("Task input is None")

        request = cloudpickle.loads(context.input)
        if not isinstance(request, ServiceRequest):
            raise ValueError(f"Expected ServiceRequest in task input, got {type(request)}")

        # Ensure __post_init__ validation runs after deserialization
        # This validates that args/kwargs are the correct types
        ServiceRequest.__post_init__(request)

        # Validate request structure
        if request.method is not None and not isinstance(request.method, str):
            raise ValueError(f"request.method must be a string or None, got {type(request.method)}")

        logger.debug(f"ServiceRequest: method={request.method}, has_args={request.args is not None}, has_kwargs={request.kwargs is not None}")

        # Preserve ObjectRefs for the aio cache resolver.
        raw_args = ()
        raw_kwargs = {}

        if request.args is not None:
            if not isinstance(request.args, (tuple, list)):
                raise ValueError(f"request.args must be a tuple or list, got {type(request.args)}: {request.args}")
            raw_args = tuple(request.args)

        if request.kwargs is not None:
            if not isinstance(request.kwargs, dict):
                raise ValueError(f"request.kwargs must be a dict, got {type(request.kwargs)}: {request.kwargs}")
            raw_kwargs = dict(request.kwargs)

        # Select the function or class method for this request.
        session_context = self._ssn_ctx
        if session_context is None:
            raise ValueError("Session context is not available")
        if context.session_id != session_context.session_id:
            raise ValueError(f"Task session '{context.session_id}' does not match bound session '{session_context.session_id}'")
        if request.method is None:
            if not callable(execution_object):
                raise ValueError(f"Function service is not callable: {type(execution_object)}")
            method = execution_object
        else:
            if not hasattr(execution_object, request.method):
                raise ValueError(f"Execution object has no method '{request.method}'")
            method = getattr(execution_object, request.method)
            if not callable(method):
                raise ValueError(f"Attribute '{request.method}' is not callable")
        return method, raw_args, raw_kwargs, session_context

    async def _resolve_object_refs(self, args: Tuple, kwargs: Dict[str, Any]) -> Tuple[Tuple, Dict[str, Any]]:
        """Fetch App arguments concurrently through the aio object cache."""
        refs = self._collect_object_refs(args, kwargs)
        if not refs:
            return args, kwargs

        limit = asyncio.Semaphore(MAX_PARALLEL_RESOLVE)

        async def fetch(location, object_ref):
            async with limit:
                try:
                    return location, await aio_core.get_object(object_ref)
                except Exception as exc:
                    raise ValueError(f"Failed to resolve ObjectRef at {location}: {exc}") from exc

        fetches = [asyncio.create_task(fetch(location, ref)) for location, ref in refs]
        try:
            values = dict(await asyncio.gather(*fetches))
        except (Exception, asyncio.CancelledError):
            for pending in fetches:
                pending.cancel()
            await asyncio.gather(*fetches, return_exceptions=True)
            raise
        resolved_args = tuple(values.get(f"arg:{index}", value) for index, value in enumerate(args))
        resolved_kwargs = {key: values.get(f"kwarg:{key}", value) for key, value in kwargs.items()}
        return resolved_args, resolved_kwargs

    async def _load_app_context(self, context: SessionContext) -> ServiceContext:
        common_data = context.common_data()
        if common_data is None:
            raise ValueError("Common data is None in session context")
        serialized_context = await aio_core.get_object(ObjectRef.decode(common_data))
        service_context = await asyncio.to_thread(cloudpickle.loads, serialized_context)
        if not isinstance(service_context, ServiceContext):
            raise ValueError(f"Expected ServiceContext in common_data, got {type(service_context)}")
        return service_context

    async def on_session_enter(self, context: SessionContext) -> bool:
        service_context = await self._load_app_context(context)
        await asyncio.to_thread(self._set_execution_from_context, service_context)
        self._ssn_ctx = context
        return True

    async def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
        method, raw_args, raw_kwargs, session_context = await asyncio.to_thread(self._prepare_task, context)
        args, kwargs = await self._resolve_object_refs(raw_args, raw_kwargs)
        with _bind_invocation_context(session_context) as invocation_context:
            try:
                if inspect.iscoroutinefunction(method):
                    result = await method(*args, **kwargs)
                else:
                    result = await asyncio.to_thread(method, *args, **kwargs)
                    if inspect.isawaitable(result):
                        result = await result
            finally:
                _stage_response_attributes(invocation_context.attributes)

        key_prefix = f"{session_context.application.name}/{session_context.session_id}"
        result_ref = await aio_core.put_object(key_prefix, result)
        return TaskOutput(result_ref.encode())

    async def on_session_leave(self) -> bool:
        """Clear the active binding while retaining executor-scoped class instances."""
        self._ssn_ctx = None
        self._app_context = None
        self._execution_object = None
        return True


def _setup_logging():
    """Setup logging based on FLAME_LOG environment variable."""
    flame_log = os.environ.get("FLAME_LOG", "info").upper()

    # Map log level strings to logging constants
    level_map = {
        "DEBUG": logging.DEBUG,
        "INFO": logging.INFO,
        "WARNING": logging.WARNING,
        "WARN": logging.WARNING,
        "ERROR": logging.ERROR,
        "CRITICAL": logging.CRITICAL,
    }

    level = level_map.get(flame_log, logging.INFO)

    # Configure root logger for flamepy modules
    logging.basicConfig(
        level=level,
        format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    )

    # Also set level for flamepy loggers specifically
    logging.getLogger("flamepy").setLevel(level)
    logging.getLogger("e2e").setLevel(level)

    logger.info(f"Logging configured with level: {flame_log} ({level})")


def main():
    """Main entrypoint for the flamepy.app.runpy module."""
    from flamepy.core.aio import cache as aio_cache
    from flamepy.core.aio.service import run

    _setup_logging()
    logger.info("Starting FlameRunpyService")
    service = FlameRunpyService()

    async def serve():
        try:
            await run(service)
        finally:
            await aio_cache.close()

    asyncio.run(serve())


if __name__ == "__main__":
    main()

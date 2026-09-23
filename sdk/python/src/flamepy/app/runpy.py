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
import os
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Any, Dict, List, Optional, Tuple

import cloudpickle

from flamepy.app._context import (
    _bind_invocation_context,
    _stage_response_attributes,
    _take_response_attributes,
)
from flamepy.app.types import ServiceContext, ServiceRequest
from flamepy.core import ObjectRef, get_object, put_object
from flamepy.core.service import FlameService as CoreFlameService
from flamepy.core.service import SessionContext, TaskContext
from flamepy.core.types import TaskOutput
from flamepy.proto.types_pb2 import ExecutorAttributes

logger = logging.getLogger(__name__)

MAX_PARALLEL_RESOLVE = 8


class FlameRunpyService(CoreFlameService):
    """
    Common Python service for Flame that executes customized Python applications.

    This service invokes App function and class services without requiring custom
    container images. It accepts positional arguments, keyword arguments, and
    object-cache references.
    """

    def __init__(self):
        """Initialize the FlameRunpyService."""
        self._ssn_ctx: SessionContext = None
        self._execution_object: Any = None
        self._class_execution_objects: Dict[str, Any] = {}
        self._app_context: ServiceContext = None

    def _take_attributes(self) -> ExecutorAttributes:
        """Take attributes belonging to the invocation assembling this response."""
        attributes = _take_response_attributes()
        if attributes is not None:
            return ExecutorAttributes(attr=list(attributes))
        return super()._take_attributes()

    def _load_app_context(self) -> ServiceContext:
        """Load the latest ServiceContext from the session common data object."""
        common_data_bytes = self._ssn_ctx.common_data()
        if common_data_bytes is None:
            raise ValueError("Common data is None in session context")

        object_ref = ObjectRef.decode(common_data_bytes)
        serialized_ctx = get_object(object_ref)
        service_context = cloudpickle.loads(serialized_ctx)

        if not isinstance(service_context, ServiceContext):
            raise ValueError(f"Expected ServiceContext in common_data, got {type(service_context)}")

        return service_context

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

    def _resolve_object_ref(self, value: Any) -> Any:
        """
        Resolve an ObjectRef to its actual value by fetching from cache.

        Args:
            value: The value to resolve. If it's an ObjectRef, fetch the data from cache.
                   If it's bytes that might be an encoded ObjectRef, try to decode it.
                   Otherwise, return the value as is.

        Returns:
            The resolved value (unpickled if it was an ObjectRef).

        Raises:
            ValueError: If ObjectRef data cannot be retrieved from cache.
        """
        if isinstance(value, ObjectRef):
            logger.debug(f"Resolving ObjectRef: {value}")
            resolved_value = get_object(value)
            logger.debug(f"Resolved ObjectRef to type: {type(resolved_value)}")
            return resolved_value

        # Handle bytes that might be an encoded ObjectRef
        if isinstance(value, bytes):
            try:
                # Try to decode as ObjectRef
                object_ref = ObjectRef.decode(value)
                logger.debug(f"Decoded bytes to ObjectRef: {object_ref}")
                resolved_value = get_object(object_ref)
                logger.debug(f"Resolved ObjectRef (from bytes) to type: {type(resolved_value)}")
                return resolved_value
            except Exception as e:
                # If decoding fails, it's not an ObjectRef, return bytes as-is
                logger.debug(f"Bytes is not an ObjectRef: {e}")
                return value

        return value

    def _collect_object_refs(self, args: Tuple, kwargs: Dict[str, Any]) -> List[Tuple[str, Any, ObjectRef]]:
        """Collect all ObjectRefs from args and kwargs for parallel resolution.

        Returns list of (location, original_value, object_ref) tuples where location
        is either 'arg:N' for positional args or 'kwarg:key' for keyword args.
        """
        refs = []

        for i, value in enumerate(args):
            if isinstance(value, ObjectRef):
                refs.append((f"arg:{i}", value, value))
            elif isinstance(value, bytes):
                try:
                    object_ref = ObjectRef.decode(value)
                    refs.append((f"arg:{i}", value, object_ref))
                except Exception:
                    pass

        for key, value in kwargs.items():
            if isinstance(value, ObjectRef):
                refs.append((f"kwarg:{key}", value, value))
            elif isinstance(value, bytes):
                try:
                    object_ref = ObjectRef.decode(value)
                    refs.append((f"kwarg:{key}", value, object_ref))
                except Exception:
                    pass

        return refs

    def _resolve_object_refs_parallel(self, args: Tuple, kwargs: Dict[str, Any]) -> Tuple[Tuple, Dict[str, Any]]:
        """Resolve all ObjectRefs in args and kwargs in parallel.

        This significantly improves performance when multiple ObjectRefs need
        to be fetched, as cache requests are made concurrently.
        """
        refs = self._collect_object_refs(args, kwargs)

        if not refs:
            return args, kwargs

        if len(refs) == 1:
            location, original, obj_ref = refs[0]
            resolved = get_object(obj_ref)

            if location.startswith("arg:"):
                idx = int(location.split(":")[1])
                args = tuple(resolved if i == idx else v for i, v in enumerate(args))
            else:
                key = location.split(":")[1]
                kwargs = {k: resolved if k == key else v for k, v in kwargs.items()}

            return args, kwargs

        resolved_values: Dict[str, Any] = {}
        max_workers = min(len(refs), MAX_PARALLEL_RESOLVE)

        logger.debug(f"Resolving {len(refs)} ObjectRefs in parallel with {max_workers} workers")

        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            future_to_location = {executor.submit(get_object, obj_ref): location for location, _, obj_ref in refs}

            for future in as_completed(future_to_location):
                location = future_to_location[future]
                try:
                    result = future.result()
                    resolved_values[location] = result
                except Exception as e:
                    raise ValueError(f"Failed to resolve ObjectRef at {location}: {e}")

        new_args = list(args)
        for i, value in enumerate(args):
            location = f"arg:{i}"
            if location in resolved_values:
                new_args[i] = resolved_values[location]

        new_kwargs = dict(kwargs)
        for key in kwargs:
            location = f"kwarg:{key}"
            if location in resolved_values:
                new_kwargs[key] = resolved_values[location]

        logger.debug(f"Resolved {len(refs)} ObjectRefs in parallel")
        return tuple(new_args), new_kwargs

    def on_session_enter(self, context: SessionContext) -> bool:
        """
        Handle session enter event.

        Loads the ServiceContext and binds its function or retained class object.
        Package installation is handled by the executor manager before this is called.

        Args:
            context: Session context containing application and session information

        Returns:
            True if successful, False otherwise
        """
        logger.info(f"Entering session: {context.session_id}")
        logger.debug(f"Application: {context.application.name}")

        # Store the session context for use in task invocation.
        self._ssn_ctx = context

        service_context = self._load_app_context()
        self._set_execution_from_context(service_context)

        logger.info(f"Session entered successfully, service bound (autoscale={service_context.autoscale})")
        return True

    def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
        """
        Handle task invoke event.

        This method:
        1. Uses the function or retained class object bound by on_session_enter
        2. Deserializes the ServiceRequest from task input
        3. Resolves any ObjectRef instances in args/kwargs
        4. Invokes the function or requested class method
        5. Returns the result as bytes

        Args:
            context: Task context containing task ID, session ID, and input

        Returns:
            bytes containing the result of the execution, or None if no output

        Raises:
            ValueError: If the input format is invalid or execution fails
        """
        logger.info(f"Invoking task: {context.task_id}")

        try:
            # Step 1: Use the service bound during session enter.
            execution_object = self._execution_object
            if execution_object is None:
                raise ValueError("Execution object is None. Session may not have been entered properly.")

            logger.debug(f"Execution object type: {type(execution_object)}")

            # Step 2: Get the ServiceRequest from task input
            # For RL module: receive bytes from core API, deserialize with cloudpickle
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

            # Step 3: Resolve ObjectRef instances in args and kwargs (in parallel)
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

            invoke_args, invoke_kwargs = self._resolve_object_refs_parallel(raw_args, raw_kwargs)
            logger.debug(f"Resolved args: {len(invoke_args)} arguments, kwargs: {len(invoke_kwargs)} keyword arguments")

            # Step 4: Execute the requested method
            session_context = self._ssn_ctx
            if session_context is None:
                raise ValueError("Session context is not available")
            if context.session_id != session_context.session_id:
                raise ValueError(f"Task session '{context.session_id}' does not match bound session '{session_context.session_id}'")
            with _bind_invocation_context(session_context) as invocation_context:
                try:
                    if request.method is None:
                        # Invoke the function service.
                        if not callable(execution_object):
                            raise ValueError(f"Function service is not callable: {type(execution_object)}")
                        logger.debug(f"Invoking function with args={invoke_args}, kwargs={invoke_kwargs}")
                        result = execution_object(*invoke_args, **invoke_kwargs)
                    else:
                        # Invoke a method on the retained class object.
                        if not hasattr(execution_object, request.method):
                            raise ValueError(f"Execution object has no method '{request.method}'")

                        method = getattr(execution_object, request.method)
                        if not callable(method):
                            raise ValueError(f"Attribute '{request.method}' is not callable")

                        logger.debug(f"Invoking method '{request.method}' with args={invoke_args}, kwargs={invoke_kwargs}")
                        result = method(*invoke_args, **invoke_kwargs)
                finally:
                    _stage_response_attributes(invocation_context.attributes)

            logger.info(f"Task {context.task_id} completed successfully")
            logger.debug(f"Result type: {type(result)}")

            # Step 5: Put the result into cache and return ObjectRef encoded as bytes
            # This enables efficient data transfer for large objects
            logger.debug("Putting result into cache")
            key_prefix = f"{self._ssn_ctx.application.name}/{self._ssn_ctx.session_id}"
            result_object_ref = put_object(key_prefix, result)
            logger.info(f"Result cached with ObjectRef: {result_object_ref}")

            # For RL module: encode ObjectRef to bytes for core API
            result_bytes = result_object_ref.encode()
            return TaskOutput(result_bytes)

        except Exception as e:
            logger.error(f"Error in task {context.task_id}: {e}", exc_info=True)
            raise

    def on_session_leave(self) -> bool:
        """
        Handle session leave event.

        This method performs cleanup at session end. In the current implementation,
        there are no packages to uninstall. Future versions will handle cleanup of
        temporarily installed packages.

        Returns:
            True if successful, False otherwise
        """
        logger.info(f"Leaving session: {self._ssn_ctx.session_id if self._ssn_ctx else 'unknown'}")

        # Clean up session context
        self._ssn_ctx = None
        self._app_context = None
        self._execution_object = None

        # Future implementation will:
        # 1. Uninstall any temporary packages that were installed
        # 2. Clean up any temporary files

        logger.info("Session left successfully")
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
    from ..core.service import run

    _setup_logging()
    logger.info("Starting FlameRunpyService")
    service = FlameRunpyService()
    run(service)


if __name__ == "__main__":
    main()

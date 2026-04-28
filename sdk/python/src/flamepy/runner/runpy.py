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

import inspect
import logging
import os
from typing import Any, Optional

import cloudpickle

from flamepy.core import ObjectRef, get_object, put_object, update_object
from flamepy.core.service import FlameService, SessionContext, TaskContext
from flamepy.core.types import TaskOutput
from flamepy.runner.types import RunnerContext, RunnerRequest

logger = logging.getLogger(__name__)


class FlameRunpyService(FlameService):
    """
    Common Python service for Flame that executes customized Python applications.

    This service allows users to execute arbitrary Python functions and objects
    remotely without building custom container images. It supports method invocation
    with various input types including positional args, keyword args, and large objects.
    """

    def __init__(self):
        """Initialize the FlameRunpyService."""
        self._ssn_ctx: SessionContext = None
        self._execution_object: Any = None
        self._runner_context: RunnerContext = None

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
            if resolved_value is None:
                raise ValueError(f"Failed to retrieve ObjectRef from cache: {value}")

            logger.debug(f"Resolved ObjectRef to type: {type(resolved_value)}")
            return resolved_value

        # Handle bytes that might be an encoded ObjectRef
        if isinstance(value, bytes):
            try:
                # Try to decode as ObjectRef
                object_ref = ObjectRef.decode(value)
                logger.debug(f"Decoded bytes to ObjectRef: {object_ref}")
                resolved_value = get_object(object_ref)
                if resolved_value is None:
                    raise ValueError(f"Failed to retrieve ObjectRef from cache: {object_ref}")
                logger.debug(f"Resolved ObjectRef (from bytes) to type: {type(resolved_value)}")
                return resolved_value
            except Exception as e:
                # If decoding fails, it's not an ObjectRef, return bytes as-is
                logger.debug(f"Bytes is not an ObjectRef: {e}")
                return value

        return value

    def on_session_enter(self, context: SessionContext) -> bool:
        """
        Handle session enter event.

        Loads the RunnerContext and execution object, instantiating classes if needed.
        Package installation is handled by the executor manager before this is called.

        Args:
            context: Session context containing application and session information

        Returns:
            True if successful, False otherwise
        """
        logger.info(f"Entering session: {context.session_id}")
        logger.debug(f"Application: {context.application.name}")

        # Store the session context for use in task invocation
        self._ssn_ctx = context

        # Load RunnerContext from common_data
        common_data_bytes = context.common_data()
        if common_data_bytes is None:
            raise ValueError("Common data is None in session context")

        # Decode bytes to ObjectRef
        object_ref = ObjectRef.decode(common_data_bytes)
        # Get from cache
        serialized_ctx = get_object(object_ref)
        # Deserialize using cloudpickle
        runner_context = cloudpickle.loads(serialized_ctx)

        if not isinstance(runner_context, RunnerContext):
            raise ValueError(f"Expected RunnerContext in common_data, got {type(runner_context)}")

        # Step 2: Store configuration
        self._runner_context = runner_context

        # Step 3: Load execution object
        execution_object = runner_context.execution_object
        if execution_object is None:
            raise ValueError("Execution object is None in RunnerContext")

        # Step 4: If it's a class, instantiate it
        if inspect.isclass(execution_object):
            logger.info(f"Instantiating class {execution_object.__name__}")
            execution_object = execution_object()  # Use default constructor

        # Step 5: Store execution object for reuse
        self._execution_object = execution_object

        logger.info(f"Session entered successfully, execution object loaded (stateful={runner_context.stateful}, autoscale={runner_context.autoscale})")
        return True

    def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
        """
        Handle task invoke event.

        This method:
        1. Uses the cached execution object from on_session_enter
        2. Deserializes the RunnerRequest from task input
        3. Resolves any ObjectRef instances in args/kwargs
        4. Executes the requested method on the execution object
        5. Persists state if stateful=True
        6. Returns the result as bytes

        Args:
            context: Task context containing task ID, session ID, and input

        Returns:
            bytes containing the result of the execution, or None if no output

        Raises:
            ValueError: If the input format is invalid or execution fails
        """
        logger.info(f"Invoking task: {context.task_id}")

        try:
            # Step 1: Use cached execution object (not from common_data)
            execution_object = self._execution_object
            if execution_object is None:
                raise ValueError("Execution object is None. Session may not have been entered properly.")

            logger.debug(f"Execution object type: {type(execution_object)}")

            # Step 2: Get the RunnerRequest from task input
            # For RL module: receive bytes from core API, deserialize with cloudpickle
            if context.input is None:
                raise ValueError("Task input is None")

            request = cloudpickle.loads(context.input)
            if not isinstance(request, RunnerRequest):
                raise ValueError(f"Expected RunnerRequest in task input, got {type(request)}")

            # Ensure __post_init__ validation runs after deserialization
            # This validates that args/kwargs are the correct types
            RunnerRequest.__post_init__(request)

            # Validate request structure
            if request.method is not None and not isinstance(request.method, str):
                raise ValueError(f"request.method must be a string or None, got {type(request.method)}")

            logger.debug(f"RunnerRequest: method={request.method}, has_args={request.args is not None}, has_kwargs={request.kwargs is not None}")

            # Step 3: Resolve ObjectRef instances in args and kwargs
            invoke_args = ()
            invoke_kwargs = {}

            if request.args is not None:
                # Ensure args is iterable (tuple or list)
                if not isinstance(request.args, (tuple, list)):
                    raise ValueError(f"request.args must be a tuple or list, got {type(request.args)}: {request.args}")
                # Resolve any ObjectRef instances in args
                invoke_args = tuple(self._resolve_object_ref(arg) for arg in request.args)
                logger.debug(f"Resolved args: {len(invoke_args)} arguments")

            if request.kwargs is not None:
                # Ensure kwargs is a dictionary
                if not isinstance(request.kwargs, dict):
                    raise ValueError(f"request.kwargs must be a dict, got {type(request.kwargs)}: {request.kwargs}")
                # Resolve any ObjectRef instances in kwargs
                invoke_kwargs = {key: self._resolve_object_ref(value) for key, value in request.kwargs.items()}
                logger.debug(f"Resolved kwargs: {len(invoke_kwargs)} keyword arguments")

            # Step 4: Execute the requested method
            if request.method is None:
                # The execution object itself is callable
                if not callable(execution_object):
                    raise ValueError(f"Execution object is not callable: {type(execution_object)}")
                logger.debug(f"Invoking callable with args={invoke_args}, kwargs={invoke_kwargs}")
                result = execution_object(*invoke_args, **invoke_kwargs)
            else:
                # Invoke a specific method on the execution object
                if not hasattr(execution_object, request.method):
                    raise ValueError(f"Execution object has no method '{request.method}'")

                method = getattr(execution_object, request.method)
                if not callable(method):
                    raise ValueError(f"Attribute '{request.method}' is not callable")

                logger.debug(f"Invoking method '{request.method}' with args={invoke_args}, kwargs={invoke_kwargs}")
                result = method(*invoke_args, **invoke_kwargs)

            logger.info(f"Task {context.task_id} completed successfully")
            logger.debug(f"Result type: {type(result)}")

            # Step 5: Update execution object state if stateful
            if self._runner_context.stateful:
                logger.debug("Persisting execution object state")
                updated_context = RunnerContext(
                    execution_object=execution_object,  # Updated object
                    stateful=self._runner_context.stateful,
                    autoscale=self._runner_context.autoscale,
                )
                # For RL module: serialize RunnerContext with cloudpickle, update in cache to get ObjectRef,
                # then encode ObjectRef to bytes for core API
                serialized_ctx = cloudpickle.dumps(updated_context, protocol=cloudpickle.DEFAULT_PROTOCOL)

                # Get original ObjectRef and update it
                common_data_bytes = self._ssn_ctx.common_data()
                object_ref = ObjectRef.decode(common_data_bytes)
                update_object(object_ref, serialized_ctx)
                logger.debug("Execution object state persisted successfully in cache")
            else:
                logger.debug("Skipping state persistence for non-stateful service")

            # Step 6: Put the result into cache and return ObjectRef encoded as bytes
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
    """Main entrypoint for the flamepy.runner.runpy module."""
    from ..core.service import run

    _setup_logging()
    logger.info("Starting FlameRunpyService")
    service = FlameRunpyService()
    run(service)


if __name__ == "__main__":
    main()

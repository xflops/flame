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
import sys
import threading
from abc import abstractmethod
from concurrent import futures
from dataclasses import dataclass, field
from typing import Callable, List, Optional, Set

# Handle typing.override compatibility for Python < 3.12
if sys.version_info >= (3, 12):
    from typing import override
else:
    try:
        from typing_extensions import override
    except ImportError:
        # If typing_extensions is not available, use a no-op decorator
        def override(func):  # type: ignore
            return func


import grpc

from flamepy.core.types import FlameError, FlameErrorCode, TaskOutput
from flamepy.proto.shim_pb2_grpc import InstanceServicer, add_InstanceServicer_to_server
from flamepy.proto.types_pb2 import (
    Result,
)
from flamepy.proto.types_pb2 import TaskResult as TaskResultProto
from flamepy.proto.types_pb2 import ExecutorAttributes
from flamepy.proto.shim_pb2 import OnSessionEnterResponse, OnTaskInvokeResponse

logger = logging.getLogger(__name__)

FLAME_INSTANCE_ENDPOINT = "FLAME_INSTANCE_ENDPOINT"


class TraceFn:
    def __init__(self, name: str):
        self.name = name
        logger.debug(f"{name} Enter")

    def __del__(self):
        logger.debug(f"{self.name} Exit")


@dataclass
class ApplicationContext:
    """Context for an application."""

    name: str
    image: Optional[str] = None
    command: Optional[str] = None
    working_directory: Optional[str] = None
    url: Optional[str] = None


@dataclass
class SessionContext:
    """Context for a session."""

    _common_data: Optional[bytes]

    session_id: str
    application: ApplicationContext
    _publish_executor_attributes: Optional[Callable[[Set[bytes]], None]] = field(
        default=None, init=False, repr=False
    )

    def publish(self, attr: Set[bytes]) -> None:
        """Publish a full replacement of this executor's opaque locality keys."""
        if any(not isinstance(key, bytes) for key in attr):
            raise TypeError("executor attributes must contain bytes values")
        if self._publish_executor_attributes is None:
            raise RuntimeError("SessionContext is not attached to a service")
        self._publish_executor_attributes(set(attr))

    def __getstate__(self):
        """Exclude the service callback when Runner persists an execution object."""
        state = self.__dict__.copy()
        state.pop("_publish_executor_attributes", None)
        return state

    def __setstate__(self, state):
        self.__dict__.update(state)
        self._publish_executor_attributes = None

    def common_data(self) -> Optional[bytes]:
        """Get the common data as bytes."""
        return self._common_data


@dataclass
class TaskContext:
    """Context for a task."""

    task_id: str
    session_id: str
    input: Optional[bytes]  # Task input as bytes in core API


class FlameService:
    """Base class for implementing Flame services."""

    @abstractmethod
    def on_session_enter(self, context: SessionContext):
        """
        Called when entering a session.

        Args:
            context: Session context information

        Returns:
            True if successful, False otherwise
        """
        pass

    @abstractmethod
    def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
        """
        Called when a task is invoked.

        Args:
            context: Task context information

        Returns:
            Task output as bytes, or None if no output
        """
        pass

    @abstractmethod
    def on_session_leave(self):
        """
        Called when leaving a session.

        Returns:
            True if successful, False otherwise
        """
        pass


class FlameInstanceServicer(InstanceServicer):
    """gRPC servicer implementation for GrpcShim service."""

    def __init__(self, service: FlameService):
        self._service = service
        self._session_context: Optional[SessionContext] = None
        self._executor_attributes: Optional[ExecutorAttributes] = None
        self._executor_attributes_lock = threading.Lock()

    def _publish_executor_attributes(self, attr: Set[bytes]) -> None:
        with self._executor_attributes_lock:
            self._executor_attributes = ExecutorAttributes(attr=list(attr))

    def _take_executor_attributes(self) -> Optional[ExecutorAttributes]:
        with self._executor_attributes_lock:
            attributes = self._executor_attributes
            self._executor_attributes = None
            return attributes

    def _clear_executor_attributes(self) -> None:
        with self._executor_attributes_lock:
            self._executor_attributes = None

    @override
    def OnSessionEnter(self, request, context):  # noqa: N802
        """Handle OnSessionEnter RPC call."""
        _trace_fn = TraceFn("OnSessionEnter")

        try:
            logger.debug(f"OnSessionEnter request: {request}")
            self._clear_executor_attributes()

            # Convert protobuf request to SessionContext
            app_context = ApplicationContext(
                name=request.application.name,
                image=(request.application.image if request.application.HasField("image") else None),
                command=(request.application.command if request.application.HasField("command") else None),
                working_directory=(request.application.working_directory if request.application.HasField("working_directory") else None),
                url=(request.application.url if request.application.HasField("url") else None),
            )

            logger.debug(f"app_context: {app_context}")

            # Common data is bytes in core API
            common_data_bytes = request.common_data if request.HasField("common_data") else None

            session_context = SessionContext(
                _common_data=common_data_bytes,
                session_id=request.session_id,
                application=app_context,
            )
            session_context._publish_executor_attributes = self._publish_executor_attributes
            self._session_context = session_context

            logger.debug(f"session_context: {session_context}")

            # Call the service implementation
            self._service.on_session_enter(session_context)
            logger.debug("on_session_enter completed successfully")

            # Return result
            return OnSessionEnterResponse(
                result=Result(return_code=0),
                attributes=self._take_executor_attributes(),
            )

        except Exception as e:
            logger.error(f"Error in OnSessionEnter: {e}")
            return OnSessionEnterResponse(result=Result(return_code=-1, message=f"{str(e)}"))

    @override
    def OnTaskInvoke(self, request, context):  # noqa: N802
        """Handle OnTaskInvoke RPC call."""
        _trace_fn = TraceFn("OnTaskInvoke")

        try:
            # Convert protobuf request to TaskContext
            # Task input is bytes in core API
            input_bytes = request.input if request.HasField("input") else None

            task_context = TaskContext(
                task_id=request.task_id,
                session_id=request.session_id,
                input=input_bytes,
            )

            logger.debug(f"task_context: {task_context}")

            # Call the service implementation
            output_data = self._service.on_task_invoke(task_context)
            logger.debug("on_task_invoke completed successfully")

            # Return task output. Leave optional output unset for services that intentionally return None.
            if output_data is None:
                task_result = TaskResultProto(return_code=0, message=None)
            else:
                task_result = TaskResultProto(return_code=0, output=output_data, message=None)
            return OnTaskInvokeResponse(
                task_result=task_result,
                attributes=(
                    self._take_executor_attributes()
                    if self._session_context is not None
                    else None
                ),
            )

        except Exception as e:
            logger.error(f"Error in OnTaskInvoke: {e}")
            return OnTaskInvokeResponse(
                task_result=TaskResultProto(return_code=-1, output=None, message=f"{str(e)}")
            )

    @override
    def OnSessionLeave(self, request, context):  # noqa: N802
        """Handle OnSessionLeave RPC call."""
        _trace_fn = TraceFn("OnSessionLeave")

        try:
            # Call the service implementation
            self._service.on_session_leave()
            logger.debug("on_session_leave completed successfully")

            # Return result
            return Result(
                return_code=0,
            )

        except Exception as e:
            logger.error(f"Error in OnSessionLeave: {e}")
            return Result(return_code=-1, message=f"{str(e)}")


class FlameInstanceServer:
    """Server for gRPC shim services."""

    def __init__(self, service: FlameService):
        self._service = service
        self._server = None

    def start(self):
        """Start the gRPC server."""
        try:
            # Create gRPC server
            self._server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))

            # Add servicer to server
            shim_servicer = FlameInstanceServicer(self._service)
            add_InstanceServicer_to_server(shim_servicer, self._server)

            # Listen on Unix socket
            endpoint = os.getenv(FLAME_INSTANCE_ENDPOINT)
            if endpoint is not None:
                self._server.add_insecure_port(f"unix://{endpoint}")
                logger.debug(f"Flame Python instance service started on Unix socket: {endpoint}")
            else:
                raise FlameError(FlameErrorCode.INVALID_CONFIG, "FLAME_INSTANCE_ENDPOINT not found")

            # Start server
            self._server.start()
            # Keep server running
            self._server.wait_for_termination()

        except Exception as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"Failed to start gRPC instance server: {str(e)}",
            )

    def stop(self):
        """Stop the gRPC server."""
        if self._server:
            self._server.stop(grace=5)
            logger.info("gRPC instance server stopped")


def run(service: FlameService):
    """
    Run a gRPC shim server.

    Args:
        service: The shim service implementation
    """

    server = FlameInstanceServer(service)
    server.start()

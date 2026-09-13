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
from dataclasses import dataclass
from typing import FrozenSet, Iterable, Optional

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
from flamepy.proto.shim_pb2 import OnSessionEnterResponse, OnTaskInvokeResponse
from flamepy.proto.shim_pb2_grpc import InstanceServicer, add_InstanceServicer_to_server
from flamepy.proto.types_pb2 import (
    ExecutorAttributes,
    Result,
)
from flamepy.proto.types_pb2 import TaskResult as TaskResultProto

logger = logging.getLogger(__name__)

FLAME_INSTANCE_ENDPOINT = "FLAME_INSTANCE_ENDPOINT"

_MAX_ATTRIBUTE_COUNT = 1_024
_MAX_ATTRIBUTE_BYTES = 256
_MAX_ATTRIBUTES_BYTES = 64 * 1_024


class _Publisher:
    """Attributes accumulated for the next shim response."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._attributes: set[bytes] = set()
        self._attribute_bytes = 0

    def publish(self, attributes: FrozenSet[bytes]) -> None:
        with self._lock:
            added = attributes.difference(self._attributes)
            added_bytes = sum(map(len, added))
            if len(self._attributes) + len(added) > _MAX_ATTRIBUTE_COUNT or self._attribute_bytes + added_bytes > _MAX_ATTRIBUTES_BYTES:
                raise ValueError("executor attribute publication exceeds configured limits")
            self._attributes.update(added)
            self._attribute_bytes += added_bytes

    def take(self) -> ExecutorAttributes:
        with self._lock:
            attributes = self._attributes
            self._attributes = set()
            self._attribute_bytes = 0
            return ExecutorAttributes(attr=list(attributes))


def _validate_attributes(attributes: Iterable[bytes]) -> FrozenSet[bytes]:
    try:
        snapshot = frozenset(attributes)
    except TypeError as e:
        raise TypeError("executor attributes must be an iterable of bytes") from e

    if any(not isinstance(key, bytes) for key in snapshot):
        raise TypeError("executor attributes must contain bytes values")
    if any(not key or len(key) > _MAX_ATTRIBUTE_BYTES for key in snapshot):
        raise ValueError("executor attributes must be nonempty and at most 256 bytes")
    if len(snapshot) > _MAX_ATTRIBUTE_COUNT or sum(map(len, snapshot)) > _MAX_ATTRIBUTES_BYTES:
        raise ValueError("executor attribute publication exceeds configured limits")
    return snapshot


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

    def _publisher(self) -> _Publisher:
        publisher = getattr(self, "_flame_publisher", None)
        if publisher is None:
            publisher = _Publisher()
            self._flame_publisher = publisher
        return publisher

    def publish(self, attributes: Iterable[bytes]) -> None:
        """Add locality keys to this instance's next response."""
        self._publisher().publish(_validate_attributes(attributes))

    def _take_attributes(self) -> ExecutorAttributes:
        return self._publisher().take()

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
        self._service._publisher()

    @override
    def OnSessionEnter(self, request, context):  # noqa: N802
        """Handle OnSessionEnter RPC call."""
        _trace_fn = TraceFn("OnSessionEnter")

        try:
            logger.debug(f"OnSessionEnter request: {request}")

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

            logger.debug(f"session_context: {session_context}")

            # Call the service implementation
            self._service.on_session_enter(session_context)
            logger.debug("on_session_enter completed successfully")

            # Return result
            return OnSessionEnterResponse(
                result=Result(return_code=0),
                attributes=self._service._take_attributes(),
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
                attributes=self._service._take_attributes(),
            )

        except Exception as e:
            logger.error(f"Error in OnTaskInvoke: {e}")
            return OnTaskInvokeResponse(
                task_result=TaskResultProto(return_code=-1, output=None, message=f"{str(e)}"),
                attributes=self._service._take_attributes(),
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
            add_InstanceServicer_to_server(FlameInstanceServicer(self._service), self._server)

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

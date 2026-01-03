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
import os
import time
import grpc
from abc import ABC, abstractmethod
from typing import Optional, Dict, Any, Union
from dataclasses import dataclass
import logging
from concurrent import futures

from .types import Shim, FlameError, FlameErrorCode, DataExpr
from .cache import get_object
from .shim_pb2_grpc import InstanceServicer, add_InstanceServicer_to_server
from .shim_pb2 import WatchEventResponse as WatchEventResponseProto
from .types_pb2 import (
    Result,
    EmptyRequest,
    TaskResult as TaskResultProto,
    Event as EventProto,
    EventOwner as EventOwnerProto,
)

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
    shim: Shim
    image: Optional[str] = None
    command: Optional[str] = None


@dataclass
class SessionContext:
    """Context for a session."""

    _queue: asyncio.Queue
    session_id: str
    application: ApplicationContext
    common_data: Optional[bytes] = None

    async def record_event(self, code: int, message: Optional[str] = None):
        """Record an event."""
        event = WatchEventResponseProto(
            owner=EventOwnerProto(session_id=self.session_id, task_id=None),
            event=EventProto(code=code, message=message, creation_time=int(time.time() * 1000)),
        )
        await self._queue.put(event)


@dataclass
class TaskContext:
    """Context for a task."""

    _queue: asyncio.Queue
    task_id: str
    session_id: str
    input: Optional[bytes] = None

    async def record_event(self, code: int, message: Optional[str] = None):
        """Record an event."""
        event = WatchEventResponseProto(
            owner=EventOwnerProto(session_id=self.session_id, task_id=self.task_id),
            event=EventProto(code=code, message=message, creation_time=int(time.time() * 1000)),
        )
        await self._queue.put(event)


@dataclass
class TaskOutput:
    """Output from a task."""

    data: Optional[bytes] = None


class FlameService:
    """Base class for implementing Flame services."""

    @abstractmethod
    async def on_session_enter(self, context: SessionContext):
        """
        Called when entering a session.

        Args:
            context: Session context information

        Returns:
            True if successful, False otherwise
        """
        pass

    @abstractmethod
    async def on_task_invoke(self, context: TaskContext) -> TaskOutput:
        """
        Called when a task is invoked.

        Args:
            context: Task context information

        Returns:
            Task output
        """
        pass

    @abstractmethod
    async def on_session_leave(self):
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
        self._queue = asyncio.Queue()
        self._common_data_expr = None

    async def OnSessionEnter(self, request, context):
        """Handle OnSessionEnter RPC call."""
        _trace_fn = TraceFn("OnSessionEnter")

        try:

            logger.debug(f"OnSessionEnter request: {request}")

            # Convert protobuf request to SessionContext
            app_context = ApplicationContext(
                name=request.application.name,
                shim=Shim(request.application.shim),
                image=(request.application.image if request.application.HasField("image") else None),
                command=(request.application.command if request.application.HasField("command") else None),
            )

            logger.debug(f"app_context: {app_context}")

            self._common_data_expr = DataExpr.from_json(request.common_data) if request.HasField("common_data") else None

            if self._common_data_expr is not None:
                self._common_data_expr = await get_object(self._common_data_expr)

            session_context = SessionContext(
                _queue=self._queue,
                session_id=request.session_id,
                application=app_context,
                common_data=self._common_data_expr.data if self._common_data_expr is not None else None,
            )

            logger.debug(f"session_context: {session_context}")

            # Call the service implementation
            await self._service.on_session_enter(session_context)
            logger.debug("on_session_enter completed successfully")

            # Return result
            return Result(
                return_code=0,
            )

        except Exception as e:
            logger.error(f"Error in OnSessionEnter: {e}")
            return Result(return_code=-1, message=f"{str(e)}")

    async def OnTaskInvoke(self, request, context):
        """Handle OnTaskInvoke RPC call."""
        _trace_fn = TraceFn("OnTaskInvoke")

        try:
            # Convert protobuf request to TaskContext
            task_context = TaskContext(
                _queue=self._queue,
                task_id=request.task_id,
                session_id=request.session_id,
                input=request.input if request.HasField("input") else None,
            )

            logger.debug(f"task_context: {task_context}")

            # Call the service implementation
            output = await self._service.on_task_invoke(task_context)
            logger.debug("on_task_invoke completed successfully")

            # Return task output
            return TaskResultProto(return_code=0, output=output.data, message=None)

        except Exception as e:
            logger.error(f"Error in OnTaskInvoke: {e}")
            return TaskResultProto(return_code=-1, output=None, message=f"{str(e)}")

    async def OnSessionLeave(self, request, context):
        """Handle OnSessionLeave RPC call."""
        _trace_fn = TraceFn("OnSessionLeave")

        try:
            # Call the service implementation
            await self._service.on_session_leave()
            logger.debug("on_session_leave completed successfully")

            # Put None to the queue to signal the queue is shutting down
            self._queue.put_nowait(None)

            # Wait for the queue to be empty
            await self._queue.join()

            logger.debug("All events processed, exiting OnSessionLeave")

            # Return result
            return Result(
                return_code=0,
            )

        except Exception as e:
            logger.error(f"Error in OnSessionLeave: {e}")
            return Result(return_code=-1, message=f"{str(e)}")

    async def WatchEvent(self, request, context):
        """Watch events from the service."""
        try:
            while True:
                event = await self._queue.get()
                logger.debug(f"Event: {event}")

                # Mark the event as processed
                self._queue.task_done()

                # If the event is None, it means the queue is shutting down
                if event is None:
                    return

                yield event
        except Exception as e:
            logger.error(f"Error in WatchEvents: {e}")
            raise


class FlameInstanceServer:
    """Server for gRPC shim services."""

    def __init__(self, service: FlameService):
        self._service = service
        self._server = None

    async def start(self):
        """Start the gRPC server."""
        try:
            # Create gRPC server
            self._server = grpc.aio.server()

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
            await self._server.start()
            # Keep server running
            await self._server.wait_for_termination()

        except Exception as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"Failed to start gRPC instance server: {str(e)}",
            )

    async def stop(self):
        """Stop the gRPC server."""
        if self._server:
            await self._server.stop(grace=5)
            logger.info("gRPC instance server stopped")


def run(service: FlameService):
    """
    Run a gRPC shim server.

    Args:
        service: The shim service implementation
    """

    server = FlameInstanceServer(service)
    asyncio.run(server.start())

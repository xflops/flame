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
import logging
import sys
import threading
from abc import abstractmethod
from contextvars import ContextVar
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


from flamepy.core.types import FlameError, FlameErrorCode, TaskOutput
from flamepy.proto.types_pb2 import (
    ExecutorAttributes,
)

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


_active_response_publisher: ContextVar[Optional[_Publisher]] = ContextVar("flamepy_active_response_publisher", default=None)


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
        active = _active_response_publisher.get()
        if active is not None:
            return active
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


class FlameInstanceServicer:
    """Compatibility adapter for direct synchronous shim calls.

    The network server uses ``flamepy.core.aio.service.FlameInstanceServicer``
    directly. This adapter retains the old direct-call surface for callers that
    invoke the servicer without starting a server.
    """

    def __init__(self, service: FlameService):
        from flamepy.core._bridge import LoopThread
        from flamepy.core.aio.service import FlameInstanceServicer as AioFlameInstanceServicer

        self._loop_thread = LoopThread("flame-shim-adapter")

        async def create():
            return AioFlameInstanceServicer(service)

        self._aio = self._loop_thread.call(create())

    def OnSessionEnter(self, request, context):  # noqa: N802
        return self._loop_thread.call(self._aio.OnSessionEnter(request, context))

    def OnTaskInvoke(self, request, context):  # noqa: N802
        return self._loop_thread.call(self._aio.OnTaskInvoke(request, context))

    def OnSessionLeave(self, request, context):  # noqa: N802
        return self._loop_thread.call(self._aio.OnSessionLeave(request, context))

    def close(self):
        if getattr(self, "_closed", False):
            return
        self._closed = True
        try:
            self._loop_thread.call(self._aio.close())
        finally:
            self._loop_thread.close()

    def __del__(self):
        try:
            self.close()
        except Exception:
            pass


class FlameInstanceServer:
    """Blocking facade over the asyncio shim server."""

    def __init__(self, service: FlameService):
        self._service = service
        self._server = None
        self._loop = None
        self._loop_thread = None
        self._stop_requested = threading.Event()

    def start(self):
        """Start the shim server and block until it terminates."""
        from flamepy.core.aio.service import FlameInstanceServer as AioFlameInstanceServer

        async def serve():
            self._loop = asyncio.get_running_loop()
            self._loop_thread = threading.current_thread()
            self._server = AioFlameInstanceServer(self._service)
            await self._server.start()
            try:
                if self._stop_requested.is_set():
                    await self._server.stop()
                await self._server.wait_for_termination()
            finally:
                await self._server.stop()

        try:
            asyncio.run(serve())
        except Exception as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"Failed to start gRPC instance server: {str(e)}",
            )

    def stop(self):
        """Stop the asyncio shim server from another thread."""
        self._stop_requested.set()
        if self._server and self._loop and self._loop.is_running():
            if threading.current_thread() is self._loop_thread:
                raise RuntimeError("Cannot stop the blocking shim server from its event-loop thread")
            asyncio.run_coroutine_threadsafe(self._server.stop(), self._loop).result()
            logger.info("gRPC instance server stopped")


def run(service: FlameService):
    """
    Run a gRPC shim server.

    Args:
        service: The shim service implementation
    """

    server = FlameInstanceServer(service)
    server.start()

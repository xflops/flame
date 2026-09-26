"""
Copyright 2026 The Flame Authors.
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
import contextvars
import logging
import os
from concurrent.futures import ThreadPoolExecutor
from typing import Optional

import grpc

from flamepy.core.service import (
    FLAME_INSTANCE_ENDPOINT,
    ApplicationContext,
    SessionContext,
    TaskContext,
    _active_response_publisher,
    _Publisher,
)
from flamepy.core.service import (
    FlameService as SyncFlameService,
)
from flamepy.core.types import FlameError, FlameErrorCode, TaskOutput
from flamepy.proto.shim_pb2 import OnSessionEnterResponse, OnTaskInvokeResponse
from flamepy.proto.shim_pb2_grpc import InstanceServicer, add_InstanceServicer_to_server
from flamepy.proto.types_pb2 import Result
from flamepy.proto.types_pb2 import TaskResult as TaskResultProto

logger = logging.getLogger(__name__)
_SHUTDOWN_TIMEOUT = 5.0


class _HookError(Exception):
    def __init__(self, cause, attributes):
        super().__init__(str(cause))
        self.attributes = attributes


class FlameService(SyncFlameService):
    """Implement shim hooks as coroutines on the server's event loop."""

    async def on_session_enter(self, context: SessionContext):
        raise NotImplementedError

    async def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
        raise NotImplementedError

    async def on_session_leave(self):
        raise NotImplementedError


class FlameInstanceServicer(InstanceServicer):
    """One RPC implementation for async and adapted synchronous services."""

    def __init__(self, service: SyncFlameService, *, max_workers: int = 16, max_inflight: int = 64):
        self._service = service
        self._service._publisher()
        self._executor = ThreadPoolExecutor(max_workers=max_workers, thread_name_prefix="flame-shim")
        self._max_inflight = max_inflight
        self._binding_lock = asyncio.Lock()
        self._active_tasks = 0
        self._hook_calls: set[asyncio.Task] = set()

    async def close(self):
        if self._hook_calls:
            _, pending = await asyncio.wait(self._hook_calls, timeout=_SHUTDOWN_TIMEOUT)
            if pending:
                logger.warning("Shim shutdown timed out waiting for %d hooks", len(pending))
                for work in pending:
                    work.cancel()
        # Running synchronous hooks cannot be interrupted. Let them finish in
        # their worker threads while making server shutdown itself bounded.
        self._executor.shutdown(wait=False, cancel_futures=True)

    async def _hook(self, name, *args):
        method = getattr(self._service, name)
        if isinstance(self._service, FlameService):
            token = _active_response_publisher.set(_Publisher())
            try:
                try:
                    result = await method(*args)
                except Exception as exc:
                    raise _HookError(exc, self._service._take_attributes()) from exc
                return result, self._service._take_attributes()
            finally:
                _active_response_publisher.reset(token)

        # The synchronous hook and its attribute handoff must run in the same
        # copied context. Taking attributes on this loop loses ContextVar state.
        def call():
            token = _active_response_publisher.set(_Publisher())
            try:
                try:
                    result = method(*args)
                except Exception as exc:
                    raise _HookError(exc, self._service._take_attributes()) from exc
                return result, self._service._take_attributes()
            finally:
                _active_response_publisher.reset(token)

        context = contextvars.copy_context()
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(self._executor, context.run, call)

    async def _binding_hook(self, name, *args):
        # Once a hook starts, RPC cancellation cannot interrupt it safely: a
        # worker thread (or user coroutine) may still mutate the binding.
        work = asyncio.create_task(self._hook(name, *args))
        self._hook_calls.add(work)
        work.add_done_callback(self._hook_calls.discard)
        try:
            return await asyncio.shield(work)
        except asyncio.CancelledError:
            while not work.done():
                try:
                    await asyncio.shield(work)
                except asyncio.CancelledError:
                    pass
            if not work.cancelled():
                work.exception()  # Consume a hook failure after the caller has gone.
            raise

    async def OnSessionEnter(self, request, context):  # noqa: N802
        try:
            app = request.application
            app_context = ApplicationContext(
                name=app.name,
                image=app.image if app.HasField("image") else None,
                command=app.command if app.HasField("command") else None,
                working_directory=app.working_directory if app.HasField("working_directory") else None,
                url=app.url if app.HasField("url") else None,
            )
            session_context = SessionContext(
                _common_data=request.common_data if request.HasField("common_data") else None,
                session_id=request.session_id,
                application=app_context,
            )
            async with self._binding_lock:
                _, attributes = await self._binding_hook("on_session_enter", session_context)
            return OnSessionEnterResponse(result=Result(return_code=0), attributes=attributes)
        except Exception as exc:
            logger.exception("OnSessionEnter failed")
            return OnSessionEnterResponse(result=Result(return_code=-1, message=str(exc)))

    async def OnTaskInvoke(self, request, context):  # noqa: N802
        task_context = TaskContext(
            task_id=request.task_id,
            session_id=request.session_id,
            input=request.input if request.HasField("input") else None,
        )
        if self._active_tasks >= self._max_inflight:
            return OnTaskInvokeResponse(task_result=TaskResultProto(return_code=-1, message="too many concurrent task invocations"))
        self._active_tasks += 1
        coroutine = self._invoke_task(task_context)
        try:
            work = asyncio.create_task(coroutine)
        except Exception:
            coroutine.close()
            self._active_tasks -= 1
            raise
        self._hook_calls.add(work)
        work.add_done_callback(self._hook_calls.discard)
        return await asyncio.shield(work)

    async def _invoke_task(self, task_context: TaskContext):
        try:
            output, attributes = await self._hook("on_task_invoke", task_context)
            task_result = TaskResultProto(return_code=0)
            if output is not None:
                task_result.output = output
            return OnTaskInvokeResponse(task_result=task_result, attributes=attributes)
        except _HookError as exc:
            logger.exception("OnTaskInvoke failed")
            return OnTaskInvokeResponse(
                task_result=TaskResultProto(return_code=-1, message=str(exc)),
                attributes=exc.attributes,
            )
        except Exception as exc:
            logger.exception("OnTaskInvoke failed")
            return OnTaskInvokeResponse(task_result=TaskResultProto(return_code=-1, message=str(exc)))
        finally:
            self._active_tasks -= 1

    async def OnSessionLeave(self, request, context):  # noqa: N802
        try:
            async with self._binding_lock:
                await self._binding_hook("on_session_leave")
            return Result(return_code=0)
        except Exception as exc:
            logger.exception("OnSessionLeave failed")
            return Result(return_code=-1, message=str(exc))


class FlameInstanceServer:
    """AsyncIO server for the executor's Unix socket."""

    def __init__(self, service: SyncFlameService):
        self._servicer = FlameInstanceServicer(service)
        self._server = None
        self._closed = False

    async def start(self):
        endpoint = os.getenv(FLAME_INSTANCE_ENDPOINT)
        if not endpoint:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "FLAME_INSTANCE_ENDPOINT not found")
        # Task admission is bounded in the servicer. A gRPC-wide cap could
        # reject session enter/leave when task calls fill all RPC slots.
        self._server = grpc.aio.server()
        try:
            add_InstanceServicer_to_server(self._servicer, self._server)
            port = self._server.add_insecure_port(f"unix://{endpoint}")
            if not port:
                raise FlameError(FlameErrorCode.INTERNAL, f"Failed to bind shim socket: {endpoint}")
            await self._server.start()
        except Exception:
            self._closed = True
            await self._server.stop(grace=0)
            await self._servicer.close()
            raise

    async def wait_for_termination(self):
        if self._server is None:
            raise RuntimeError("Shim server is not started")
        await self._server.wait_for_termination()

    async def stop(self):
        if self._closed:
            return
        self._closed = True
        if self._server is not None:
            await self._server.stop(grace=5)
        await self._servicer.close()


async def run(service: SyncFlameService):
    """Run the shim server until it is stopped."""
    server = FlameInstanceServer(service)
    await server.start()
    try:
        await server.wait_for_termination()
    finally:
        await server.stop()

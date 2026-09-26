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
import tempfile
import threading
from pathlib import Path

import cloudpickle
import grpc
import pytest

from flamepy.app.runpy import FlameRunpyService
from flamepy.app.types import ServiceContext, ServiceRequest
from flamepy.core.aio import service as aio_service
from flamepy.core.service import ApplicationContext, SessionContext, TaskContext
from flamepy.proto import shim_pb2, types_pb2
from flamepy.proto.shim_pb2_grpc import InstanceStub


def test_aio_shim_handles_concurrent_hooks_and_isolates_attributes(monkeypatch):

    class Service(aio_service.FlameService):
        async def on_session_enter(self, context):
            assert context.session_id == "session"

        async def on_task_invoke(self, context):
            self.publish({context.task_id.encode()})
            await asyncio.sleep(0.01)
            return context.input

        async def on_session_leave(self):
            pass

    async def exercise():
        server = aio_service.FlameInstanceServer(Service())
        await server.start()
        try:
            async with grpc.aio.insecure_channel(f"unix://{endpoint}") as channel:
                stub = InstanceStub(channel)
                enter = await stub.OnSessionEnter(
                    shim_pb2.SessionContext(
                        session_id="session",
                        application=shim_pb2.ApplicationContext(name="app"),
                    )
                )
                assert enter.result.return_code == 0
                responses = await asyncio.gather(*(stub.OnTaskInvoke(shim_pb2.TaskContext(task_id=str(i), session_id="session", input=b"payload")) for i in range(12)))
                for i, response in enumerate(responses):
                    assert response.task_result.return_code == 0
                    assert response.task_result.output == b"payload"
                    assert set(response.attributes.attr) == {str(i).encode()}
                leave = await stub.OnSessionLeave(types_pb2.EmptyRequest())
                assert leave.return_code == 0
        finally:
            await server.stop()

    with tempfile.TemporaryDirectory(prefix="flame-shim-", dir="/tmp") as directory:
        endpoint = str(Path(directory) / "shim.sock")
        monkeypatch.setenv(aio_service.FLAME_INSTANCE_ENDPOINT, endpoint)
        asyncio.run(exercise())


def test_aio_shim_moves_synchronous_hooks_off_event_loop():
    class Service(aio_service.SyncFlameService):
        def on_session_enter(self, context):
            pass

        def on_task_invoke(self, context):
            self.publish({b"sync"})
            return threading.current_thread().name.encode()

        def on_session_leave(self):
            pass

    async def exercise():
        servicer = aio_service.FlameInstanceServicer(Service())
        try:
            response = await servicer.OnTaskInvoke(shim_pb2.TaskContext(task_id="task", session_id="session", input=b""), None)
            assert response.task_result.output.startswith(b"flame-shim")
            assert set(response.attributes.attr) == {b"sync"}
        finally:
            await servicer.close()

    asyncio.run(exercise())


def test_task_limit_keeps_session_control_available():
    started = asyncio.Event()
    release = asyncio.Event()

    class Service(aio_service.FlameService):
        async def on_session_enter(self, context):
            pass

        async def on_task_invoke(self, context):
            started.set()
            await release.wait()
            return b"done"

        async def on_session_leave(self):
            pass

    async def exercise():
        servicer = aio_service.FlameInstanceServicer(Service(), max_inflight=1)
        try:
            first = asyncio.create_task(servicer.OnTaskInvoke(shim_pb2.TaskContext(task_id="first", session_id="session"), None))
            await started.wait()
            excess = await servicer.OnTaskInvoke(shim_pb2.TaskContext(task_id="excess", session_id="session"), None)
            assert excess.task_result.return_code == -1
            assert "too many concurrent" in excess.task_result.message
            leave = asyncio.create_task(servicer.OnSessionLeave(types_pb2.EmptyRequest(), None))
            await asyncio.sleep(0)
            assert (await leave).return_code == 0
            release.set()
            assert (await first).task_result.output == b"done"
        finally:
            await servicer.close()

    asyncio.run(exercise())


def test_cancelled_enter_keeps_binding_blocked_until_worker_finishes():
    started = threading.Event()
    release = threading.Event()
    observations = []

    class Service(aio_service.SyncFlameService):
        def on_session_enter(self, context):
            started.set()
            if not release.wait(timeout=5):
                raise TimeoutError("test worker was not released")
            observations.append("entered")

        def on_task_invoke(self, context):
            return b"done"

        def on_session_leave(self):
            observations.append("left")

    async def exercise():
        servicer = aio_service.FlameInstanceServicer(Service())
        try:
            enter = asyncio.create_task(
                servicer.OnSessionEnter(
                    shim_pb2.SessionContext(
                        session_id="session",
                        application=shim_pb2.ApplicationContext(name="app"),
                    ),
                    None,
                )
            )
            assert await asyncio.to_thread(started.wait, 2)
            enter.cancel()
            leave = asyncio.create_task(servicer.OnSessionLeave(types_pb2.EmptyRequest(), None))
            await asyncio.sleep(0)
            assert not enter.done()
            assert not leave.done()
            release.set()
            with pytest.raises(asyncio.CancelledError):
                await enter
            assert (await leave).return_code == 0
            assert observations == ["entered", "left"]
        finally:
            release.set()
            await servicer.close()

    asyncio.run(exercise())


def test_close_cancels_stalled_async_hook_after_timeout(monkeypatch):
    monkeypatch.setattr(aio_service, "_SHUTDOWN_TIMEOUT", 0.05)
    started = asyncio.Event()

    class Service(aio_service.FlameService):
        async def on_session_enter(self, context):
            started.set()
            await asyncio.Event().wait()

        async def on_task_invoke(self, context):
            return None

        async def on_session_leave(self):
            pass

    async def exercise():
        servicer = aio_service.FlameInstanceServicer(Service())
        enter = asyncio.create_task(
            servicer.OnSessionEnter(
                shim_pb2.SessionContext(session_id="session", application=shim_pb2.ApplicationContext(name="app")),
                None,
            )
        )
        await started.wait()
        await asyncio.wait_for(servicer.close(), timeout=0.5)
        with pytest.raises(asyncio.CancelledError):
            await enter

    asyncio.run(exercise())


def test_close_does_not_wait_indefinitely_for_sync_hook(monkeypatch):
    monkeypatch.setattr(aio_service, "_SHUTDOWN_TIMEOUT", 0.05)
    started = threading.Event()
    release = threading.Event()
    finished = threading.Event()

    class Service(aio_service.SyncFlameService):
        def on_session_enter(self, context):
            pass

        def on_task_invoke(self, context):
            started.set()
            try:
                release.wait(timeout=5)
                return b"done"
            finally:
                finished.set()

        def on_session_leave(self):
            pass

    async def exercise():
        servicer = aio_service.FlameInstanceServicer(Service())
        task = asyncio.create_task(servicer.OnTaskInvoke(shim_pb2.TaskContext(task_id="task", session_id="session"), None))
        try:
            assert await asyncio.to_thread(started.wait, 2)
            await asyncio.wait_for(servicer.close(), timeout=0.5)
            assert not finished.is_set()
            with pytest.raises(asyncio.CancelledError):
                await task
        finally:
            release.set()
            assert await asyncio.to_thread(finished.wait, 2)

    asyncio.run(exercise())


def test_aio_app_worker_awaits_function_and_class_method(monkeypatch):
    async def function(value):
        await asyncio.sleep(0)
        return value + 1

    class Worker:
        def __init__(self, value):
            self.value = value

        async def add(self, amount):
            await asyncio.sleep(0)
            return self.value + amount

    stored = []

    async def fake_put_object(key_prefix, value):
        stored.append((key_prefix, value))
        return type("Ref", (), {"encode": lambda self: b"ref"})()

    # The aio cache function is imported when the worker handles a task.
    monkeypatch.setattr("flamepy.core.aio.put_object", fake_put_object)

    async def exercise(execution_object, request, constructor_args=None):
        service = FlameRunpyService()

        async def load_context(context):
            return ServiceContext(execution_object, constructor_args=constructor_args)

        service._load_app_context = load_context
        await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))
        result = await service.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(request)))
        assert result == b"ref"
        await service.on_session_leave()

    asyncio.run(exercise(function, ServiceRequest(args=(4,))))
    asyncio.run(exercise(Worker, ServiceRequest(method="add", args=(3,)), constructor_args=(7,)))
    assert stored == [("app/session", 5), ("app/session", 10)]


def test_aio_app_worker_retains_class_across_session_bindings(monkeypatch):
    class Worker:
        creations = 0

        def __init__(self):
            type(self).creations += 1
            self.calls = 0

        async def call(self):
            self.calls += 1
            return self.calls

    stored = []

    async def fake_put_object(key_prefix, value):
        stored.append((key_prefix, value))
        return type("Ref", (), {"encode": lambda self: b"ref"})()

    monkeypatch.setattr("flamepy.core.aio.put_object", fake_put_object)
    service = FlameRunpyService()

    async def load_context(context):
        return ServiceContext(Worker, constructor_args=(), service_id="worker")

    service._load_app_context = load_context

    async def exercise():
        for session_id in ("first", "second"):
            await service.on_session_enter(SessionContext(None, session_id, ApplicationContext("app")))
            result = await service.on_task_invoke(
                TaskContext(
                    "task",
                    session_id,
                    cloudpickle.dumps(ServiceRequest(method="call")),
                )
            )
            assert result == b"ref"
            await service.on_session_leave()

    asyncio.run(exercise())
    assert Worker.creations == 1
    assert stored == [("app/first", 1), ("app/second", 2)]


def test_aio_app_worker_isolates_invocation_attributes(monkeypatch):
    import flamepy.app as app

    async def function(value):
        app.publish_attributes({str(value).encode()})
        await asyncio.sleep(0.01)
        assert app.session_context().session_id == "session"
        return value

    async def fake_put_object(key_prefix, value):
        return type("Ref", (), {"encode": lambda self: b"ref"})()

    monkeypatch.setattr("flamepy.core.aio.put_object", fake_put_object)
    service = FlameRunpyService()

    async def load_context(context):
        return ServiceContext(function)

    service._load_app_context = load_context

    async def exercise():
        await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))

        async def invoke(value):
            await service.on_task_invoke(
                TaskContext(
                    str(value),
                    "session",
                    cloudpickle.dumps(ServiceRequest(args=(value,))),
                )
            )
            return set(service._take_attributes().attr)

        assert await asyncio.gather(invoke(1), invoke(2)) == [{b"1"}, {b"2"}]

    asyncio.run(exercise())

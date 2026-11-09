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

Serving setup and teardown through direct lifecycle RPCs and debug mode.
"""

import asyncio
import tempfile
import threading
from pathlib import Path

import cloudpickle
import grpc
import pytest
from fastapi.testclient import TestClient

from flamepy.core.aio import service
from flamepy.core.service import FlameInstanceServicer
from flamepy.proto import shim_pb2, types_pb2
from flamepy.proto.shim_pb2_grpc import InstanceStub
from flamepy.proto.types_pb2 import EmptyRequest
from flamepy.serving.instance import Instance, run_debug_service


@pytest.mark.asyncio
@pytest.mark.parametrize("service_class", [service.SyncFlameService, service.FlameService])
async def test_instance_hooks_default_to_noops(service_class):
    servicer = service.FlameInstanceServicer(service_class())
    try:
        assert (await servicer.OnInstanceCreate(EmptyRequest(), None)).return_code == 0
        assert (await servicer.OnInstanceDestroy(EmptyRequest(), None)).return_code == 0
    finally:
        await servicer.close()


@pytest.mark.asyncio
@pytest.mark.parametrize("synchronous", [False, True])
async def test_instance_rpc_dispatches_each_request_to_the_hook(synchronous):
    events = []

    class SyncService(service.SyncFlameService):
        def on_instance_create(self):
            assert threading.current_thread().name.startswith("flame-shim")
            events.append("create")

        def on_instance_destroy(self):
            assert threading.current_thread().name.startswith("flame-shim")
            events.append("destroy")

    class AioService(service.FlameService):
        async def on_instance_create(self):
            assert asyncio.get_running_loop() is loop
            events.append("create")

        async def on_instance_destroy(self):
            assert asyncio.get_running_loop() is loop
            events.append("destroy")

    loop = asyncio.get_running_loop()
    servicer = service.FlameInstanceServicer(SyncService() if synchronous else AioService())
    try:
        for _ in range(2):
            assert (await servicer.OnInstanceCreate(EmptyRequest(), None)).return_code == 0
            assert (await servicer.OnInstanceDestroy(EmptyRequest(), None)).return_code == 0
        assert events == ["create", "destroy", "create", "destroy"]
    finally:
        await servicer.close()


@pytest.mark.asyncio
@pytest.mark.parametrize("hook, rpc", [("on_instance_create", "OnInstanceCreate"), ("on_instance_destroy", "OnInstanceDestroy")])
@pytest.mark.parametrize("synchronous", [False, True])
async def test_instance_hook_error_is_returned_to_the_caller(hook, rpc, synchronous):
    def fail(self):
        raise ValueError("model lifecycle failed")

    async def fail_async(self):
        fail(self)

    base = service.SyncFlameService if synchronous else service.FlameService

    class Service(base):
        pass

    setattr(Service, hook, fail if synchronous else fail_async)
    servicer = service.FlameInstanceServicer(Service())
    try:
        result = await getattr(servicer, rpc)(EmptyRequest(), None)
        assert result.return_code == -1
        assert result.message == "model lifecycle failed"
    finally:
        await servicer.close()


@pytest.mark.asyncio
async def test_task_dispatch_does_not_run_instance_setup_implicitly():
    class Service(service.FlameService):
        async def on_instance_create(self):
            raise AssertionError("only OnInstanceCreate should call setup")

        async def on_task_invoke(self, context):
            return b"done"

    servicer = service.FlameInstanceServicer(Service())
    try:
        response = await servicer.OnTaskInvoke(shim_pb2.TaskContext(task_id="task", session_id="session"), None)
        assert response.task_result.return_code == 0
        assert response.task_result.output == b"done"
    finally:
        await servicer.close()


@pytest.mark.asyncio
async def test_instance_rpc_setup_starts_after_server_is_listening(monkeypatch):
    started = asyncio.Event()
    release = asyncio.Event()
    events = []

    class Service(service.FlameService):
        async def on_instance_create(self):
            started.set()
            await release.wait()
            events.append("created")

        async def on_instance_destroy(self):
            events.append("destroyed")

    with tempfile.TemporaryDirectory(prefix="flame-lifecycle-", dir="/tmp") as directory:
        endpoint = str(Path(directory) / "instance.sock")
        monkeypatch.setenv(service.FLAME_INSTANCE_ENDPOINT, endpoint)
        server = service.FlameInstanceServer(Service())
        await server.start()
        try:
            assert Path(f"{endpoint}.ready").exists()
            assert not started.is_set()
            async with grpc.aio.insecure_channel(f"unix://{endpoint}") as channel:
                stub = InstanceStub(channel)
                create = asyncio.ensure_future(stub.OnInstanceCreate(types_pb2.EmptyRequest()))
                await asyncio.wait_for(started.wait(), 2)
                assert not create.done()
                release.set()
                assert (await create).return_code == 0
                assert (await stub.OnInstanceDestroy(types_pb2.EmptyRequest())).return_code == 0
                assert events == ["created", "destroyed"]
        finally:
            release.set()
            await server.stop()


def test_serving_lifecycle_registration_and_sync_adapter():
    instance = Instance()
    events = []

    @instance.setup
    def setup():
        events.append("setup")

    @instance.teardown
    def teardown():
        events.append("teardown")

    assert instance._setup is setup
    assert instance._teardown is teardown
    assert not events
    with pytest.raises(ValueError, match="already registered"):
        instance.setup(lambda: None)
    with pytest.raises(ValueError, match="already registered"):
        instance.teardown(lambda: None)

    servicer = FlameInstanceServicer(instance)
    try:
        assert servicer.OnInstanceCreate(EmptyRequest(), None).return_code == 0
        instance.on_session_leave()
        assert events == ["setup"]
        assert servicer.OnInstanceDestroy(EmptyRequest(), None).return_code == 0
        assert events == ["setup", "teardown"]
    finally:
        servicer.close()


@pytest.mark.parametrize("decorator", ["setup", "teardown"])
def test_serving_lifecycle_requires_sync_zero_argument_callbacks(decorator):
    instance = Instance()

    async def async_callback():
        pass

    def with_argument(value):
        pass

    def generator():
        yield "setup would never run"

    class GeneratorCallback:
        def __call__(self):
            yield "setup would never run"

    register = getattr(instance, decorator)
    with pytest.raises(TypeError, match="synchronous"):
        register(async_callback)
    with pytest.raises(TypeError, match="zero arguments"):
        register(with_argument)
    with pytest.raises(TypeError, match="synchronous"):
        register(None)
    for callback in [generator, GeneratorCallback()]:
        with pytest.raises(TypeError, match="generator"):
            register(callback)


def test_serving_debug_lifespan_runs_setup_before_requests_and_teardown(monkeypatch):
    instance = Instance()
    events = []

    @instance.setup
    def setup():
        events.append("setup")

    @instance.teardown
    def teardown():
        events.append("teardown")

    @instance.entrypoint
    async def generate(request):
        assert events[0] == "setup"
        events.append("task")
        return request + 1

    def run(app, **kwargs):
        with TestClient(app) as client:
            assert events == ["setup"]
            response = client.post("/generate", content=cloudpickle.dumps(41))
            assert response.status_code == 200
            assert cloudpickle.loads(response.content) == 42
            assert events == ["setup", "task"]

    monkeypatch.setattr("flamepy.serving.instance.uvicorn.run", run)
    run_debug_service(instance)
    assert events == ["setup", "task", "teardown"]


def test_serving_debug_setup_failure_runs_partial_teardown(monkeypatch):
    instance = Instance()
    events = []

    @instance.setup
    def setup():
        events.append("setup")
        raise ValueError("model is unavailable")

    @instance.teardown
    def teardown():
        events.append("teardown")

    def run(app, **kwargs):
        with pytest.raises(RuntimeError, match="model is unavailable"), TestClient(app):
            pytest.fail("debug requests must not start after failed setup")

    monkeypatch.setattr("flamepy.serving.instance.uvicorn.run", run)
    run_debug_service(instance)
    assert events == ["setup", "teardown"]

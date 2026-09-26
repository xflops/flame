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
import threading
from concurrent.futures import Future
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import cloudpickle
import pytest

import flamepy.app.client as app_client
from flamepy.app.runpy import FlameRunpyService
from flamepy.app.types import ServiceContext, ServiceRequest, ServiceResponse
from flamepy.core import ValueRef


@pytest.mark.asyncio
async def test_runpy_rejects_output_above_128_kib():
    from flamepy.app.types import INLINE_PAYLOAD_LIMIT
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    def echo(data):
        return data

    service = FlameRunpyService()
    service._load_app_context = lambda context: _context(ServiceContext(echo))
    await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))

    size = next(size for size in range(INLINE_PAYLOAD_LIMIT - 384, INLINE_PAYLOAD_LIMIT) if len(cloudpickle.dumps(ServiceResponse(ValueRef(b"x" * size)))) == INLINE_PAYLOAD_LIMIT)
    exact = b"x" * size
    with patch("flamepy.app.runpy.aio_core.put_object", new=AsyncMock()) as put:
        output = await service.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(ServiceRequest(args=(exact,)))))
        assert cloudpickle.loads(output) == ServiceResponse(ValueRef(exact))
        with pytest.raises(ValueError, match="App task output exceeds"):
            await service.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(ServiceRequest(args=(exact + b"x",)))))
    put.assert_not_awaited()


@pytest.mark.asyncio
async def test_runpy_rejects_worker_objectfuture_with_clear_error():
    from flamepy.app import ObjectFuture
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    def return_future():
        return ObjectFuture(Future())

    service = FlameRunpyService()
    service._load_app_context = lambda context: _context(ServiceContext(return_future))
    await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))

    with pytest.raises(TypeError, match="cannot return ObjectFuture"):
        await service.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(ServiceRequest())))


@pytest.mark.asyncio
async def test_runpy_wraps_explicit_objectref_in_service_response():
    from flamepy.core import ObjectRef
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/value", version=1)

    def return_ref():
        return ref

    service = FlameRunpyService()
    service._load_app_context = lambda context: _context(ServiceContext(return_ref))
    await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))
    with patch("flamepy.app.runpy.aio_core.put_object", new=AsyncMock()) as put:
        output = await service.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(ServiceRequest())))
    assert cloudpickle.loads(output) == ServiceResponse(ref)
    put.assert_not_awaited()


async def _context(value):
    return value


@pytest.mark.asyncio
async def test_runpy_resolves_object_ref_to_cached_none():
    """Test cached None is a valid ObjectRef value, not a retrieval miss."""
    from flamepy.core.cache import ObjectRef

    svc = FlameRunpyService()
    ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/object", version=1)

    with patch("flamepy.core.aio.get_object", new=AsyncMock(return_value=None)) as get:
        args, kwargs = await svc._resolve_object_refs((ref,), {"value": ref})
        assert get.await_count == 2
        assert args == (None,)
        assert kwargs == {"value": None}


@pytest.mark.asyncio
async def test_runpy_resolves_inline_and_cached_refs_in_arguments():
    from flamepy.core import ObjectRef

    service = FlameRunpyService()
    ref = ObjectRef(endpoint="grpc://host:9090", key="app/session/cached", version=1)

    async def get_object(value):
        return value.value if isinstance(value, ValueRef) else "cached"

    with patch("flamepy.app.runpy.aio_core.get_object", new=AsyncMock(side_effect=get_object)) as get:
        args, kwargs = await service._resolve_object_refs((ValueRef("inline"), ref), {"inline": ValueRef(None)})
    assert args == ("inline", "cached")
    assert kwargs == {"inline": None}
    assert get.await_count == 3


@pytest.mark.asyncio
async def test_runpy_binds_session_context_and_publishes_invocation_attributes():
    import flamepy.app as app
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    class Worker:
        def run(self):
            app.publish_attributes({b"b"})
            app.publish_attributes({b"c"})
            return app.session_context().session_id

    svc = FlameRunpyService()
    svc._load_app_context = lambda context: _context(ServiceContext(Worker, constructor_args=()))
    session = SessionContext(None, "session", ApplicationContext("app"))

    await svc.on_session_enter(session)
    assert list(svc._take_attributes().attr) == []

    request = ServiceRequest(method="run")
    object_ref = SimpleNamespace(encode=lambda: b"result-ref")
    with patch("flamepy.core.aio.put_object", new=AsyncMock(return_value=object_ref)) as put:
        result = await svc.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(request)))

    assert cloudpickle.loads(result) == ServiceResponse(ValueRef("session"))
    put.assert_not_called()
    assert set(svc._take_attributes().attr) == {b"b", b"c"}


@pytest.mark.asyncio
async def test_runpy_handles_async_method_and_sync_method_returning_awaitable():
    import flamepy.app as app
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    class Worker:
        async def async_method(self):
            app.publish_attributes({b"async"})
            return app.session_context().session_id

        def returns_awaitable(self):
            async def resolve():
                app.publish_attributes({b"awaitable"})
                return app.session_context().session_id

            return resolve()

    service = FlameRunpyService()
    service._load_app_context = lambda context: _context(ServiceContext(Worker, constructor_args=()))
    await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))

    for method, attribute in (("async_method", b"async"), ("returns_awaitable", b"awaitable")):
        request = TaskContext("task", "session", cloudpickle.dumps(ServiceRequest(method=method)))
        output = await service.on_task_invoke(request)
        assert cloudpickle.loads(output) == ServiceResponse(ValueRef("session"))
        assert list(service._take_attributes().attr) == [attribute]


@pytest.mark.asyncio
async def test_runpy_request_decode_does_not_block_event_loop():
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    started = threading.Event()
    release = threading.Event()

    def slow_decode(_input):
        started.set()
        if not release.wait(timeout=5):
            raise TimeoutError("test did not release request decoding")
        return ServiceRequest()

    service = FlameRunpyService()
    service._load_app_context = lambda context: _context(ServiceContext(lambda: b"ok"))
    await service.on_session_enter(SessionContext(None, "session", ApplicationContext("app")))

    timer = threading.Timer(2, release.set)
    timer.start()
    try:
        with patch("flamepy.app.runpy.cloudpickle.loads", side_effect=slow_decode):
            call = asyncio.create_task(service.on_task_invoke(TaskContext("task", "session", b"request")))
            assert await asyncio.to_thread(started.wait, 3)
            assert not release.is_set(), "request decoding blocked the event loop"
            release.set()
            await call
    finally:
        release.set()
        timer.cancel()


@pytest.mark.asyncio
async def test_runpy_binds_recursive_service_to_current_session(monkeypatch):
    import flamepy.app as app
    from flamepy import FlameError
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    captured = {}

    class Worker:
        def run(self):
            @app.service()
            class RecursiveProxy:
                def run(self):
                    return None

            recursive_proxy = RecursiveProxy.remote()
            captured["proxy"] = recursive_proxy
            captured["execution_object"] = recursive_proxy._execution_object
            return recursive_proxy._session.id

    monkeypatch.setattr(app_client, "_runtime", None)
    put_context = MagicMock()
    monkeypatch.setattr("flamepy.app.client._core_put_object", put_context)
    borrowed_session = MagicMock(id="recursive-session")
    open_session = MagicMock(return_value=borrowed_session)
    monkeypatch.setattr("flamepy.app.client.core_client.open_session", open_session)

    svc = FlameRunpyService()
    svc._load_app_context = lambda context: _context(ServiceContext(Worker, constructor_args=()))
    session_context = SessionContext(None, "recursive-session", ApplicationContext("recursive-app"))
    await svc.on_session_enter(session_context)

    request = ServiceRequest(method="run")
    result_ref = SimpleNamespace(encode=lambda: b"result-ref")
    with patch("flamepy.core.aio.put_object", new=AsyncMock(return_value=result_ref)):
        result = await svc.on_task_invoke(TaskContext("task", "recursive-session", cloudpickle.dumps(request)))

    assert cloudpickle.loads(result) == ServiceResponse(ValueRef("recursive-session"))
    assert captured["proxy"]._session_context is session_context
    assert "_session_context" not in captured["execution_object"].__dict__
    put_context.assert_not_called()
    open_session.assert_called_once_with(session_id="recursive-session")
    with pytest.raises(RuntimeError, match="not running in a Flame invocation"):
        app.session_context()

    captured["proxy"].close()
    borrowed_session.close.assert_not_called()

    with pytest.raises(FlameError, match="flamepy.app.init"):

        @app.service()
        def outside_invocation():
            return None


@pytest.mark.asyncio
async def test_runpy_resets_recursive_session_context_after_failure(monkeypatch):
    import flamepy.app as app
    from flamepy import FlameError
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    class Worker:
        def run(self):
            @app.service()
            def recursive_proxy():
                return None

            raise RuntimeError("service failed")

    monkeypatch.setattr(app_client, "_runtime", None)
    monkeypatch.setattr(
        "flamepy.app.client.core_client.open_session",
        MagicMock(return_value=MagicMock(id="recursive-session")),
    )

    svc = FlameRunpyService()
    svc._load_app_context = lambda context: _context(ServiceContext(Worker, constructor_args=()))
    await svc.on_session_enter(SessionContext(None, "recursive-session", ApplicationContext("recursive-app")))
    request = ServiceRequest(method="run")

    with pytest.raises(RuntimeError, match="service failed"):
        await svc.on_task_invoke(TaskContext("task", "recursive-session", cloudpickle.dumps(request)))

    with pytest.raises(RuntimeError, match="not running in a Flame invocation"):
        app.session_context()
    with pytest.raises(FlameError, match="flamepy.app.init"):

        @app.service()
        def outside_invocation():
            return None


@pytest.mark.asyncio
async def test_app_runtime_helpers_are_invocation_scoped():
    import flamepy.app as app
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    class Worker:
        def run(self):
            app.publish_attributes({b"initial"})
            app.publish_attributes({b"initial", b"next"})
            return app.session_context().session_id

        def invalid(self):
            app.publish_attributes({"invalid"})

    with pytest.raises(RuntimeError, match="not running in a Flame invocation"):
        app.session_context()
    with pytest.raises(RuntimeError, match="not running in a Flame invocation"):
        app.publish_attributes({b"outside"})

    svc = FlameRunpyService()
    svc._load_app_context = lambda context: _context(ServiceContext(Worker, constructor_args=()))
    session = SessionContext(None, "session", ApplicationContext("app"))
    await svc.on_session_enter(session)

    result_ref = SimpleNamespace(encode=lambda: b"result-ref")
    with patch("flamepy.core.aio.put_object", new=AsyncMock(return_value=result_ref)):
        result = await svc.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(ServiceRequest(method="run"))))
    assert cloudpickle.loads(result) == ServiceResponse(ValueRef("session"))
    assert set(svc._take_attributes().attr) == {b"initial", b"next"}

    with pytest.raises(TypeError, match="must contain bytes"):
        await svc.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(ServiceRequest(method="invalid"))))
    assert list(svc._take_attributes().attr) == []


@pytest.mark.asyncio
async def test_runpy_uses_constructor_args_to_distinguish_class_from_function():
    """Executor construction is driven by the serialized context contract."""

    class Worker:
        def __init__(self, value):
            self.value = value

    def function():
        return "function"

    class_context = ServiceContext(Worker, constructor_args=(7,))
    function_context = ServiceContext(function)
    svc = FlameRunpyService()

    with patch("inspect.isclass") as isclass:
        svc._set_execution_from_context(class_context)
        assert isinstance(svc._execution_object, Worker)
        assert svc._execution_object.value == 7

        svc._set_execution_from_context(function_context)
        assert svc._execution_object is function

    isclass.assert_not_called()


@pytest.mark.asyncio
async def test_runpy_reuses_class_execution_object_between_sessions():
    from flamepy.core.service import ApplicationContext, SessionContext, TaskContext

    class Worker:
        init_count = 0

        def __init__(self):
            type(self).init_count += 1

        def session_id(self):
            import flamepy.app as app

            return app.session_context().session_id

        def fail(self):
            import flamepy.app as app

            app.publish_attributes({b"failed"})
            raise RuntimeError("failed")

    class OtherWorker:
        pass

    svc = FlameRunpyService()
    contexts = iter(
        [
            ServiceContext(
                Worker,
                constructor_args=(),
                service_id="worker-service",
            ),
            ServiceContext(
                Worker,
                constructor_args=(),
                service_id="worker-service",
            ),
            ServiceContext(
                OtherWorker,
                constructor_args=(),
                service_id="other-service",
            ),
        ]
    )
    svc._load_app_context = lambda context: _context(next(contexts))
    session = SessionContext(None, "session", ApplicationContext("app"))

    await svc.on_session_enter(session)
    assert Worker.init_count == 1
    assert list(svc._take_attributes().attr) == []

    request = ServiceRequest(method="fail")
    with pytest.raises(RuntimeError, match="failed"):
        await svc.on_task_invoke(TaskContext("task", "session", cloudpickle.dumps(request)))
    assert set(svc._take_attributes().attr) == {b"failed"}

    with pytest.raises(ValueError, match="Task input is None"):
        await svc.on_task_invoke(TaskContext("invalid-task", "session", None))
    assert list(svc._take_attributes().attr) == []

    original = svc._execution_object
    await svc.on_session_leave()
    assert svc._ssn_ctx is None
    assert svc._app_context is None
    assert svc._execution_object is None

    rebound = SessionContext(None, "rebound-session", ApplicationContext("app"))
    await svc.on_session_enter(rebound)
    assert svc._execution_object is original
    assert Worker.init_count == 1
    assert list(svc._take_attributes().attr) == []

    result_ref = SimpleNamespace(encode=lambda: b"result-ref")
    with patch("flamepy.core.aio.put_object", new=AsyncMock(return_value=result_ref)):
        await svc.on_task_invoke(
            TaskContext(
                "task",
                "rebound-session",
                cloudpickle.dumps(ServiceRequest(method="session_id")),
            )
        )

    previous = svc._execution_object
    other = SessionContext(None, "other-session", ApplicationContext("app"))
    await svc.on_session_leave()
    await svc.on_session_enter(other)
    assert svc._execution_object is not previous
    assert isinstance(svc._execution_object, OtherWorker)
    assert list(svc._take_attributes().attr) == []


@pytest.mark.asyncio
async def test_runpy_does_not_reuse_functions():

    def first_function():
        return "first"

    def second_function():
        return "second"

    svc = FlameRunpyService()

    for execution_object in (
        first_function,
        second_function,
    ):
        svc._set_execution_from_context(ServiceContext(execution_object))
        assert svc._execution_object is execution_object
        await svc.on_session_leave()


@pytest.mark.asyncio
async def test_runpy_distinguishes_constructed_handles_by_service_id():

    class Worker:
        pass

    svc = FlameRunpyService()
    svc._set_execution_from_context(
        ServiceContext(
            Worker,
            constructor_args=(),
            service_id="first-worker",
        )
    )
    first_execution_object = svc._execution_object
    await svc.on_session_leave()

    svc._set_execution_from_context(
        ServiceContext(
            Worker,
            constructor_args=(),
            service_id="second-worker",
        )
    )
    assert svc._execution_object is not first_execution_object

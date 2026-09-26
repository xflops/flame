"""Tests for flamepy service APIs."""

import gc
import logging
import os
import threading
import types

import cloudpickle
import pytest

import flamepy.core.service as service
import flamepy.service.client as service_client
from flamepy.core.service import ApplicationContext, SessionContext, TaskContext
from flamepy.core.types import TaskOutput
from flamepy.proto import shim_pb2
from flamepy.proto.types_pb2 import Result as ResultProto
from flamepy.service.instance import FlameInstance


class _NoopService(service.FlameService):
    def on_session_enter(self, context):
        pass

    def on_task_invoke(self, context):
        pass

    def on_session_leave(self):
        pass


# Core Service Tests


class DummyContext:
    pass


def test_tracefn_logs_enter_and_exit(caplog):
    caplog.set_level(logging.DEBUG)
    name = "TraceTest"
    t = service.TraceFn(name)
    # Enter log should appear on creation
    assert any(f"{name} Enter" in rec.getMessage() for rec in caplog.records)
    # Force destruction to trigger __del__ and Exit log
    del t
    gc.collect()
    assert any(f"{name} Exit" in rec.getMessage() for rec in caplog.records)


def test_dataclasses_fields_and_methods():
    app = service.ApplicationContext("my-app", image="my-image:latest", command="run", working_directory="/work", url="http://example/")
    assert app.name == "my-app"
    assert app.image == "my-image:latest"
    assert app.command == "run"
    assert app.working_directory == "/work"
    assert app.url == "http://example/"

    sess = service.SessionContext(_common_data=b"ABC", session_id="sess-1", application=app)
    assert sess.session_id == "sess-1"
    assert sess.application is app
    assert sess.common_data() == b"ABC"

    task = service.TaskContext(task_id="task-1", session_id="sess-1", input=b"in")
    assert task.task_id == "task-1"
    assert task.session_id == "sess-1"
    assert task.input == b"in"


def test_session_context_round_trips_through_cloudpickle():
    app = service.ApplicationContext("my-app")
    context = service.SessionContext(_common_data=b"ABC", session_id="sess-1", application=app)

    restored = cloudpickle.loads(cloudpickle.dumps(context))

    assert restored.common_data() == b"ABC"
    assert not hasattr(restored, "publish")


def test_publish_calls_are_unioned_and_sent_once():
    publisher = _NoopService()
    publisher.publish([b"kv-cache-key", b"kv-cache-key"])
    publisher.publish([b"prefix-key", b"block-key"])
    publisher.publish([])

    attributes = publisher._take_attributes()
    assert set(attributes.attr) == {b"kv-cache-key", b"prefix-key", b"block-key"}
    assert list(publisher._take_attributes().attr) == []

    publisher.publish([])
    republished = publisher._take_attributes()
    assert list(republished.attr) == []

    instance = FlameInstance()
    instance.publish({b"instance-key"})
    assert instance._take_attributes().attr == [b"instance-key"]


def test_publish_racing_response_delivers_one_complete_snapshot():
    publisher = service._Publisher()
    barrier = threading.Barrier(2)

    def update():
        barrier.wait()
        publisher.publish(frozenset({b"race-a", b"race-b"}))

    thread = threading.Thread(target=update)
    thread.start()
    barrier.wait()
    raced = publisher.take()
    thread.join()
    after_race = publisher.take()
    nonempty = [set(attributes.attr) for attributes in (raced, after_race) if attributes.attr]

    assert nonempty == [{b"race-a", b"race-b"}]


def test_concurrent_servicers_publish_to_their_own_instance():
    barrier = threading.Barrier(2)

    class PublishingService(service.FlameService):
        def __init__(self, key):
            self.key = key

        def on_session_enter(self, context):
            pass

        def on_task_invoke(self, context):
            barrier.wait()
            self.publish({self.key})

        def on_session_leave(self):
            pass

    servicers = [
        service.FlameInstanceServicer(PublishingService(b"instance-a")),
        service.FlameInstanceServicer(PublishingService(b"instance-b")),
    ]
    responses = [None, None]

    def invoke(index):
        responses[index] = servicers[index].OnTaskInvoke(
            shim_pb2.TaskContext(task_id=f"task-{index}", session_id="session"),
            DummyContext(),
        )

    threads = [threading.Thread(target=invoke, args=(index,)) for index in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert set(responses[0].attributes.attr) == {b"instance-a"}
    assert set(responses[1].attributes.attr) == {b"instance-b"}


def test_direct_servicer_uses_one_loop_for_concurrent_calls():
    barrier = threading.Barrier(2)

    class Service(service.FlameService):
        def on_session_enter(self, context):
            pass

        def on_task_invoke(self, context):
            barrier.wait(timeout=2)
            return context.task_id.encode()

        def on_session_leave(self):
            pass

    servicer = service.FlameInstanceServicer(Service())
    loop_thread = servicer._loop_thread
    responses = [None, None]

    def invoke(index):
        responses[index] = servicer.OnTaskInvoke(shim_pb2.TaskContext(task_id=str(index), session_id="session"), DummyContext())

    threads = [threading.Thread(target=invoke, args=(index,)) for index in range(2)]
    try:
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=3)
        assert all(not thread.is_alive() for thread in threads)
        assert [response.task_result.output for response in responses] == [b"0", b"1"]
    finally:
        servicer.close()
    assert not loop_thread._thread.is_alive()


def test_servicer_wraps_enter_and_task_publication():
    class PublishingService(service.FlameService):
        def on_session_enter(self, context):
            self.publish({b"entered"})
            self.publish({b"prefix", b"block"})

        def on_task_invoke(self, context):
            if context.task_id == "task-error":
                self.publish({b"error-key"})
                raise RuntimeError("task failed")
            return None

        def on_session_leave(self):
            pass

    servicer = service.FlameInstanceServicer(PublishingService())
    enter_request = shim_pb2.SessionContext(
        session_id="session-1",
        application=shim_pb2.ApplicationContext(name="application-1"),
    )
    task_request = shim_pb2.TaskContext(task_id="task-1", session_id="session-2")

    enter_response = servicer.OnSessionEnter(enter_request, DummyContext())
    assert set(enter_response.attributes.attr) == {b"entered", b"prefix", b"block"}

    servicer.OnSessionLeave(None, DummyContext())
    task_response = servicer.OnTaskInvoke(task_request, DummyContext())
    assert list(task_response.attributes.attr) == []

    next_response = servicer.OnTaskInvoke(task_request, DummyContext())
    assert next_response.HasField("attributes")
    assert list(next_response.attributes.attr) == []

    failed_response = servicer.OnTaskInvoke(
        shim_pb2.TaskContext(task_id="task-error", session_id="session-2"),
        DummyContext(),
    )
    assert failed_response.task_result.return_code == -1
    assert set(failed_response.attributes.attr) == {b"error-key"}


def test_failed_session_enter_does_not_leak_publication():
    class FailFirstEnterService(service.FlameService):
        def on_session_enter(self, context):
            if context.session_id == "failed-session":
                self.publish({b"failed-enter-key"})
                raise RuntimeError("session enter failed")

        def on_task_invoke(self, context):
            return None

        def on_session_leave(self):
            pass

    servicer = service.FlameInstanceServicer(FailFirstEnterService())

    def request(session_id):
        return shim_pb2.SessionContext(
            session_id=session_id,
            application=shim_pb2.ApplicationContext(name="application-1"),
        )

    failed = servicer.OnSessionEnter(request("failed-session"), DummyContext())
    assert failed.result.return_code == -1
    assert not failed.HasField("attributes")

    succeeded = servicer.OnSessionEnter(request("next-session"), DummyContext())
    assert succeeded.result.return_code == 0
    assert list(succeeded.attributes.attr) == []

    empty = servicer.OnSessionEnter(request("empty-session"), DummyContext())
    assert empty.HasField("attributes")
    assert list(empty.attributes.attr) == []


@pytest.mark.parametrize(
    ("attributes", "error"),
    [
        (["not-bytes"], TypeError),
        ([b""], ValueError),
        ([b"x" * 257], ValueError),
        ([index.to_bytes(2, "big") for index in range(1_025)], ValueError),
    ],
)
def test_publish_validates_rfe275_limits(attributes, error):
    with pytest.raises(error):
        _NoopService().publish(attributes)


def test_publish_rejects_cumulative_limit_without_partial_update():
    publisher = _NoopService()
    first = [index.to_bytes(2, "big") for index in range(600)]
    overflow = [index.to_bytes(2, "big") for index in range(600, 1_100)]

    publisher.publish(first)
    with pytest.raises(ValueError):
        publisher.publish(overflow)
    assert len(publisher._take_attributes().attr) == 600
    assert list(publisher._take_attributes().attr) == []


def test_publish_byte_limit_resets_after_take():
    publisher = _NoopService()
    boundary = [index.to_bytes(2, "big") + bytes(254) for index in range(256)]

    publisher.publish(boundary)
    publisher.publish(boundary)
    with pytest.raises(ValueError):
        publisher.publish([b"overflow"])
    assert len(publisher._take_attributes().attr) == 256

    publisher.publish(boundary)
    assert len(publisher._take_attributes().attr) == 256


def test_flame_service_abstract_minimal_implementation():
    class MyService(service.FlameService):
        def __init__(self):
            self.called = {}

        def on_session_enter(self, context: service.SessionContext):
            self.called["enter"] = context
            return True

        def on_task_invoke(self, context: service.TaskContext):
            self.called["invoke"] = context
            return b"OUT"

        def on_session_leave(self):
            self.called["leave"] = True
            return True

    svc = MyService()
    servicer = service.FlameInstanceServicer(svc)

    # Build simple mock request for OnSessionEnter
    class MockAppCtx:
        def __init__(self):
            self.name = "app"
            self.image = "img"
            self.command = "cmd"
            self.working_directory = "/work"
            self.url = "http://url"

        def HasField(self, field):  # noqa: N802
            return field == "image" and self.image is not None

    class MockSessionEnterRequest:
        def __init__(self):
            self.session_id = "sess-123"
            self.application = MockAppCtx()
            self.common_data = b"C"

        def HasField(self, field):  # noqa: N802
            if field == "common_data":
                return self.common_data is not None
            return False

    req = MockSessionEnterRequest()
    resp = servicer.OnSessionEnter(req, DummyContext())
    assert resp.result.return_code == 0
    # Verify service received a SessionContext with the right fields
    assert svc.called["enter"].session_id == "sess-123"

    # OnTaskInvoke path
    class MockTaskRequest:
        def __init__(self):
            self.task_id = "t1"
            self.session_id = "sess-123"
            self.input = b"in"

        def HasField(self, field):  # noqa: N802
            return field == "input" and self.input is not None

    req2 = MockTaskRequest()
    resp2 = servicer.OnTaskInvoke(req2, DummyContext())
    assert resp2.task_result.return_code == 0
    assert resp2.task_result.output == b"OUT"
    assert svc.called["invoke"].task_id == "t1"

    # OnSessionLeave path
    resp3 = servicer.OnSessionLeave(None, DummyContext())
    assert isinstance(resp3, ResultProto)
    assert resp3.return_code == 0


def test_on_session_enter_exception_path_returns_error():  # noqa: N802
    class FailService(service.FlameService):
        def on_session_enter(self, context: service.SessionContext):
            raise RuntimeError("boom")

        def on_task_invoke(self, context: service.TaskContext):
            return b"X"

        def on_session_leave(self):
            return True

    svc = FailService()
    servicer = service.FlameInstanceServicer(svc)

    class MockAppCtx:
        def __init__(self):
            self.name = "app"
            self.image = "img"

        def HasField(self, field):  # noqa: N802
            return field == "image" and self.image is not None

    class MockSessionEnterRequest:
        def __init__(self):
            self.session_id = "sess-1"
            self.application = MockAppCtx()
            self.common_data = None

        def HasField(self, field):  # noqa: N802
            return False

    req = MockSessionEnterRequest()
    resp = servicer.OnSessionEnter(req, DummyContext())
    assert resp.result.return_code == -1


def test_on_task_invoke_exception_path():  # noqa: N802
    class FailService(service.FlameService):
        def on_session_enter(self, context: service.SessionContext):
            return True

        def on_task_invoke(self, context: service.TaskContext):
            raise ValueError("bad task")

        def on_session_leave(self):
            return True

    svc = FailService()
    servicer = service.FlameInstanceServicer(svc)

    class MockTaskRequest:
        def __init__(self):
            self.task_id = "tid"
            self.session_id = "sess"
            self.input = b"in"

        def HasField(self, field):  # noqa: N802
            return field == "input" and self.input is not None

    req = MockTaskRequest()
    resp = servicer.OnTaskInvoke(req, DummyContext())
    assert resp.task_result.return_code == -1
    assert not resp.task_result.HasField("output")


def test_service_preserves_empty_optional_bytes():  # noqa: N802
    class CaptureService(service.FlameService):
        def __init__(self):
            self.session_context = None
            self.task_context = None

        def on_session_enter(self, context: service.SessionContext):
            self.session_context = context
            return True

        def on_task_invoke(self, context: service.TaskContext):
            self.task_context = context
            return None

        def on_session_leave(self):
            return True

    svc = CaptureService()
    servicer = service.FlameInstanceServicer(svc)

    class MockAppCtx:
        name = "app"
        image = None
        command = None
        working_directory = None
        url = None

        def HasField(self, field):  # noqa: N802
            return False

    class MockSessionEnterRequest:
        session_id = "sess"
        application = MockAppCtx()
        common_data = b""

        def HasField(self, field):  # noqa: N802
            return field == "common_data"

    class MockTaskRequest:
        task_id = "task"
        session_id = "sess"
        input = b""

        def HasField(self, field):  # noqa: N802
            return field == "input"

    enter_resp = servicer.OnSessionEnter(MockSessionEnterRequest(), DummyContext())
    invoke_resp = servicer.OnTaskInvoke(MockTaskRequest(), DummyContext())

    assert enter_resp.result.return_code == 0
    assert svc.session_context.common_data() == b""
    assert invoke_resp.task_result.return_code == 0
    assert not invoke_resp.task_result.HasField("output")
    assert svc.task_context.input == b""


def test_flame_instance_server_start_and_stop(monkeypatch, tmp_path):
    from flamepy.core.aio import service as aio_service

    calls = []

    class FakeServer:
        def __init__(self, implementation):
            calls.append(("init", implementation))

        async def start(self):
            calls.append(("start",))

        async def wait_for_termination(self):
            calls.append(("wait",))

        async def stop(self):
            calls.append(("stop",))

    monkeypatch.setattr(aio_service, "FlameInstanceServer", FakeServer)

    class DummyService(service.FlameService):
        def on_session_enter(self, context):
            return True

        def on_task_invoke(self, context):
            return b"OUT"

        def on_session_leave(self):
            return True

    s = service.FlameInstanceServer(DummyService())
    s.start()
    assert [call[0] for call in calls] == ["init", "start", "wait", "stop"]
    s.stop()
    assert [call[0] for call in calls] == ["init", "start", "wait", "stop"]


def test_flame_instance_server_start_without_endpoint_raises():
    # Ensure the environment does not provide the endpoint
    if service.FLAME_INSTANCE_ENDPOINT in os.environ:
        del os.environ[service.FLAME_INSTANCE_ENDPOINT]

    class DummyService(service.FlameService):
        def on_session_enter(self, context):
            return True

        def on_task_invoke(self, context):
            return b""

        def on_session_leave(self):
            return True

    with pytest.raises(Exception):
        service.FlameInstanceServer(DummyService()).start()


# Service Session Tests


class FakeSession:
    def __init__(self):
        self.application = "myapp"
        self.id = "sess-1"

    def invoke(self, input_bytes):
        return cloudpickle.dumps("OK")

    def common_data(self):
        return None

    def close(self):
        pass


def test_session_init_and_invoke(monkeypatch):
    # Patch create_session to return fake session
    monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
    session = service_client.Session(name="myapp")
    # Patch the session to return a known value on invoke
    result = session.invoke("hello")
    assert result == "OK"


def test_cloudpickle_serialization_of_callable():
    def f(x):
        return x * 2

    s = cloudpickle.dumps(f)
    f2 = cloudpickle.loads(s)
    assert f2(3) == 6


class TestSessionInitialization:
    def test_session_requires_name_or_session_id(self):
        with pytest.raises(ValueError, match="Either 'name' or 'session_id' must be provided"):
            service_client.Session()

    def test_session_rejects_both_name_and_session_id(self):
        with pytest.raises(ValueError, match="Cannot provide both"):
            service_client.Session(name="myapp", session_id="sess-1")

    def test_session_with_session_id_opens_existing(self, monkeypatch):
        fake_session = FakeSession()
        monkeypatch.setattr(service_client, "open_session", lambda session_id: fake_session)
        session = service_client.Session(session_id="sess-1")
        assert session._name == "myapp"
        assert session._session is fake_session

    def test_session_with_name_creates_new_session(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        assert session._name == "myapp"
        assert session._session is not None

    def test_session_with_dict_resreq(self, monkeypatch):
        captured_kwargs = {}

        def capture_create_session(**kwargs):
            captured_kwargs.update(kwargs)
            return FakeSession()

        monkeypatch.setattr(service_client, "create_session", capture_create_session)
        service_client.Session(name="myapp", resreq={"cpu": 4, "memory": "8g", "gpu": 1})
        assert captured_kwargs.get("resreq") is not None
        assert captured_kwargs["resreq"].cpu == 4
        assert captured_kwargs["resreq"].gpu == 1


class TestSessionOperations:
    def test_session_id_returns_session_id(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        assert session.id() == "sess-1"

    def test_session_id_returns_none_when_no_session(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        session._session = None
        assert session.id() is None

    def test_session_invoke_raises_when_no_session(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        session._session = None
        with pytest.raises(RuntimeError, match="not initialized"):
            session.invoke("test")

    def test_session_invoke_returns_none_for_none_output(self, monkeypatch):
        class NoneOutputSession(FakeSession):
            def invoke(self, input_bytes):
                return None

        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: NoneOutputSession())
        session = service_client.Session(name="myapp")
        result = session.invoke("test")
        assert result is None

    def test_session_context_returns_none_when_no_session(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        session._session = None
        assert session.context() is None

    def test_session_context_returns_none_when_no_common_data(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        assert session.context() is None


class TestSessionContextManager:
    def test_session_context_manager_enter(self, monkeypatch):
        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: FakeSession())
        session = service_client.Session(name="myapp")
        result = session.__enter__()
        assert result is session

    def test_session_context_manager_exit_closes_session(self, monkeypatch):
        closed = {"called": False}

        class TrackingSession(FakeSession):
            def close(self):
                closed["called"] = True

        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: TrackingSession())
        session = service_client.Session(name="myapp")
        session.__exit__(None, None, None)
        assert closed["called"]

    def test_session_with_statement(self, monkeypatch):
        closed = {"called": False}

        class TrackingSession(FakeSession):
            def close(self):
                closed["called"] = True

        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: TrackingSession())
        with service_client.Session(name="myapp") as session:
            assert session._session is not None
        assert closed["called"]

    def test_session_close_is_idempotent(self, monkeypatch):
        close_count = {"count": 0}

        class CountingSession(FakeSession):
            def close(self):
                close_count["count"] += 1

        monkeypatch.setattr(service_client, "create_session", lambda **kwargs: CountingSession())
        session = service_client.Session(name="myapp")
        session.close()
        session.close()
        assert close_count["count"] == 1


# Service Instance Tests


class DummyObjectRef:
    """Dummy ObjectRef for testing."""

    def __init__(self, data=b"test"):
        self._data = data

    @classmethod
    def decode(cls, data: bytes) -> "DummyObjectRef":
        return cls(data)

    def encode(self) -> bytes:
        return self._data


@pytest.fixture
def flame_instance():
    """Create a fresh FlameInstance for testing."""
    return FlameInstance()


def test_flameinstance_init(flame_instance):
    """Test FlameInstance initializes with correct defaults."""
    assert flame_instance._entrypoint is None
    assert flame_instance._parameter is None
    assert flame_instance._object_ref is None


def test_entrypoint_decorator_registers_function(flame_instance):
    """Test that entrypoint decorator registers the function."""

    @flame_instance.entrypoint
    def my_handler(data):
        return data

    assert flame_instance._entrypoint is my_handler
    assert flame_instance._parameter is not None
    assert flame_instance._parameter.name == "data"


def test_entrypoint_decorator_zero_params(flame_instance):
    """Test entrypoint decorator with zero-parameter function."""

    @flame_instance.entrypoint
    def no_params():
        return "done"

    assert flame_instance._entrypoint is no_params
    assert flame_instance._parameter is None


def test_entrypoint_decorator_rejects_multiple_params():
    """Test entrypoint decorator rejects functions with multiple params."""
    fi = FlameInstance()

    with pytest.raises(AssertionError):

        @fi.entrypoint
        def bad_handler(a, b, c):
            pass


def test_on_session_enter_decodes_object_ref(flame_instance, monkeypatch):
    """Test on_session_enter decodes ObjectRef from common_data."""
    dummy_ref = DummyObjectRef(b"session-data")

    monkeypatch.setattr(
        "flamepy.service.instance.ObjectRef",
        types.SimpleNamespace(decode=lambda data: dummy_ref),
    )

    app_ctx = ApplicationContext(name="test-app")
    session_ctx = SessionContext(
        _common_data=b"encoded-ref",
        session_id="sess-1",
        application=app_ctx,
    )

    flame_instance.on_session_enter(session_ctx)
    assert flame_instance._object_ref is dummy_ref


def test_on_session_enter_handles_none_common_data(flame_instance):
    """Test on_session_enter handles None common_data."""
    app_ctx = ApplicationContext(name="test-app")
    session_ctx = SessionContext(
        _common_data=None,
        session_id="sess-1",
        application=app_ctx,
    )

    flame_instance.on_session_enter(session_ctx)
    assert flame_instance._object_ref is None


def test_on_task_invoke_calls_entrypoint(flame_instance, monkeypatch):
    """Test on_task_invoke calls registered entrypoint with deserialized input."""
    received_input = []

    @flame_instance.entrypoint
    def handler(data):
        received_input.append(data)
        return {"result": "ok"}

    monkeypatch.setattr(
        "flamepy.service.instance.cloudpickle",
        types.SimpleNamespace(
            loads=lambda x: {"key": "value"},
            dumps=cloudpickle.dumps,
            DEFAULT_PROTOCOL=cloudpickle.DEFAULT_PROTOCOL,
        ),
    )

    task_ctx = TaskContext(
        task_id="task-1",
        session_id="sess-1",
        input=b"serialized-input",
    )

    result = flame_instance.on_task_invoke(task_ctx)

    assert len(received_input) == 1
    assert received_input[0] == {"key": "value"}
    assert isinstance(result, TaskOutput)


def test_on_task_invoke_with_none_input(flame_instance, monkeypatch):
    """Test on_task_invoke with None input."""
    received_input = []

    @flame_instance.entrypoint
    def handler(data):
        received_input.append(data)
        return None

    task_ctx = TaskContext(
        task_id="task-1",
        session_id="sess-1",
        input=None,
    )

    flame_instance.on_task_invoke(task_ctx)

    assert len(received_input) == 1
    assert received_input[0] is None


def test_on_task_invoke_without_entrypoint(flame_instance):
    """Test on_task_invoke returns None when no entrypoint is registered."""
    task_ctx = TaskContext(
        task_id="task-1",
        session_id="sess-1",
        input=b"data",
    )

    result = flame_instance.on_task_invoke(task_ctx)
    assert result is None


def test_on_task_invoke_with_zero_param_entrypoint(flame_instance, monkeypatch):
    """Test on_task_invoke with zero-parameter entrypoint."""

    @flame_instance.entrypoint
    def no_params():
        return "done"

    monkeypatch.setattr(
        "flamepy.service.instance.cloudpickle",
        types.SimpleNamespace(
            loads=lambda x: "ignored",
            dumps=cloudpickle.dumps,
            DEFAULT_PROTOCOL=cloudpickle.DEFAULT_PROTOCOL,
        ),
    )

    task_ctx = TaskContext(
        task_id="task-1",
        session_id="sess-1",
        input=b"ignored",
    )

    result = flame_instance.on_task_invoke(task_ctx)
    assert isinstance(result, TaskOutput)


def test_on_session_leave_clears_object_ref(flame_instance):
    """Test on_session_leave clears the object reference."""
    flame_instance._object_ref = DummyObjectRef()

    flame_instance.on_session_leave()

    assert flame_instance._object_ref is None


def test_context_returns_deserialized_data(flame_instance, monkeypatch):
    """Test context() returns deserialized data from cache."""
    flame_instance._object_ref = DummyObjectRef()

    monkeypatch.setattr(
        "flamepy.service.instance.get_object",
        lambda ref: b"serialized-ctx",
    )
    monkeypatch.setattr(
        "flamepy.service.instance.cloudpickle",
        types.SimpleNamespace(loads=lambda x: {"ctx_key": "ctx_value"}),
    )

    result = flame_instance.context()
    assert result == {"ctx_key": "ctx_value"}


def test_context_returns_none_when_no_ref(flame_instance):
    """Test context() returns None when no object_ref."""
    flame_instance._object_ref = None

    result = flame_instance.context()
    assert result is None


def test_update_context_serializes_and_updates(flame_instance, monkeypatch):
    """Test update_context() serializes data and updates cache."""
    flame_instance._object_ref = DummyObjectRef()
    updated_refs = []

    def mock_update(ref, data):
        updated_refs.append((ref, data))
        return DummyObjectRef(data)

    monkeypatch.setattr("flamepy.service.instance.update_object", mock_update)
    monkeypatch.setattr(
        "flamepy.service.instance.cloudpickle",
        types.SimpleNamespace(
            dumps=lambda x, protocol=None: b"serialized:" + str(x).encode(),
            DEFAULT_PROTOCOL=4,
        ),
    )

    flame_instance.update_context({"new": "data"})

    assert len(updated_refs) == 1
    assert updated_refs[0][1] == b"serialized:{'new': 'data'}"


def test_update_context_noop_when_no_ref(flame_instance, monkeypatch):
    """Test update_context() does nothing when no object_ref."""
    flame_instance._object_ref = None
    called = []

    monkeypatch.setattr(
        "flamepy.service.instance.update_object",
        lambda ref, data: called.append(True),
    )

    flame_instance.update_context({"data": 1})

    assert len(called) == 0

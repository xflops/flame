"""Tests for flamepy service APIs."""

import gc
import logging
import os
import types

import cloudpickle
import pytest

import flamepy.core.service as service
import flamepy.service.client as service_client
from flamepy.core.service import ApplicationContext, SessionContext, TaskContext
from flamepy.core.types import TaskOutput
from flamepy.proto.types_pb2 import Result as ResultProto
from flamepy.proto.types_pb2 import TaskResult as TaskResultProto
from flamepy.service.instance import FlameInstance

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
    assert isinstance(resp, ResultProto)
    assert resp.return_code == 0
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
    assert isinstance(resp2, TaskResultProto)
    assert resp2.return_code == 0
    assert resp2.output == b"OUT"
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
    assert resp.return_code == -1


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
    assert resp.return_code == -1
    assert not resp.HasField("output")


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

    assert enter_resp.return_code == 0
    assert svc.session_context.common_data() == b""
    assert invoke_resp.return_code == 0
    assert not invoke_resp.HasField("output")
    assert svc.task_context.input == b""


def test_flame_instance_server_start_and_stop(monkeypatch, tmp_path):
    # Fake grpc server and helper to intercept calls
    started = {"start": False, "stop": False}

    class FakeServer:
        def __init__(self, *args, **kwargs):
            self._stopped = False

        def add_insecure_port(self, addr):
            # Accept the unix socket address; just store for verification
            self._port = addr

        def start(self):
            started["start"] = True

        def wait_for_termination(self):
            # Immediately return to avoid blocking
            return None

        def stop(self, grace=None):
            started["stop"] = True
            self._stopped = True

    fake_grpc = type("fake_grpc", (), {})()
    fake_grpc.server = lambda executor=None: FakeServer()

    # Patch grpc in the service module
    monkeypatch.setattr(service, "grpc", fake_grpc)
    # Patch add_InstanceServicer_to_server to a no-op
    called = {"added": None}
    monkeypatch.setattr(service, "add_InstanceServicer_to_server", lambda servicer, srv: called.__setitem__("added", (servicer, srv)))

    # Ensure endpoint is set
    os.environ[service.FLAME_INSTANCE_ENDPOINT] = "/tmp/flame.sock"

    class DummyService(service.FlameService):
        def on_session_enter(self, context):
            return True

        def on_task_invoke(self, context):
            return b"OUT"

        def on_session_leave(self):
            return True

    s = service.FlameInstanceServer(DummyService())
    s.start()
    # Verify server was started and added to server
    assert started["start"] is True
    # Stop should call server.stop
    s.stop()
    assert started["stop"] is True


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

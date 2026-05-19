"""Tests for flamepy core client and types."""

import json
import time
from datetime import datetime, timezone

import pytest

import flamepy
import flamepy.core.client as client
from flamepy import core as flamepy_core
from flamepy.core.types import (
    Application,
    ApplicationAttributes,
    ApplicationSchema,
    ApplicationState,
    Event,
    FlameContext,
    FlameError,
    FlameErrorCode,
    ResourceRequirement,
    SessionAttributes,
    SessionState,
    Shim,
    Task,
    TaskState,
    short_name,
)

# Client Tests


class DummyChannel:
    def __init__(self, location):
        self.location = location

    def close(self):
        pass


class DummyFrontend:
    def __init__(self):
        pass


def test_connection_connect_http(monkeypatch):
    import grpc

    monkeypatch.setattr(grpc, "insecure_channel", lambda loc: DummyChannel(loc))

    class DummyFuture:
        def result(self, timeout=None):
            return None

    monkeypatch.setattr(grpc, "channel_ready_future", lambda ch: DummyFuture())
    monkeypatch.setattr(grpc, "secure_channel", lambda loc, creds=None: DummyChannel(loc))
    monkeypatch.setattr(grpc, "ssl_channel_credentials", lambda root_certificates=None: b"certs")
    monkeypatch.setattr("flamepy.core.client.FrontendStub", lambda channel: DummyFrontend())

    conn = client.Connection.connect("http://localhost:1234")
    assert isinstance(conn, client.Connection)
    conn.close()


def test_connection_connect_https_with_tls(monkeypatch, tmp_path):
    import grpc

    monkeypatch.setattr(grpc, "insecure_channel", lambda loc: DummyChannel(loc))

    class DummyFuture:
        def result(self, timeout=None):
            return None

    monkeypatch.setattr(grpc, "channel_ready_future", lambda ch: DummyFuture())
    monkeypatch.setattr(grpc, "secure_channel", lambda loc, creds=None: DummyChannel(loc))
    called = {"ok": False}

    def fake_ssl_credentials(*args, **kwargs):
        called["ok"] = True
        return b"certs"

    monkeypatch.setattr(grpc, "ssl_channel_credentials", fake_ssl_credentials)
    monkeypatch.setattr("flamepy.core.client.FrontendStub", lambda channel: DummyFrontend())

    tls = client.FlameClientTls(ca_file=str(tmp_path / "ca.pem"))
    (tmp_path / "ca.pem").write_text("CERT")
    tls.ca_file = str(tmp_path / "ca.pem")
    conn = client.Connection.connect("https://localhost:1234", tls_config=tls)
    assert isinstance(conn, client.Connection)
    assert called["ok"]
    conn.close()


def test_session_create_task_with_mocked_frontend(monkeypatch):

    class DummyFrontend:
        def CreateTask(self, req):  # noqa: N802
            class StatusMock:
                state = 0
                creation_time = int(time.time() * 1000)
                completion_time = int(time.time() * 1000)
                events = []

                def HasField(self, name):  # noqa: N802
                    return name == "completion_time"

            class Resp:
                metadata = type("M", (), {"id": "tid-1"})
                status = StatusMock()

            return Resp()

    class DummyConnection:
        def __init__(self):
            self._frontend = DummyFrontend()
            import concurrent.futures

            self._executor = concurrent.futures.ThreadPoolExecutor(max_workers=2)

        def close(self):
            pass

    fake_conn = DummyConnection()
    from flamepy.core.client import Session, SessionState

    s = Session(connection=fake_conn, id="sess-1", application="app", state=SessionState.OPEN, creation_time=datetime.now(timezone.utc), pending=0, running=0, succeed=0, failed=0, completion_time=None)

    t = s.create_task(b"input")
    assert t.session_id == s.id
    assert t.id is not None


class TestConnectionValidation:
    def test_connection_rejects_empty_address(self):
        with pytest.raises(FlameError) as exc_info:
            client.Connection.connect("")
        assert exc_info.value.code == FlameErrorCode.INVALID_CONFIG

    def test_connection_handles_timeout(self, monkeypatch):
        import grpc

        monkeypatch.setattr(grpc, "insecure_channel", lambda loc: DummyChannel(loc))

        class TimeoutFuture:
            def result(self, timeout=None):
                raise grpc.FutureTimeoutError()

        monkeypatch.setattr(grpc, "channel_ready_future", lambda ch: TimeoutFuture())

        with pytest.raises(FlameError) as exc_info:
            client.Connection.connect("http://localhost:1234")
        assert "timeout" in str(exc_info.value).lower()


class TestSessionOperations:
    def create_test_session(self, connection=None):
        from flamepy.core.client import Session, SessionState

        if connection is None:
            connection = type("Conn", (), {"_frontend": DummyFrontend(), "_executor": None, "close": lambda self: None})()

        return Session(
            connection=connection,
            id="sess-test",
            application="test-app",
            state=SessionState.OPEN,
            creation_time=datetime.now(timezone.utc),
            pending=0,
            running=0,
            succeed=0,
            failed=0,
            completion_time=None,
        )

    def test_session_common_data_returns_none_by_default(self):
        session = self.create_test_session()
        assert session.common_data() is None

    def test_session_common_data_returns_bytes(self):
        from flamepy.core.client import Session, SessionState

        connection = type("Conn", (), {"_frontend": DummyFrontend(), "_executor": None, "close": lambda self: None})()
        session = Session(
            connection=connection,
            id="sess-test",
            application="test-app",
            state=SessionState.OPEN,
            creation_time=datetime.now(timezone.utc),
            pending=0,
            running=0,
            succeed=0,
            failed=0,
            completion_time=None,
            common_data=b"test-data",
        )
        assert session.common_data() == b"test-data"

    def test_get_session_preserves_events(self):
        from flamepy.proto.types_pb2 import Event as EventProto
        from flamepy.proto.types_pb2 import Metadata, SessionSpec, SessionStatus
        from flamepy.proto.types_pb2 import Session as SessionProto

        event_time = int(time.time() * 1000)

        class DummyFrontendWithSession:
            def GetSession(self, req):  # noqa: N802
                return SessionProto(
                    metadata=Metadata(id=req.session_id),
                    spec=SessionSpec(application="test-app"),
                    status=SessionStatus(
                        state=SessionState.OPEN,
                        creation_time=event_time,
                        events=[
                            EventProto(
                                code=1001,
                                message="failed to bind session",
                                creation_time=event_time,
                            )
                        ],
                    ),
                )

        connection = client.Connection("http://localhost:1234", DummyChannel("http://localhost:1234"), DummyFrontendWithSession())
        try:
            session = connection.get_session("sess-events")
        finally:
            connection.close()

        assert len(session.events) == 1
        assert session.events[0].code == 1001
        assert session.events[0].message == "failed to bind session"

    def test_session_get_task_preserves_empty_optional_bytes(self):
        from flamepy.core.client import Session, SessionState
        from flamepy.proto.types_pb2 import Metadata, Task, TaskSpec, TaskStatus

        class DummyFrontendWithTask:
            def GetTask(self, req):  # noqa: N802
                task = Task(
                    metadata=Metadata(id="task-1"),
                    spec=TaskSpec(session_id=req.session_id, input=b"", output=b""),
                    status=TaskStatus(state=2, creation_time=int(time.time() * 1000)),
                )
                return task

        connection = type("Conn", (), {"_frontend": DummyFrontendWithTask(), "_executor": None, "close": lambda self: None})()
        session = Session(
            connection=connection,
            id="sess-test",
            application="test-app",
            state=SessionState.OPEN,
            creation_time=datetime.now(timezone.utc),
            pending=0,
            running=0,
            succeed=0,
            failed=0,
            completion_time=None,
        )

        task = session.get_task("task-1")

        assert task.input == b""
        assert task.output == b""

    def test_session_create_task_rejects_non_bytes(self):
        session = self.create_test_session()
        with pytest.raises(FlameError) as exc_info:
            session.create_task("not bytes")
        assert exc_info.value.code == FlameErrorCode.INVALID_ARGUMENT


class TestGrpcErrorMapping:
    def test_not_found_error_mapping(self):
        import grpc

        class FakeRpcError(grpc.RpcError):
            def code(self):
                return grpc.StatusCode.NOT_FOUND

            def details(self):
                return "Resource not found"

        error = client.Connection._grpc_error_to_flame_error(FakeRpcError(), "test operation")
        assert error.code == FlameErrorCode.NOT_FOUND

    def test_already_exists_error_mapping(self):
        import grpc

        class FakeRpcError(grpc.RpcError):
            def code(self):
                return grpc.StatusCode.ALREADY_EXISTS

            def details(self):
                return "Already exists"

        error = client.Connection._grpc_error_to_flame_error(FakeRpcError(), "test operation")
        assert error.code == FlameErrorCode.ALREADY_EXISTS

    def test_invalid_argument_error_mapping(self):
        import grpc

        class FakeRpcError(grpc.RpcError):
            def code(self):
                return grpc.StatusCode.INVALID_ARGUMENT

            def details(self):
                return "Invalid argument"

        error = client.Connection._grpc_error_to_flame_error(FakeRpcError(), "test operation")
        assert error.code == FlameErrorCode.INVALID_ARGUMENT

    def test_failed_precondition_error_mapping(self):
        import grpc

        class FakeRpcError(grpc.RpcError):
            def code(self):
                return grpc.StatusCode.FAILED_PRECONDITION

            def details(self):
                return "Precondition failed"

        error = client.Connection._grpc_error_to_flame_error(FakeRpcError(), "test operation")
        assert error.code == FlameErrorCode.INVALID_STATE

    def test_unknown_error_mapping(self):
        import grpc

        class FakeRpcError(grpc.RpcError):
            def code(self):
                return grpc.StatusCode.UNKNOWN

            def details(self):
                return "Unknown error"

        error = client.Connection._grpc_error_to_flame_error(FakeRpcError(), "test operation")
        assert error.code == FlameErrorCode.INTERNAL


class TestApplicationConversion:
    def test_register_application_raises_on_failed_result(self):
        from flamepy.proto.types_pb2 import Result as ResultProto

        class Frontend:
            def RegisterApplication(self, req):  # noqa: N802
                return ResultProto(return_code=-1, message="registration rejected")

        conn = client.Connection("http://unused", DummyChannel("unused"), Frontend())
        try:
            with pytest.raises(FlameError, match="registration rejected"):
                conn.register_application("app", ApplicationAttributes())
        finally:
            conn.close()

    def test_unregister_application_raises_on_failed_result(self):
        from flamepy.proto.types_pb2 import Result as ResultProto

        class Frontend:
            def UnregisterApplication(self, req):  # noqa: N802
                return ResultProto(return_code=-1, message="unregistration rejected")

        conn = client.Connection("http://unused", DummyChannel("unused"), Frontend())
        try:
            with pytest.raises(FlameError, match="unregistration rejected"):
                conn.unregister_application("app")
        finally:
            conn.close()

    def test_list_applications_preserves_absent_optional_fields(self):
        from flamepy.proto.types_pb2 import Application as ApplicationProto
        from flamepy.proto.types_pb2 import ApplicationList, ApplicationStatus, Metadata

        class Frontend:
            def ListApplication(self, req):  # noqa: N802
                app = ApplicationProto(
                    metadata=Metadata(id="app-1", name="app"),
                    status=ApplicationStatus(state=0, creation_time=int(time.time() * 1000)),
                )
                return ApplicationList(applications=[app])

        conn = client.Connection("http://unused", DummyChannel("unused"), Frontend())
        try:
            apps = conn.list_applications()
        finally:
            conn.close()

        assert len(apps) == 1
        app = apps[0]
        assert app.image is None
        assert app.command is None
        assert app.working_directory is None
        assert app.max_instances is None
        assert app.delay_release is None
        assert app.schema is None
        assert app.url is None
        assert app.installer is None

    def test_get_application_preserves_present_empty_optional_fields(self):
        from flamepy.proto.types_pb2 import Application as ApplicationProto
        from flamepy.proto.types_pb2 import ApplicationSchema, ApplicationStatus, Metadata

        class Frontend:
            def GetApplication(self, req):  # noqa: N802
                app = ApplicationProto(
                    metadata=Metadata(id="app-1", name=req.name),
                    status=ApplicationStatus(state=0, creation_time=int(time.time() * 1000)),
                )
                app.spec.image = ""
                app.spec.schema.CopyFrom(ApplicationSchema(input=""))
                return app

        conn = client.Connection("http://unused", DummyChannel("unused"), Frontend())
        try:
            app = conn.get_application("app")
        finally:
            conn.close()

        assert app.image == ""
        assert app.command is None
        assert app.schema is not None
        assert app.schema.input == ""
        assert app.schema.output is None


class TestTaskWatcher:
    def test_task_watcher_iteration(self):
        from flamepy.core.client import TaskWatcher

        class FakeStream:
            def __init__(self):
                self.items = []
                self.index = 0

            def __next__(self):
                if self.index >= len(self.items):
                    raise StopIteration
                item = self.items[self.index]
                self.index += 1
                return item

        stream = FakeStream()
        watcher = TaskWatcher(stream)
        assert iter(watcher) is watcher

    def test_task_watcher_timeout_check(self):
        from flamepy.core.client import TaskWatcher

        class EmptyStream:
            def __next__(self):
                raise StopIteration

        watcher = TaskWatcher(EmptyStream(), timeout=0.001)
        import time as time_module

        time_module.sleep(0.01)
        with pytest.raises(TimeoutError):
            next(watcher)


class TestTaskIterator:
    def test_task_iterator_is_iterable(self):
        from flamepy.core.client import TaskIterator

        class FakeStream:
            def __next__(self):
                raise StopIteration

        iterator = TaskIterator(FakeStream(), "sess-1")
        assert iter(iterator) is iterator


# Type Tests


def test_enums_and_flame_error():
    # Enums should be int-like and have expected values
    assert int(SessionState.OPEN) == 0
    assert int(TaskState.PENDING) == 0
    assert int(ApplicationState.ENABLED) == 0
    assert int(Shim.HOST) == 0
    assert int(FlameErrorCode.INVALID_ARGUMENT) == 2

    # FlameError
    err = FlameError(FlameErrorCode.INVALID_ARGUMENT, "bad arg")
    assert err.code == FlameErrorCode.INVALID_ARGUMENT
    assert "bad arg" in str(err)


def test_dataclass_defaults_and_instantiation():
    t = Event(code=1)
    sa = SessionAttributes(application="app")
    ap_schema = ApplicationSchema()
    ap_attrs = ApplicationAttributes()
    dt = datetime.now(timezone.utc)
    task = Task(id="tid", session_id="sid", state=TaskState.PENDING, creation_time=dt)
    app = Application(id="aid", name="n", state=ApplicationState.ENABLED, creation_time=dt)
    assert t.code == 1
    assert sa.application == "app"
    assert ap_schema.input is None
    assert ap_attrs.image is None
    assert task.input is None
    assert app.name == "n"


def test_resource_requirement_defaults():
    """Test ResourceRequirement with default values."""
    rr = ResourceRequirement()
    assert rr.cpu == 0
    assert rr.memory == 0
    assert rr.gpu == 0


def test_resource_requirement_public_exports():
    """ResourceRequirement should be available from documented SDK entrypoints."""
    assert flamepy.ResourceRequirement is ResourceRequirement
    assert flamepy_core.ResourceRequirement is ResourceRequirement


def test_resource_requirement_explicit_values():
    """Test ResourceRequirement with explicit values."""
    rr = ResourceRequirement(cpu=4, memory=8 * 1024**3, gpu=2)
    assert rr.cpu == 4
    assert rr.memory == 8 * 1024**3
    assert rr.gpu == 2


def test_resource_requirement_from_string_full():
    """Test parsing resource requirements from full string."""
    rr = ResourceRequirement.from_string("cpu=4,mem=16g,gpu=2")
    assert rr.cpu == 4
    assert rr.memory == 16 * 1024**3
    assert rr.gpu == 2


def test_resource_requirement_from_string_partial():
    """Test parsing resource requirements with only some fields."""
    rr = ResourceRequirement.from_string("cpu=8")
    assert rr.cpu == 8
    assert rr.memory == 0
    assert rr.gpu == 0


def test_resource_requirement_from_string_memory_variants():
    """Test parsing different memory unit formats."""
    # Kilobytes
    rr_k = ResourceRequirement.from_string("memory=1024k")
    assert rr_k.memory == 1024 * 1024

    # Megabytes
    rr_m = ResourceRequirement.from_string("mem=512m")
    assert rr_m.memory == 512 * 1024**2

    # Gigabytes
    rr_g = ResourceRequirement.from_string("mem=8g")
    assert rr_g.memory == 8 * 1024**3

    # Binary suffixes printed by flmctl
    rr_gi = ResourceRequirement.from_string("mem=8Gi")
    assert rr_gi.memory == 8 * 1024**3
    rr_tb = ResourceRequirement.from_string("mem=2TB")
    assert rr_tb.memory == 2 * 1024**4
    rr_ti = ResourceRequirement.from_string("mem=2Ti")
    assert rr_ti.memory == 2 * 1024**4
    rr_pb = ResourceRequirement.from_string("mem=1PB")
    assert rr_pb.memory == 1024**5
    rr_pi = ResourceRequirement.from_string("mem=1Pi")
    assert rr_pi.memory == 1024**5

    # Plain bytes
    rr_bytes = ResourceRequirement.from_string("memory=1048576")
    assert rr_bytes.memory == 1048576


def test_resource_requirement_from_string_with_spaces():
    """Test parsing with extra whitespace."""
    rr = ResourceRequirement.from_string("  cpu = 2 , mem = 4g , gpu = 1 ")
    assert rr.cpu == 2
    assert rr.memory == 4 * 1024**3
    assert rr.gpu == 1


@pytest.mark.parametrize("value", ["cpu=abc", "mem=bogus", "gpu=", "foo=1", "cpu=1,"])
def test_resource_requirement_from_string_rejects_malformed_input(value):
    """Malformed resource requirements should not silently become zeroes."""
    with pytest.raises(ValueError):
        ResourceRequirement.from_string(value)


def test_resource_requirement_parse_memory_empty():
    """Test _parse_memory with empty string."""
    assert ResourceRequirement._parse_memory("") == 0
    assert ResourceRequirement._parse_memory("  ") == 0


def test_session_attributes_with_resreq():
    """Test SessionAttributes with resource requirements."""
    rr = ResourceRequirement(cpu=4, memory=8 * 1024**3, gpu=1)
    sa = SessionAttributes(application="test-app", resreq=rr)
    assert sa.application == "test-app"
    assert sa.resreq is not None
    assert sa.resreq.cpu == 4
    assert sa.resreq.memory == 8 * 1024**3
    assert sa.resreq.gpu == 1


def test_session_attributes_without_resreq():
    """Test SessionAttributes without resource requirements.

    With slots fully removed, an unset `resreq` is the supported way to defer
    to the server-side cluster default / hardcoded fallback.
    """
    sa = SessionAttributes(application="test-app")
    assert sa.application == "test-app"
    assert sa.resreq is None


def test_application_attributes_with_installer():
    """Test ApplicationAttributes with installer field."""
    attrs = ApplicationAttributes(
        image="my-image:latest",
        command="/usr/bin/app",
        installer="pip install mypackage",
    )
    assert attrs.image == "my-image:latest"
    assert attrs.command == "/usr/bin/app"
    assert attrs.installer == "pip install mypackage"


def test_application_with_installer():
    """Test Application dataclass with installer field."""
    dt = datetime.now(timezone.utc)
    app = Application(
        id="app-1",
        name="test-app",
        state=ApplicationState.ENABLED,
        creation_time=dt,
        installer="curl -sSL https://install.sh | bash",
    )
    assert app.installer == "curl -sSL https://install.sh | bash"


def test_short_name_generation():
    s1 = short_name("foo", length=8)
    s2 = short_name("bar", length=8)
    assert s1.startswith("foo-")
    assert s2.startswith("bar-")
    assert len(s1) >= len("foo-") + 8
    assert len(s2) >= len("bar-") + 8


def test_flame_context_env_overrides(tmp_path, monkeypatch):
    # Build a fake flame.yaml in a temp home and override with env vars
    fake_home = tmp_path / ".home"
    fake_home.mkdir()
    # Monkeypatch Path.home() via env var in FlameContext by setting HOME to tmp
    monkeypatch.setenv("HOME", str(fake_home))

    flame_yaml = {
        "current-context": "flame",
        "contexts": [
            {
                "name": "flame",
                "cluster": {"endpoint": "http://localhost:8080"},
            }
        ],
    }
    conf_dir = fake_home / ".flame"
    conf_dir.mkdir()
    (conf_dir / "flame.yaml").write_text(json.dumps(flame_yaml))

    # No env override: endpoint should come from config
    ctx = FlameContext()
    assert ctx.endpoint == "http://localhost:8080"

    # Override with FLAME_ENDPOINT
    monkeypatch.setenv("FLAME_ENDPOINT", "http://override:1234")
    ctx2 = FlameContext()
    assert ctx2.endpoint == "http://override:1234"

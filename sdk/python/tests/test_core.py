"""Tests for flamepy core client and types."""

import asyncio
import json
import threading
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
            def ListApplications(self, req):  # noqa: N802
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
    assert ctx.app == "flmrun"

    # Override with FLAME_ENDPOINT
    monkeypatch.setenv("FLAME_ENDPOINT", "http://override:1234")
    ctx2 = FlameContext()
    assert ctx2.endpoint == "http://override:1234"


def test_flame_context_reads_scalar_app_template(tmp_path, monkeypatch):
    fake_home = tmp_path / ".home"
    conf_dir = fake_home / ".flame"
    conf_dir.mkdir(parents=True)
    monkeypatch.setenv("HOME", str(fake_home))
    (conf_dir / "flame.yaml").write_text(
        json.dumps(
            {
                "current-context": "flame",
                "contexts": [{"name": "flame", "app": "custom-flmrun"}],
            }
        )
    )

    assert FlameContext().app == "custom-flmrun"


def _task_watch_test_session(watch_frontend):
    from flamepy.proto.types_pb2 import Metadata, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Task as TaskProto

    class Frontend:
        next_id = 0

        def CreateTask(self, request):  # noqa: N802
            self.next_id += 1
            return TaskProto(
                metadata=Metadata(id=str(self.next_id)),
                spec=TaskSpec(session_id=request.task.session_id),
                status=TaskStatus(state=0, creation_time=0),
            )

    conn = client.Connection("http://unused", DummyChannel("unused"), Frontend())
    conn._watch_frontend = watch_frontend
    session = client.Session(
        connection=conn,
        id="session-1",
        application="test-app",
        state=SessionState.OPEN,
        creation_time=datetime.now(timezone.utc),
        pending=0,
        running=0,
        succeed=0,
        failed=0,
        completion_time=None,
    )
    return conn, session


def test_run_fails_when_watch_stream_ends_before_terminal_update():
    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            if False:
                yield requests

    conn, session = _task_watch_test_session(WatchFrontend())
    try:
        future = session.run(b"input")
        with pytest.raises(FlameError, match="watch stream ended before session session-1 tasks completed"):
            future.result(timeout=2)
    finally:
        conn.close()


def test_run_fails_when_task_is_cancelled():
    from flamepy.proto.types_pb2 import Metadata, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Task as TaskProto

    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id),
                    status=TaskStatus(state=4, creation_time=0),
                )

    conn, session = _task_watch_test_session(WatchFrontend())
    try:
        with pytest.raises(FlameError, match="Task was cancelled"):
            session.run(b"input").result(timeout=2)
    finally:
        conn.close()


def test_close_session_waits_for_pending_task_cancellation():
    from flamepy.proto.types_pb2 import Metadata, SessionSpec, SessionStatus, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Session as SessionProto
    from flamepy.proto.types_pb2 import Task as TaskProto

    closed = threading.Event()
    watching = threading.Event()

    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                watching.set()
                await asyncio.to_thread(closed.wait)
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id),
                    status=TaskStatus(state=TaskState.CANCELLED, creation_time=0),
                )

    conn, session = _task_watch_test_session(WatchFrontend())

    def close_session(request):
        closed.set()
        return SessionProto(
            metadata=Metadata(id=request.session_id),
            spec=SessionSpec(application="test-app"),
            status=SessionStatus(state=SessionState.CLOSED, creation_time=0),
        )

    conn._frontend.CloseSession = close_session
    try:
        future = session.run(b"input")
        assert watching.wait(timeout=2)
        session.close()
        with pytest.raises(FlameError, match="Task was cancelled"):
            future.result(timeout=2)
        deadline = time.monotonic() + 2
        while session.id in conn._session_watches and time.monotonic() < deadline:
            time.sleep(0.001)
        assert session.id not in conn._session_watches
    finally:
        closed.set()
        conn.close()


def test_close_session_waits_for_task_watch_registration_in_flight():
    from flamepy.proto.types_pb2 import Metadata, SessionSpec, SessionStatus, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Session as SessionProto
    from flamepy.proto.types_pb2 import Task as TaskProto

    creating = threading.Event()
    release_create = threading.Event()

    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id),
                    status=TaskStatus(state=TaskState.CANCELLED, creation_time=0),
                )

    conn, session = _task_watch_test_session(WatchFrontend())

    def create_task(request):
        creating.set()
        assert release_create.wait(timeout=2)
        return TaskProto(
            metadata=Metadata(id="1"),
            spec=TaskSpec(session_id=request.task.session_id),
            status=TaskStatus(state=TaskState.PENDING, creation_time=0),
        )

    def close_session(request):
        return SessionProto(
            metadata=Metadata(id=request.session_id),
            spec=SessionSpec(application="test-app"),
            status=SessionStatus(state=SessionState.CLOSED, creation_time=0),
        )

    conn._frontend.CreateTask = create_task
    conn._frontend.CloseSession = close_session
    submitted = []
    worker = threading.Thread(target=lambda: submitted.append(session.run(b"input")))
    try:
        worker.start()
        assert creating.wait(timeout=2)
        session.close()
        release_create.set()
        worker.join(timeout=2)
        assert not worker.is_alive()
        with pytest.raises(FlameError, match="Task was cancelled"):
            submitted[0].result(timeout=2)
    finally:
        release_create.set()
        worker.join(timeout=2)
        conn.close()


def test_close_session_keeps_watch_open_when_another_task_is_being_created():
    from flamepy.proto.types_pb2 import Metadata, SessionSpec, SessionStatus, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Session as SessionProto
    from flamepy.proto.types_pb2 import Task as TaskProto

    closed = threading.Event()
    watching_first = threading.Event()
    creating_second = threading.Event()
    release_second = threading.Event()

    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                if request.task_id == "1":
                    watching_first.set()
                    await asyncio.to_thread(closed.wait)
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id),
                    status=TaskStatus(state=TaskState.CANCELLED, creation_time=0),
                )

    conn, session = _task_watch_test_session(WatchFrontend())
    next_id = 0

    def create_task(request):
        nonlocal next_id
        next_id += 1
        if next_id == 2:
            creating_second.set()
            assert release_second.wait(timeout=2)
        return TaskProto(
            metadata=Metadata(id=str(next_id)),
            spec=TaskSpec(session_id=request.task.session_id),
            status=TaskStatus(state=TaskState.PENDING, creation_time=0),
        )

    def close_session(request):
        closed.set()
        return SessionProto(
            metadata=Metadata(id=request.session_id),
            spec=SessionSpec(application="test-app"),
            status=SessionStatus(state=SessionState.CLOSED, creation_time=0),
        )

    conn._frontend.CreateTask = create_task
    conn._frontend.CloseSession = close_session
    submitted = []
    worker = threading.Thread(target=lambda: submitted.append(session.run(b"second")))
    try:
        first = session.run(b"first")
        assert watching_first.wait(timeout=2)
        worker.start()
        assert creating_second.wait(timeout=2)
        session.close()
        with pytest.raises(FlameError, match="Task was cancelled"):
            first.result(timeout=2)
        assert session.id in conn._session_watches
        release_second.set()
        worker.join(timeout=2)
        assert not worker.is_alive()
        with pytest.raises(FlameError, match="Task was cancelled"):
            submitted[0].result(timeout=2)
    finally:
        release_second.set()
        closed.set()
        worker.join(timeout=2)
        conn.close()


def test_short_task_completes_with_ten_long_watches():
    from flamepy.proto.types_pb2 import Metadata, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Task as TaskProto

    class WatchFrontend:
        def __init__(self):
            self.started = threading.Event()
            self.long_watches = 0
            self.streams = 0

        async def WatchTasks(self, requests):  # noqa: N802
            self.streams += 1
            async for request in requests:
                if request.task_id != "11":
                    self.long_watches += 1
                    if self.long_watches == 10:
                        self.started.set()
                else:
                    yield TaskProto(
                        metadata=Metadata(id="11"),
                        spec=TaskSpec(session_id=request.session_id, output=b"short"),
                        status=TaskStatus(state=2, creation_time=0),
                    )

    watch_frontend = WatchFrontend()
    conn, session = _task_watch_test_session(watch_frontend)
    try:
        long_futures = [session.run(b"long") for _ in range(10)]
        assert watch_frontend.started.wait(timeout=2)
        short_future = session.run(b"short")
        assert short_future.result(timeout=2) == b"short"
        assert all(not future.done() for future in long_futures)
        assert watch_frontend.streams == 1
    finally:
        conn.close()
    assert all(future.done() for future in long_futures)


def test_future_callback_waiting_on_another_task_does_not_block_watches():
    from flamepy.proto.types_pb2 import Metadata, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Task as TaskProto

    callback_started = threading.Event()
    callback_done = threading.Event()
    callback_result = []

    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                if request.task_id == "2":
                    while not callback_started.is_set():
                        await asyncio.sleep(0.001)
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id, output=request.task_id.encode()),
                    status=TaskStatus(state=2, creation_time=0),
                )

    conn, session = _task_watch_test_session(WatchFrontend())
    try:
        first = session.run(b"first")
        second = session.run(b"second")

        def wait_for_second(_):
            callback_started.set()
            try:
                callback_result.append(second.result(timeout=2))
            finally:
                callback_done.set()

        first.add_done_callback(wait_for_second)
        assert callback_done.wait(timeout=2)
        assert callback_result == [b"2"]
        assert first.result(timeout=2) == b"1"
    finally:
        conn.close()


def test_task_completion_does_not_wait_for_saturated_callback_workers():
    from flamepy.proto.types_pb2 import Metadata, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Task as TaskProto

    class WatchFrontend:
        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id, output=b"done"),
                    status=TaskStatus(state=2, creation_time=0),
                )

    blocked = threading.Event()
    all_started = threading.Event()
    started = 0
    started_lock = threading.Lock()

    def slow_callback(_):
        nonlocal started
        with started_lock:
            started += 1
            if started == 8:
                all_started.set()
        blocked.wait(timeout=3)

    conn, session = _task_watch_test_session(WatchFrontend())
    try:
        for _ in range(8):
            session.run(b"blocked callback").add_done_callback(slow_callback)
        assert all_started.wait(timeout=2)
        assert session.run(b"unblocked future").result(timeout=2) == b"done"
    finally:
        blocked.set()
        conn.close()


def test_connection_close_cancels_pending_watches_and_is_idempotent():
    class WatchFrontend:
        started = threading.Event()

        async def WatchTasks(self, requests):  # noqa: N802
            async for request in requests:
                self.started.set()
                await asyncio.Future()
                if False:
                    yield request

    watch_frontend = WatchFrontend()
    conn, session = _task_watch_test_session(watch_frontend)
    future = session.run(b"input")
    assert watch_frontend.started.wait(timeout=2)
    closer = threading.Thread(target=conn.close)
    closer.start()
    conn.close()
    closer.join(timeout=2)
    assert not closer.is_alive()
    with pytest.raises(FlameError, match="connection closed before task completed"):
        future.result(timeout=2)


def test_run_watches_task_over_grpc_aio_channel():
    """The async watcher must work with the generated gRPC frontend stub."""
    from concurrent.futures import ThreadPoolExecutor

    import grpc

    from flamepy.proto.frontend_pb2_grpc import FrontendServicer, add_FrontendServicer_to_server
    from flamepy.proto.types_pb2 import Metadata, TaskSpec, TaskStatus
    from flamepy.proto.types_pb2 import Task as TaskProto

    class Frontend(FrontendServicer):
        def CreateTask(self, request, context):  # noqa: N802
            return TaskProto(
                metadata=Metadata(id="task-1"),
                spec=TaskSpec(session_id=request.task.session_id),
                status=TaskStatus(state=0, creation_time=0),
            )

        def WatchTasks(self, requests, context):  # noqa: N802
            for request in requests:
                yield TaskProto(
                    metadata=Metadata(id=request.task_id),
                    spec=TaskSpec(session_id=request.session_id, output=b"done"),
                    status=TaskStatus(state=2, creation_time=0),
                )

    server = grpc.server(ThreadPoolExecutor(max_workers=2))
    add_FrontendServicer_to_server(Frontend(), server)
    port = server.add_insecure_port("127.0.0.1:0")
    server.start()
    conn = None
    try:
        conn = client.Connection.connect(f"http://127.0.0.1:{port}")
        session = client.Session(
            connection=conn,
            id="session-1",
            application="test-app",
            state=SessionState.OPEN,
            creation_time=datetime.now(timezone.utc),
            pending=0,
            running=0,
            succeed=0,
            failed=0,
            completion_time=None,
        )
        assert session.run(b"input").result(timeout=2) == b"done"
        assert [task.output for task in session.watch_task("task-1")] == [b"done"]
    finally:
        if conn is not None:
            conn.close()
        server.stop(0).wait(timeout=2)

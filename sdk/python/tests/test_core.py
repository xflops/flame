"""Tests for the Flame synchronous core facade and shared types."""

import json
import threading
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


def test_sync_frontend_api_parity(frontend_server):
    endpoint, service, server_loop = frontend_server
    connection = client.connect(endpoint)
    try:
        connection.register_application(
            "app",
            ApplicationAttributes(
                schema=ApplicationSchema(input="bytes"),
            ),
        )
        connection.unregister_application("app")
        apps = connection.list_applications()
        assert len(apps) == 1 and apps[0].image == ""
        assert apps[0].schema.input == ""
        assert connection.get_application("missing") is None
        assert connection.list_executors() == []
        assert connection.list_nodes() == []

        attrs = SessionAttributes(application="app", id="sess-1", common_data=b"", resreq=ResourceRequirement(cpu=2))
        session = connection.create_session(attrs)
        assert session.id == "sess-1" and session.common_data() == b""
        assert session.events[0].code == 1001
        assert connection.open_session("sess-1").id == session.id
        assert connection.get_session("sess-1").id == session.id
        assert len(connection.list_sessions()) == 1
        task = session.create_task(b"input")
        assert task.id == "task-1"
        assert session.get_task(task.id).input == b""
        assert [item.id for item in session.list_tasks()] == ["task-1"]
        assert next(session.watch_task(task.id)).output == b"done"
        assert session.invoke(b"input") == b"done"
        assert connection.close_session(session.id).id == session.id
        created = [req for req in service.requests if req.DESCRIPTOR.name == "CreateSessionRequest"]
        assert created[0].session.resreq.cpu == 2
    finally:
        connection.close()


def test_failed_close_can_be_retried(frontend_server):
    endpoint, service, _ = frontend_server
    connection = client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        service.reject_close = True
        with pytest.raises(FlameError, match="close rejected"):
            session.close()
        assert connection.get_session(session.id).id == session.id
        service.reject_close = False
        session.close()
    finally:
        connection.close()


def test_run_reports_create_task_error_before_return(frontend_server):
    endpoint, service, _ = frontend_server
    connection = client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        service.reject_create_task = True
        with pytest.raises(FlameError, match="task rejected"):
            session.run(b"input")
        assert service.watches == 0
    finally:
        connection.close()


def test_watch_current_then_update_and_sync_callback_off_loop(frontend_server):
    endpoint, service, server_loop = frontend_server
    connection = client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        watcher = session.watch_task("hold")
        assert next(watcher).state == TaskState.PENDING
        future = session.run(b"hold")
        callback_threads = []
        completed = threading.Event()

        def callback(_future):
            callback_threads.append(threading.current_thread().name)
            completed.set()

        future.add_done_callback(callback)
        server_loop.call(_release(service))
        assert next(watcher).state == TaskState.SUCCEED
        assert future.result(timeout=3) == b"done"
        assert completed.wait(3)
        assert callback_threads == ["flamepy-callback_0"]
    finally:
        connection.close()


def test_close_errors_pending_watches_without_per_task_threads(frontend_server):
    endpoint, service, _ = frontend_server
    connection = client.connect(endpoint)
    session = connection.create_session(SessionAttributes(application="app"))
    futures = [session.run(b"hold") for _ in range(20)]
    assert len([thread for thread in threading.enumerate() if thread.name == "flamepy-aio"]) == 1
    connection.close()
    for future in futures:
        with pytest.raises(FlameError, match="connection closed"):
            future.result(timeout=3)


def test_result_callback_can_close_connection(frontend_server):
    endpoint, service, server_loop = frontend_server
    connection = client.connect(endpoint)
    session = connection.create_session(SessionAttributes(application="app"))
    result = session.run(b"hold")
    closed = threading.Event()
    errors = []

    def close_from_callback(_future):
        try:
            connection.close()
        except Exception as error:
            errors.append(error)
        finally:
            closed.set()

    result.add_done_callback(close_from_callback)
    server_loop.call(_release(service))
    assert result.result(timeout=3) == b"done"
    assert closed.wait(3)
    assert errors == []


def test_blocked_callbacks_do_not_starve_task_completion(frontend_server):
    endpoint, service, server_loop = frontend_server
    connection = client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        blockers = [session.run(b"hold") for _ in range(4)]
        dependents = []
        ready = threading.Event()
        all_started = threading.Event()
        all_done = threading.Event()
        lock = threading.Lock()
        started = 0
        finished = 0
        callback_errors = []

        def callback(_future, index):
            nonlocal started, finished
            with lock:
                started += 1
                if started == 4:
                    all_started.set()
            try:
                assert ready.wait(3)
                assert dependents[index].result(timeout=2) == b"done"
            except Exception as error:
                callback_errors.append(error)
            finally:
                with lock:
                    finished += 1
                    if finished == 4:
                        all_done.set()

        for index, blocker in enumerate(blockers):
            blocker.add_done_callback(lambda future, index=index: callback(future, index))
        server_loop.call(_release(service))
        assert all_started.wait(3)
        dependents.extend(session.run(b"input") for _ in range(4))
        ready.set()
        assert [future.result(timeout=1) for future in dependents] == [b"done"] * 4
        assert all_done.wait(3)
        assert callback_errors == []
    finally:
        connection.close()


def test_callbacks_keep_registration_order(frontend_server):
    endpoint, service, server_loop = frontend_server
    connection = client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        future = session.run(b"hold")
        order = []
        completed = threading.Event()
        future.add_done_callback(lambda _future: order.append(1))

        def last_callback(_future):
            order.append(2)
            completed.set()

        future.add_done_callback(last_callback)
        server_loop.call(_release(service))
        assert future.result(timeout=3) == b"done"
        assert completed.wait(3)
        assert order == [1, 2]
    finally:
        connection.close()


def test_slow_callbacks_bound_completion_jobs_without_blocking_aio(frontend_server, monkeypatch):
    endpoint, service, server_loop = frontend_server
    monkeypatch.setattr(client, "_MAX_CALLBACK_JOBS", 4)
    connection = client.connect(endpoint)
    release_callbacks = threading.Event()
    workers_started = threading.Event()
    all_callbacks = threading.Event()
    callback_count = 0
    callback_started = 0
    callback_lock = threading.Lock()
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        futures = [session.run(b"hold") for _ in range(20)]

        def callback(_future):
            nonlocal callback_count, callback_started
            with callback_lock:
                callback_started += 1
                if callback_started == 4:
                    workers_started.set()
            release_callbacks.wait(3)
            with callback_lock:
                callback_count += 1
                if callback_count == len(futures):
                    all_callbacks.set()

        for future in futures:
            future.add_done_callback(callback)
        server_loop.call(_release(service))
        assert workers_started.wait(3)
        assert session.get_task("task-1").id == "task-1"
        # The four occupied callback slots keep later watch tasks from
        # resolving; they do not create another unbounded completion queue.
        assert sum(future.done() for future in futures) <= 4
    finally:
        release_callbacks.set()
        assert all_callbacks.wait(3)
        assert [future.result(timeout=3) for future in futures] == [b"done"] * 20
        connection.close()


def test_close_drains_many_blocked_callbacks_with_fixed_workers(frontend_server, monkeypatch):
    endpoint, service, server_loop = frontend_server
    monkeypatch.setattr(client, "_MAX_CALLBACK_JOBS", 4)
    connection = client.connect(endpoint)
    release_callbacks = threading.Event()
    workers_started = threading.Event()
    all_callbacks = threading.Event()
    callback_count = 0
    callback_lock = threading.Lock()
    session = connection.create_session(SessionAttributes(application="app"))
    futures = [session.run(b"hold") for _ in range(20)]

    def callback(_future):
        nonlocal callback_count
        with callback_lock:
            callback_count += 1
            if callback_count == 4:
                workers_started.set()
            if callback_count == len(futures):
                all_callbacks.set()
        release_callbacks.wait(3)

    for future in futures:
        future.add_done_callback(callback)
    try:
        server_loop.call(_release(service))
        assert workers_started.wait(3)
        connection.close()
        assert all(future.done() for future in futures)
        assert len([thread for thread in threading.enumerate() if thread.name.startswith("flamepy-callback")]) <= 4
    finally:
        release_callbacks.set()
        assert all_callbacks.wait(3)
        assert callback_count == len(futures)


def test_mass_cancellation_runs_callbacks_inline_without_queueing(frontend_server):
    endpoint, _, _ = frontend_server
    connection = client.connect(endpoint)
    release_callbacks = threading.Event()
    first_callback = threading.Event()
    all_callbacks = threading.Event()
    lock = threading.Lock()
    callback_count = 0
    callback_threads = []
    futures = []
    cancel_thread = None
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        futures = [session.run(b"hold") for _ in range(20)]

        def callback(_future):
            nonlocal callback_count
            with lock:
                callback_count += 1
                callback_threads.append(threading.current_thread().name)
                if callback_count == 1:
                    first_callback.set()
                if callback_count == len(futures):
                    all_callbacks.set()
            release_callbacks.wait(3)

        for future in futures:
            future.add_done_callback(callback)

        def cancel_all():
            for future in futures:
                future.cancel()

        cancel_thread = threading.Thread(target=cancel_all, name="test-cancel")
        cancel_thread.start()
        assert first_callback.wait(3)
        assert sum(future.cancelled() for future in futures) == 1
        assert connection.list_nodes() == []
        assert cancel_thread.is_alive()
    finally:
        release_callbacks.set()
        if cancel_thread is not None:
            cancel_thread.join(timeout=3)
            assert not cancel_thread.is_alive()
        assert all_callbacks.wait(3)
        assert all(future.cancelled() for future in futures)
        assert callback_threads == ["test-cancel"] * 20
        connection.close()


def test_callback_worker_can_cancel_another_future_at_capacity(frontend_server, monkeypatch):
    endpoint, _, _ = frontend_server
    monkeypatch.setattr(client, "_MAX_CALLBACK_JOBS", 4)
    connection = client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))
        first = [session.run(b"hold") for _ in range(4)]
        second = [session.run(b"hold") for _ in range(4)]
        all_workers = threading.Barrier(4)
        all_nested = threading.Event()
        callback_count = 0
        callback_lock = threading.Lock()
        errors = []

        def nested_callback(_future):
            nonlocal callback_count
            with callback_lock:
                callback_count += 1
                if callback_count == 4:
                    all_nested.set()

        def cancel_second(_future, index):
            try:
                all_workers.wait(timeout=3)
                assert second[index].cancel()
            except Exception as error:
                errors.append(error)

        for index in range(4):
            second[index].add_done_callback(nested_callback)
            first[index].add_done_callback(lambda future, index=index: cancel_second(future, index))
        cancel_threads = [threading.Thread(target=future.cancel) for future in first]
        for thread in cancel_threads:
            thread.start()
        assert all_nested.wait(3)
        for thread in cancel_threads:
            thread.join(timeout=3)
            assert not thread.is_alive()
        assert callback_count == 4
        assert errors == []
        assert all(future.cancelled() for future in second)
    finally:
        connection.close()


async def _release(service):
    service.release.set()


def test_connection_rejects_empty_address():
    with pytest.raises(FlameError) as error:
        client.connect("")
    assert error.value.code == FlameErrorCode.INVALID_CONFIG


def test_module_connection_reopens_after_close(frontend_server, monkeypatch):
    endpoint, _, _ = frontend_server
    first = client.connect(endpoint)
    monkeypatch.setattr(client.ConnectionInstance, "_connection", first)
    monkeypatch.setattr(client, "FlameContext", lambda: type("Context", (), {"endpoint": endpoint, "tls": None})())
    assert client.ConnectionInstance.instance() is first
    first.close()
    second = client.ConnectionInstance.instance()
    try:
        assert second is not first
        assert second.list_nodes() == []
    finally:
        second.close()


def test_module_connection_resets_lock_and_handle_after_fork(monkeypatch):
    old_lock = threading.Lock()
    old_lock.acquire()
    monkeypatch.setattr(client.ConnectionInstance, "_lock", old_lock)
    monkeypatch.setattr(client.ConnectionInstance, "_connection", object())
    monkeypatch.setattr(client.ConnectionInstance, "_context", object())
    try:
        client.ConnectionInstance._reset_after_fork()
        assert client.ConnectionInstance._connection is None
        assert client.ConnectionInstance._context is None
        assert client.ConnectionInstance._lock.acquire(blocking=False)
        client.ConnectionInstance._lock.release()
    finally:
        old_lock.release()


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

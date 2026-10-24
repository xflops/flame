"""Tests for flamepy.agent.Sandbox."""

import json
from concurrent.futures import Future
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from flamepy import FlameError, FlameErrorCode, ResourceRequirement
from flamepy.agent import CommandResult, Sandbox
from flamepy.agent import sandbox as sandbox_mod
from flamepy.core.types import SessionState


def ok_output(exit_code=0, stdout=b"ok\n", stderr=b""):
    return json.dumps({"exit_code": exit_code, "stdout": list(stdout), "stderr": list(stderr)}).encode("utf-8")


class FakeSession:
    def __init__(self, session_id="sandbox-1", application="flmbox"):
        self.id = session_id
        self.application = application
        self.closed = False
        self.submitted = []
        self.result = ok_output()
        self.error = None

    def submit(self, payload):
        self.submitted.append(payload)
        future = Future()
        if self.error is not None:
            future.set_exception(self.error)
        else:
            future.set_result(self.result)
        return future

    def close(self):
        self.closed = True


class FakeCore:
    def __init__(self, session=None):
        self.session = session or FakeSession()
        self.state = SessionState.OPEN
        self.create_kwargs = None
        self.create_app = None
        self.opened = None
        self.suspended = 0
        self.resumed = 0
        # states returned by successive get_session calls once suspend/resume was requested
        self.after_suspend = [SessionState.SUSPENDING, SessionState.SUSPENDED]
        self.after_resume = [SessionState.SUSPENDED, SessionState.OPEN]
        self.queue = []

    def create(self, application, **kwargs):
        self.create_app = application
        self.create_kwargs = kwargs
        return self.session

    def open(self, sid):
        self.opened = sid
        return self.session

    def get(self, sid):
        if self.queue:
            self.state = self.queue.pop(0)
        return SimpleNamespace(id=sid, state=self.state)

    def suspend(self, sid):
        self.suspended += 1
        self.queue = list(self.after_suspend)

    def resume(self, sid):
        self.resumed += 1
        self.queue = list(self.after_resume)


@pytest.fixture
def core():
    fake = FakeCore()
    with (
        patch.object(sandbox_mod, "_create_core_session", fake.create),
        patch.object(sandbox_mod, "_open_core_session", fake.open),
        patch.object(sandbox_mod, "_get_core_session", fake.get),
        patch.object(sandbox_mod, "_suspend_core_session", fake.suspend),
        patch.object(sandbox_mod, "_resume_core_session", fake.resume),
    ):
        yield fake


def sent(core, index=0):
    return json.loads(core.session.submitted[index].decode("utf-8"))


def test_create_session_args(core):
    box = Sandbox.create(image="alpine:3")
    kw = core.create_kwargs
    assert core.create_app == "flmbox"
    assert json.loads(kw["common_data"].decode("utf-8")) == {"image": "alpine:3"}
    assert kw["min_instances"] == 1 and kw["max_instances"] == 1 and kw["batch_size"] == 1
    assert kw["resreq"] is None
    assert kw["session_id"].startswith("sandbox-") and len(kw["session_id"]) == len("sandbox-") + 10
    assert box.id == "sandbox-1"


def test_create_without_image_and_resreq_not_in_common_data(core):
    resreq = ResourceRequirement(cpu=2, memory=1024, gpu=0)
    Sandbox.create(resreq=resreq)
    kw = core.create_kwargs
    assert json.loads(kw["common_data"].decode("utf-8")) == {"image": None}
    assert kw["resreq"].cpu == 2 and kw["resreq"] is not resreq
    resreq.cpu = 9
    assert kw["resreq"].cpu == 2


def test_create_rejects_bad_image(core):
    with pytest.raises(FlameError) as exc:
        Sandbox.create(image="")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_connect_checks_application(core):
    assert Sandbox.connect("sandbox-1").id == "sandbox-1"
    assert core.opened == "sandbox-1"
    core.session = FakeSession(application="flmexec")
    with pytest.raises(FlameError) as exc:
        Sandbox.connect("x")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_run_request_and_result(core):
    box = Sandbox.create()
    core.session.result = ok_output(3, b"out\xff", b"err")
    result = box.run("ls", cwd="/tmp", envs={"A": "1"}, timeout=5)
    assert sent(core) == {"code": "ls", "language": "shell", "cwd": "/tmp", "envs": {"A": "1"}, "input": None, "timeout": 5}
    assert result == CommandResult(3, b"out\xff", b"err")
    assert not result.ok
    assert result.stdout_text() == "out�"
    assert result.stderr_text() == "err"


def test_run_defaults_and_zero_exit(core):
    box = Sandbox.create()
    result = box.run("true")
    assert sent(core) == {"code": "true", "language": "shell", "cwd": None, "envs": None, "input": None, "timeout": None}
    assert result.ok


def test_run_code_request_and_input(core):
    box = Sandbox.create()
    box.run_code("echo hi", language="shell", input=b"in", timeout=2.5)
    assert sent(core) == {"code": "echo hi", "language": "shell", "cwd": None, "envs": None, "input": [105, 110], "timeout": 2.5}
    box.run_code("print(1)")
    assert sent(core, 1) == {"code": "print(1)", "language": "python", "cwd": None, "envs": None, "input": None, "timeout": None}


def test_validation_errors(core):
    box = Sandbox.create()
    for call in (
        lambda: box.run_code("x", language="javascript"),
        lambda: box.run_code("x", input="str"),
        lambda: box.run("ls", timeout=0),
        lambda: box.run(""),
    ):
        with pytest.raises(FlameError) as exc:
            call()
        assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT
    assert core.session.submitted == []


def test_submit_futures(core):
    box = Sandbox.create()
    assert box.submit("ls").result() == CommandResult(0, b"ok\n", b"")
    assert box.submit_code("print(1)").result().stdout_text() == "ok\n"


def test_submit_propagates_errors(core):
    box = Sandbox.create()
    core.session.error = FlameError(FlameErrorCode.INTERNAL, "boom")
    with pytest.raises(FlameError) as exc:
        box.submit("ls").result()
    assert exc.value.message == "boom"


@pytest.mark.parametrize("state", [SessionState.SUSPENDED, SessionState.SUSPENDING, SessionState.CLOSED])
def test_not_open_rejects_tasks(core, state):
    box = Sandbox.create()
    core.state = state
    for call in (lambda: box.run("ls"), lambda: box.submit("ls"), lambda: box.run_code("1"), lambda: box.submit_code("1")):
        with pytest.raises(FlameError) as exc:
            call()
        assert exc.value.code == FlameErrorCode.INVALID_STATE
    assert core.session.submitted == []


def test_state_mapping(core):
    box = Sandbox.create()
    for state, name in [
        (SessionState.OPEN, "open"),
        (SessionState.SUSPENDING, "suspending"),
        (SessionState.SUSPENDED, "suspended"),
        (SessionState.CLOSED, "closed"),
    ]:
        core.state = state
        assert box.state == name


def test_suspend_polls_until_suspended(core):
    box = Sandbox.create()
    with patch.object(sandbox_mod.time, "sleep") as sleep:
        box.suspend()
    assert core.suspended == 1
    assert core.state == SessionState.SUSPENDED
    sleep.assert_called_with(0.2)


def test_suspend_times_out(core):
    box = Sandbox.create()
    core.after_suspend = [SessionState.SUSPENDING] * 100
    clock = iter(range(0, 1000, 10))
    with patch.object(sandbox_mod.time, "sleep"), patch.object(sandbox_mod.time, "monotonic", lambda: next(clock)):
        with pytest.raises(FlameError) as exc:
            box.suspend(timeout=25)
    assert exc.value.code == FlameErrorCode.INTERNAL


def test_resume(core):
    box = Sandbox.create()
    core.state = SessionState.SUSPENDED
    with patch.object(sandbox_mod.time, "sleep"):
        box.resume()
    assert core.resumed == 1
    assert box.state == "open"
    box.run("ls")


def test_close_idempotent_and_blocks_use(core):
    box = Sandbox.create()
    box.close()
    box.close()
    assert core.session.closed is True
    with pytest.raises(FlameError) as exc:
        box.run("ls")
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_context_manager_closes(core):
    with Sandbox.create() as box:
        assert box.id == "sandbox-1"
    assert core.session.closed is True


@pytest.mark.parametrize("raw", [b"not-json", b'{"exit_code": 0}', b"\xff"])
def test_invalid_output_json(core, raw):
    box = Sandbox.create()
    core.session.result = raw
    with pytest.raises(FlameError) as exc:
        box.run("ls")
    assert exc.value.code == FlameErrorCode.INTERNAL

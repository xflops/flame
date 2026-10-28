"""Tests for flamepy.agent Session APIs."""

import importlib
import json
import threading
from concurrent.futures import Future
from dataclasses import FrozenInstanceError
from unittest.mock import patch

import pytest

from flamepy import FlameError, FlameErrorCode, ResourceRequirement
from flamepy.agent import CommandResult, Session, open_session
from flamepy.agent.sandbox import _decode_result, _encode_request
from flamepy.agent.session import _encode_options, _SessionOptions
from flamepy.core import TaskFuture
from flamepy.core import client as core_client
from flamepy.core.types import SessionAttributes


class FakeSession:
    def __init__(self, session_id="ssn-1", application="flmbox", common_data=None):
        self.id = session_id
        self.application = application
        self._common_data = common_data
        self.closed = False
        self.invoked = []
        self.invoke_result = json.dumps({"exit_code": 0, "stdout": list(b"ok\n"), "stderr": []}).encode("utf-8")

    def common_data(self):
        return self._common_data

    def run(self, payload):
        self.invoked.append(("run", payload))
        return self.invoke_result

    def submit(self, payload):
        self.invoked.append(("submit", payload))
        future = Future()
        future.set_result(self.invoke_result)
        return future

    def close(self):
        self.closed = True


def create_from_options(application, common_data=None, **kwargs):
    return FakeSession(common_data=common_data)


def test_agent_public_exports():
    import flamepy.agent as agent

    assert agent.__all__ == [
        "CommandResult",
        "Sandbox",
        "Session",
        "open_session",
    ]
    assert agent.Session is Session
    assert agent.open_session is open_session
    assert not hasattr(agent, "create_session")
    assert not hasattr(agent, "close_session")
    assert not hasattr(agent, "SessionAttr")
    assert not hasattr(agent, "ResourceRequirement")
    assert not hasattr(agent, "FlameError")
    assert not hasattr(agent, "Script")
    assert not hasattr(agent, "flmexec")
    assert not hasattr(agent, "SessionOutput")

    with pytest.raises(ModuleNotFoundError):
        importlib.import_module("flamepy.tools")


def test_private_session_options_are_frozen():
    options = _SessionOptions(image="a")
    with pytest.raises(FrozenInstanceError):
        options.image = "b"


def test_open_without_id_creates_normalized_session():
    created = {}

    def fake_create_session(application, common_data=None, min_instances=0, max_instances=None, resreq=None):
        created["application"] = application
        created["common_data"] = common_data
        created["min_instances"] = min_instances
        created["max_instances"] = max_instances
        created["resreq"] = resreq
        return FakeSession(common_data=common_data)

    with patch("flamepy.agent.session._create_core_session", fake_create_session):
        agent_session = open_session(min_instances=1)

    assert created["application"] == "flmbox"
    assert created["max_instances"] is None
    assert created["min_instances"] == 1
    assert agent_session.attr.image is None
    assert agent_session.id == "ssn-1"
    stored = json.loads(created["common_data"].decode("utf-8"))
    assert "language" not in stored
    assert "runtime" not in stored
    assert stored["min_instances"] == 1
    assert stored["resreq"] is None


def test_open_stores_image_for_flmbox_and_restores_it():
    created = {}

    def fake_create_session(application, common_data=None, **kwargs):
        created["common_data"] = common_data
        return FakeSession(common_data=common_data)

    with patch("flamepy.agent.session._create_core_session", fake_create_session):
        agent_session = open_session(image="python:3.12-slim")

    assert agent_session.attr.image == "python:3.12-slim"
    # flmbox reads the sandbox image from the session common_data "image" key.
    assert json.loads(created["common_data"].decode("utf-8"))["image"] == "python:3.12-slim"

    reopened = FakeSession(common_data=created["common_data"])
    with patch("flamepy.agent.session._open_core_session", return_value=reopened):
        assert open_session(ssn_id="ssn-1").attr.image == "python:3.12-slim"


def test_open_without_image_uses_the_service_default():
    created = {}

    def fake_create_session(application, common_data=None, **kwargs):
        created["common_data"] = common_data
        return FakeSession(common_data=common_data)

    with patch("flamepy.agent.session._create_core_session", fake_create_session):
        assert open_session().attr.image is None
    assert json.loads(created["common_data"].decode("utf-8"))["image"] is None


@pytest.mark.parametrize("image", ["", "  ", 5])
def test_open_rejects_invalid_image(image):
    with pytest.raises(FlameError) as exc:
        open_session(image=image)
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_run_code_rejects_unsupported_language():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        for call in (agent_session.run_code, agent_session.submit_code):
            with pytest.raises(FlameError) as exc:
                call("1", language="javascript")
            assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT
    assert agent_session._session.invoked == []


def test_open_copies_resreq():
    resreq = ResourceRequirement(cpu=2, memory=1024, gpu=1)
    created = {}

    def fake_create_session(application, common_data=None, resreq=None, **kwargs):
        created["resreq"] = resreq
        return FakeSession(common_data=common_data)

    with patch("flamepy.agent.session._create_core_session", fake_create_session):
        agent_session = open_session(resreq=resreq)

    resreq.cpu = 99
    assert agent_session.attr.resreq.cpu == 2
    assert created["resreq"].cpu == 2
    assert created["resreq"] is not resreq


def test_run_code_encodes_flmbox_request():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        result = agent_session.run_code("echo hello", input=b"in")

    assert result == CommandResult(exit_code=0, stdout=b"ok\n", stderr=b"")
    kind, payload = agent_session._session.invoked[0]
    assert kind == "run"
    request = json.loads(payload.decode("utf-8"))
    assert request == {"code": "echo hello", "language": "shell", "cwd": None, "envs": None, "input": [105, 110], "timeout": None}


def test_run_and_submit_code_pass_per_call_options_to_flmbox():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        agent_session.run_code("echo hi", cwd="/tmp", envs={"A": "1"}, timeout=5)
        agent_session.submit_code("print(1)", language="python", cwd="/work", timeout=2.5).result()

    run_request = json.loads(agent_session._session.invoked[0][1].decode("utf-8"))
    assert run_request == {"code": "echo hi", "language": "shell", "cwd": "/tmp", "envs": {"A": "1"}, "input": None, "timeout": 5}
    submit_request = json.loads(agent_session._session.invoked[1][1].decode("utf-8"))
    assert submit_request["language"] == "python"
    assert submit_request["cwd"] == "/work"
    assert submit_request["timeout"] == 2.5


def test_run_code_without_input():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        agent_session.run_code("print(1)")

    request = json.loads(agent_session._session.invoked[0][1].decode("utf-8"))
    assert request["language"] == "shell"
    assert request["input"] is None


def test_run_code_returns_a_failing_script_as_a_result():
    core_session = FakeSession()
    core_session.invoke_result = json.dumps({"exit_code": 3, "stdout": list(b"partial"), "stderr": list(b"boom")}).encode("utf-8")
    with patch("flamepy.agent.session._create_core_session", return_value=core_session):
        result = open_session().run_code("raise SystemExit(3)")

    assert result.exit_code == 3
    assert not result.ok
    assert result.stdout_text() == "partial"
    assert result.stderr_text() == "boom"


def test_run_code_rejects_non_bytes_input():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        with pytest.raises(FlameError) as exc:
            agent_session.run_code("print(1)", input="not-bytes")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_submit_code_decodes_future():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        future = agent_session.submit_code("print(1)")

    result = future.result()
    assert isinstance(result, CommandResult)
    assert result.stdout_text() == "ok\n"
    assert agent_session._session.invoked[0][0] == "submit"


def test_submit_code_completion_is_not_blocked_by_user_callbacks(frontend_server):
    endpoint, _, _ = frontend_server
    connection = core_client.connect(endpoint)
    core_session = connection.create_session(SessionAttributes(application="flmbox"))
    release_callbacks = threading.Event()
    all_callbacks_started = threading.Event()
    started = 0
    started_lock = threading.Lock()

    def blocked_callback(_):
        nonlocal started
        with started_lock:
            started += 1
            if started == 4:
                all_callbacks_started.set()
        release_callbacks.wait(timeout=5)

    class PendingSession(FakeSession):
        def submit(self, payload):
            self.raw_future = TaskFuture(connection)
            return self.raw_future

    try:
        for _ in range(4):
            future = TaskFuture(connection)
            future.add_done_callback(blocked_callback)
            future.set_result(b"done")
        assert all_callbacks_started.wait(timeout=2)

        core_session = PendingSession()
        agent_session = Session(core_session, _SessionOptions())
        mapped = agent_session.submit_code("print(1)")
        core_session.raw_future.set_result(core_session.invoke_result)
        assert mapped.result(timeout=2).stdout_text() == "ok\n"
    finally:
        release_callbacks.set()
        connection.close()


def test_open_restores_options():
    options = _SessionOptions(min_instances=1)
    session = FakeSession(session_id="ssn-open", common_data=_encode_options(options))

    with patch("flamepy.agent.session._open_core_session", return_value=session):
        agent_session = open_session(ssn_id="ssn-open")

    assert agent_session.id == "ssn-open"
    assert agent_session.attr.min_instances == 1


def test_open_by_id_passes_creation_options_as_spec():
    spec = {}

    def fake_open(session_id, attrs=None):
        spec["id"], spec["attrs"] = session_id, attrs
        return FakeSession(session_id=session_id, common_data=attrs.common_data)

    resreq = ResourceRequirement(cpu=1)
    with patch("flamepy.agent.session._open_core_session", fake_open):
        agent_session = open_session(ssn_id="named", image="img", min_instances=1, max_instances=2, resreq=resreq)

    attrs = spec["attrs"]
    assert spec["id"] == "named"
    assert attrs.id == "named"
    assert attrs.application == "flmbox"
    assert attrs.min_instances == 1
    assert attrs.max_instances == 2
    assert attrs.resreq == resreq
    assert json.loads(attrs.common_data.decode("utf-8"))["image"] == "img"
    assert agent_session.id == "named"
    assert agent_session.attr.image == "img"


def test_open_existing_session_keeps_its_own_options():
    existing = FakeSession(common_data=_encode_options(_SessionOptions(image="img")))

    with patch("flamepy.agent.session._open_core_session", return_value=existing):
        agent_session = open_session(ssn_id="ssn-1", image="ignored")

    assert agent_session.attr.image == "img"


def test_open_rejects_non_flmbox_session():
    session = FakeSession(application="flmping")

    with patch("flamepy.agent.session._open_core_session", return_value=session):
        with pytest.raises(FlameError) as exc:
            open_session(ssn_id="other")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_open_rejects_invalid_common_data():
    session = FakeSession(common_data=b"not-json")

    with patch("flamepy.agent.session._open_core_session", return_value=session):
        with pytest.raises(FlameError) as exc:
            open_session(ssn_id="ssn-1")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_open_rejects_invalid_input_type():
    with pytest.raises(FlameError) as exc:
        open_session(ssn_id=123)
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_close_destroys_session_and_is_idempotent():
    core_session = FakeSession()

    with patch("flamepy.agent.session._create_core_session", return_value=core_session):
        agent_session = open_session()
        agent_session.close()
        agent_session.close()

    assert core_session.closed is True
    with pytest.raises(FlameError) as exc:
        agent_session.run_code("print(1)")
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_context_manager_closes_session():
    core_session = FakeSession()

    with patch("flamepy.agent.session._create_core_session", return_value=core_session):
        with open_session() as agent_session:
            assert agent_session.id == "ssn-1"
        with pytest.raises(FlameError) as exc:
            agent_session.run_code("print(1)")
    assert core_session.closed is True
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_decode_result_rejects_invalid_output():
    with pytest.raises(FlameError) as exc:
        _decode_result(b"not-json")
    assert exc.value.code == FlameErrorCode.INTERNAL


def test_encode_request_matches_flmbox_contract():
    payload = json.loads(_encode_request("print(1)", "python", None, None, None, None).decode("utf-8"))
    assert payload == {"code": "print(1)", "language": "python", "cwd": None, "envs": None, "input": None, "timeout": None}

    payload = json.loads(_encode_request("echo ok", "shell", None, None, b"ab", None).decode("utf-8"))
    assert payload["input"] == [97, 98]

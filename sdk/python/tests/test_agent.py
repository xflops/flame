"""Tests for flamepy.agent Session APIs."""

import importlib
import json
import threading
from concurrent.futures import Future
from dataclasses import FrozenInstanceError
from unittest.mock import patch

import pytest

from flamepy import FlameError, FlameErrorCode, ResourceRequirement
from flamepy.agent import Session, SessionOutput, open_session
from flamepy.agent.session import _decode_output, _encode_options, _encode_script, _SessionOptions
from flamepy.core import client as core_client
from flamepy.core.client import _LazyTaskFuture
from flamepy.core.types import SessionAttributes


class FakeSession:
    def __init__(self, session_id="ssn-1", application="flmexec", common_data=None):
        self.id = session_id
        self.application = application
        self._common_data = common_data
        self.closed = False
        self.invoked = []
        self.invoke_result = json.dumps({"data": list(b"ok\n")}).encode("utf-8")

    def common_data(self):
        return self._common_data

    def invoke(self, payload):
        self.invoked.append(("invoke", payload))
        return self.invoke_result

    def run(self, payload):
        self.invoked.append(("run", payload))
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
        "Session",
        "SessionOutput",
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

    with pytest.raises(ModuleNotFoundError):
        importlib.import_module("flamepy.tools")


def test_private_session_options_are_frozen():
    options = _SessionOptions(language="python")
    with pytest.raises(FrozenInstanceError):
        options.language = "shell"


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
        agent_session = open_session(language="Python", min_instances=1)

    assert created["application"] == "flmexec"
    assert created["min_instances"] == 1
    assert agent_session.attr.language == "python"
    assert agent_session.id == "ssn-1"
    stored = json.loads(created["common_data"].decode("utf-8"))
    assert stored["language"] == "python"
    assert stored["runtime"] is None
    assert stored["min_instances"] == 1
    assert stored["resreq"] is None


def test_open_rejects_unsupported_language():
    with pytest.raises(FlameError) as exc:
        open_session(language="javascript")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_open_copies_resreq():
    resreq = ResourceRequirement(cpu=2, memory=1024, gpu=1)
    created = {}

    def fake_create_session(application, common_data=None, resreq=None, **kwargs):
        created["resreq"] = resreq
        return FakeSession(common_data=common_data)

    with patch("flamepy.agent.session._create_core_session", fake_create_session):
        agent_session = open_session(language="python", resreq=resreq)

    resreq.cpu = 99
    assert agent_session.attr.resreq.cpu == 2
    assert created["resreq"].cpu == 2
    assert created["resreq"] is not resreq


def test_run_code_encodes_session_language_and_runtime():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session(language="shell", runtime="bash")
        output = agent_session.run_code("echo hello", input=b"in")

    assert output.text() == "ok\n"
    kind, payload = agent_session._session.invoked[0]
    assert kind == "invoke"
    request = json.loads(payload.decode("utf-8"))
    assert request == {"language": "shell", "runtime": "bash", "code": "echo hello", "input": [105, 110]}


def test_run_code_omits_runtime_when_unset():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session()
        agent_session.run_code("print(1)")

    request = json.loads(agent_session._session.invoked[0][1].decode("utf-8"))
    assert "runtime" not in request
    assert request["input"] is None


def test_run_code_rejects_non_bytes_input():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session(language="python")
        with pytest.raises(FlameError) as exc:
            agent_session.run_code("print(1)", input="not-bytes")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_submit_code_decodes_future():
    with patch("flamepy.agent.session._create_core_session", create_from_options):
        agent_session = open_session(language="python")
        future = agent_session.submit_code("print(1)")

    result = future.result()
    assert isinstance(result, SessionOutput)
    assert result.text() == "ok\n"
    assert agent_session._session.invoked[0][0] == "run"


def test_submit_code_completion_is_not_blocked_by_user_callbacks(frontend_server):
    endpoint, _, _ = frontend_server
    connection = core_client.connect(endpoint)
    core_session = connection.create_session(SessionAttributes(application="flmexec"))
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
        def run(self, payload):
            self.raw_future = _LazyTaskFuture(core_session)
            return self.raw_future

    try:
        for _ in range(4):
            future = _LazyTaskFuture(core_session)
            future.add_done_callback(blocked_callback)
            future.set_result(b"done")
        assert all_callbacks_started.wait(timeout=2)

        core_session = PendingSession()
        agent_session = Session(core_session, _SessionOptions(language="python"))
        mapped = agent_session.submit_code("print(1)")
        core_session.raw_future.set_result(core_session.invoke_result)
        assert mapped.result(timeout=2).text() == "ok\n"
    finally:
        release_callbacks.set()
        connection.close()


def test_open_restores_options():
    options = _SessionOptions(language="python", runtime="3.12", min_instances=1)
    session = FakeSession(session_id="ssn-open", common_data=_encode_options(options))

    with patch("flamepy.agent.session._open_core_session", return_value=session):
        agent_session = open_session(ssn_id="ssn-open")

    assert agent_session.id == "ssn-open"
    assert agent_session.attr.language == "python"
    assert agent_session.attr.runtime == "3.12"
    assert agent_session.attr.min_instances == 1


def test_open_by_id_ignores_creation_options():
    options = _SessionOptions(language="shell", runtime="bash")
    core_session = FakeSession(common_data=_encode_options(options))

    with patch("flamepy.agent.session._open_core_session", return_value=core_session) as core_open:
        agent_session = open_session(ssn_id="ssn-1", language="unsupported", runtime="ignored")

    core_open.assert_called_once_with("ssn-1")
    assert agent_session.attr == options


def test_open_rejects_non_flmexec_session():
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
        agent_session = open_session(language="python")
        agent_session.close()
        agent_session.close()

    assert core_session.closed is True
    with pytest.raises(FlameError) as exc:
        agent_session.run_code("print(1)")
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_context_manager_closes_session():
    core_session = FakeSession()

    with patch("flamepy.agent.session._create_core_session", return_value=core_session):
        with open_session(language="python") as agent_session:
            assert agent_session.id == "ssn-1"
        with pytest.raises(FlameError) as exc:
            agent_session.run_code("print(1)")
    assert core_session.closed is True
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_session_output_empty_and_invalid():
    assert _decode_output(None).data == b""
    with pytest.raises(FlameError) as exc:
        _decode_output(b"not-json")
    assert exc.value.code == FlameErrorCode.INTERNAL


def test_encode_script_matches_flmexec_contract():
    payload = json.loads(_encode_script("python", None, "print(1)", None).decode("utf-8"))
    assert payload == {"language": "python", "code": "print(1)", "input": None}

    payload = json.loads(_encode_script("shell", "zsh", "echo ok", b"ab").decode("utf-8"))
    assert payload == {"language": "shell", "runtime": "zsh", "code": "echo ok", "input": [97, 98]}

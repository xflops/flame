"""Tests for flamepy.tools Sandbox APIs."""

import json
from concurrent.futures import Future
from dataclasses import FrozenInstanceError
from unittest.mock import patch

import pytest

from flamepy import FlameError, FlameErrorCode, ResourceRequirement
from flamepy.tools import Sandbox, SandboxAttr, SandboxOutput
from flamepy.tools.sandbox import _decode_output, _encode_attr, _encode_script


class FakeSession:
    def __init__(self, session_id="sb-1", application="flmexec", common_data=None):
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


def test_tools_public_exports():
    import flamepy.tools as tools

    assert tools.__all__ == ["Sandbox", "SandboxAttr", "SandboxOutput"]
    assert not hasattr(tools, "Session")
    assert not hasattr(tools, "create_session")
    assert not hasattr(tools, "ResourceRequirement")
    assert not hasattr(tools, "FlameError")
    assert not hasattr(tools, "Script")
    assert not hasattr(tools, "flmexec")


def test_sandbox_attr_is_frozen():
    attr = SandboxAttr(language="python")
    with pytest.raises(FrozenInstanceError):
        attr.language = "shell"


def test_create_normalizes_language_and_persists_attr():
    created = {}

    def fake_create_session(application, common_data=None, min_instances=0, max_instances=None, resreq=None, **kwargs):
        created["application"] = application
        created["common_data"] = common_data
        created["min_instances"] = min_instances
        created["max_instances"] = max_instances
        created["resreq"] = resreq
        return FakeSession(common_data=common_data)

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="Python", min_instances=1))

    assert created["application"] == "flmexec"
    assert created["min_instances"] == 1
    assert sb.attr.language == "python"
    assert sb.sandbox_id == "sb-1"
    stored = json.loads(created["common_data"].decode("utf-8"))
    assert stored["language"] == "python"
    assert stored["runtime"] is None
    assert stored["min_instances"] == 1
    assert stored["resreq"] is None


def test_create_rejects_unsupported_language():
    with pytest.raises(FlameError) as exc:
        Sandbox.create(SandboxAttr(language="javascript"))
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_create_copies_resreq():
    resreq = ResourceRequirement(cpu=2, memory=1024, gpu=1)
    created = {}

    def fake_create_session(application, common_data=None, min_instances=0, max_instances=None, resreq=None, **kwargs):
        created["resreq"] = resreq
        return FakeSession(common_data=common_data)

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="python", resreq=resreq))

    resreq.cpu = 99
    assert sb.attr.resreq.cpu == 2
    assert created["resreq"].cpu == 2
    assert created["resreq"] is not resreq


def test_run_code_encodes_attr_language_and_runtime():
    session = FakeSession()

    def fake_create_session(*args, **kwargs):
        return session

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="shell", runtime="bash"))
        output = sb.run_code("echo hello", input=b"in")

    assert output.text() == "ok\n"
    kind, payload = session.invoked[0]
    assert kind == "invoke"
    request = json.loads(payload.decode("utf-8"))
    assert request == {"language": "shell", "runtime": "bash", "code": "echo hello", "input": [105, 110]}


def test_run_code_omits_runtime_when_unset():
    session = FakeSession()

    def fake_create_session(*args, **kwargs):
        return session

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="python"))
        sb.run_code("print(1)")

    request = json.loads(session.invoked[0][1].decode("utf-8"))
    assert "runtime" not in request
    assert request["input"] is None


def test_run_code_rejects_non_bytes_input():
    session = FakeSession()

    def fake_create_session(*args, **kwargs):
        return session

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="python"))
        with pytest.raises(FlameError) as exc:
            sb.run_code("print(1)", input="not-bytes")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_submit_code_decodes_future():
    session = FakeSession()

    def fake_create_session(*args, **kwargs):
        return session

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="python"))
        future = sb.submit_code("print(1)")

    result = future.result()
    assert isinstance(result, SandboxOutput)
    assert result.text() == "ok\n"
    assert session.invoked[0][0] == "run"


def test_open_restores_attr():
    attr = SandboxAttr(language="python", runtime="3.12", min_instances=1)
    session = FakeSession(session_id="sb-open", common_data=_encode_attr(attr))

    with patch("flamepy.tools.sandbox.open_session", return_value=session):
        sb = Sandbox.open("sb-open")

    assert sb.sandbox_id == "sb-open"
    assert sb.attr.language == "python"
    assert sb.attr.runtime == "3.12"
    assert sb.attr.min_instances == 1


def test_open_rejects_non_flmexec_session():
    session = FakeSession(application="flmping")

    with patch("flamepy.tools.sandbox.open_session", return_value=session):
        with pytest.raises(FlameError) as exc:
            Sandbox.open("other")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_open_rejects_invalid_common_data():
    session = FakeSession(common_data=b"not-json")

    with patch("flamepy.tools.sandbox.open_session", return_value=session):
        with pytest.raises(FlameError) as exc:
            Sandbox.open("sb-1")
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT


def test_close_destroys_sandbox_and_is_idempotent():
    session = FakeSession()

    def fake_create_session(*args, **kwargs):
        return session

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        sb = Sandbox.create(SandboxAttr(language="python"))
        sb.close()
        sb.close()

    assert session.closed is True
    with pytest.raises(FlameError) as exc:
        sb.run_code("print(1)")
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_context_manager_closes_sandbox():
    session = FakeSession()

    def fake_create_session(*args, **kwargs):
        return session

    with patch("flamepy.tools.sandbox.create_session", fake_create_session):
        with Sandbox.create(SandboxAttr(language="python")) as sb:
            assert sb.sandbox_id == "sb-1"
        with pytest.raises(FlameError) as exc:
            sb.run_code("print(1)")
    assert session.closed is True
    assert exc.value.code == FlameErrorCode.INVALID_STATE


def test_sandbox_output_empty_and_invalid():
    assert _decode_output(None).data == b""
    with pytest.raises(FlameError) as exc:
        _decode_output(b"not-json")
    assert exc.value.code == FlameErrorCode.INTERNAL


def test_encode_script_matches_flmexec_contract():
    payload = json.loads(_encode_script("python", None, "print(1)", None).decode("utf-8"))
    assert payload == {"language": "python", "code": "print(1)", "input": None}

    payload = json.loads(_encode_script("shell", "zsh", "echo ok", b"ab").decode("utf-8"))
    assert payload == {"language": "shell", "runtime": "zsh", "code": "echo ok", "input": [97, 98]}

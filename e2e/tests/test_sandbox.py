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

import pytest
from flamepy import FlameError, FlameErrorCode
from flamepy.tools import Sandbox, SandboxAttr


def test_sandbox_python_run_code():
    with Sandbox.create(SandboxAttr(language="python")) as sb:
        result = sb.run_code("print(1 + 2)")
    assert result.text().strip() == "3"


def test_sandbox_shell_run_code():
    with Sandbox.create(SandboxAttr(language="shell", runtime="bash")) as sb:
        result = sb.run_code("echo hello")
    assert result.text().strip() == "hello"


def test_sandbox_open_restores_attr():
    sb = Sandbox.create(SandboxAttr(language="python", runtime="3.12"))
    try:
        other = Sandbox.open(sb.sandbox_id)
        assert other.sandbox_id == sb.sandbox_id
        assert other.attr.language == "python"
        assert other.attr.runtime == "3.12"
        assert other.run_code("print('ready')").text().strip() == "ready"
    finally:
        sb.close()


def test_sandbox_python_stdin():
    with Sandbox.create(SandboxAttr(language="python")) as sb:
        result = sb.run_code("import sys; print(sys.stdin.read().upper())", input=b"flame")
    assert result.text().strip() == "FLAME"


def test_sandbox_submit_code():
    with Sandbox.create(SandboxAttr(language="python")) as sb:
        futures = [sb.submit_code(f"print({i} * {i})") for i in range(4)]
        assert [future.result().text().strip() for future in futures] == ["0", "1", "4", "9"]


def test_sandbox_close_prevents_reopen():
    sb = Sandbox.create(SandboxAttr(language="python"))
    sandbox_id = sb.sandbox_id
    sb.close()

    with pytest.raises(FlameError) as exc:
        Sandbox.open(sandbox_id)
    assert exc.value.code == FlameErrorCode.INVALID_STATE

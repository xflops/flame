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
from flamepy.agent import open_session


def test_session_python_run_code():
    with open_session() as ssn:
        result = ssn.run_code("print(1 + 2)")
    assert result.text().strip() == "3"


def test_session_shell_run_code():
    with open_session(language="shell", runtime="bash") as ssn:
        result = ssn.run_code("echo hello")
    assert result.text().strip() == "hello"


def test_session_open_restores_attr():
    ssn = open_session(language="python", runtime="3.12")
    try:
        other = open_session(ssn_id=ssn.id)
        assert other.id == ssn.id
        assert other.attr.language == "python"
        assert other.attr.runtime == "3.12"
        assert other.run_code("print('ready')").text().strip() == "ready"
    finally:
        ssn.close()


def test_session_python_stdin():
    with open_session(language="python") as ssn:
        result = ssn.run_code("import sys; print(sys.stdin.read().upper())", input=b"flame")
    assert result.text().strip() == "FLAME"


def test_session_submit_code():
    with open_session(language="python") as ssn:
        futures = [ssn.submit_code(f"print({i} * {i})") for i in range(4)]
        assert [future.result().text().strip() for future in futures] == ["0", "1", "4", "9"]


def test_session_close_prevents_reopen():
    ssn = open_session(language="python")
    ssn_id = ssn.id
    ssn.close()

    with pytest.raises(FlameError) as exc:
        open_session(ssn_id=ssn_id)
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT
    assert "not open" in str(exc.value).lower()

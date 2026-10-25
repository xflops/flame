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

import os
import shutil

import flamepy
import pytest
from flamepy import FlameError, FlameErrorCode
from flamepy.agent import open_session


def _env_flag(name):
    return os.environ.get(name, "").strip().lower() in ("1", "true", "yes")


@pytest.fixture(scope="module", autouse=True)
def flmbox_env():
    """Agent sessions run on the flmbox application, like sandboxes.

    Skip unless flmbox is registered and runsc is available. With
    FLAME_E2E_REQUIRE_SANDBOX=1 a missing prerequisite fails instead; with
    FLAME_E2E_REMOTE_RUNSC=1 runsc lives in the executor manager, so it is not
    looked up locally.
    """
    required = _env_flag("FLAME_E2E_REQUIRE_SANDBOX")

    def unavailable(reason):
        (pytest.fail if required else pytest.skip)(reason)

    try:
        app = flamepy.get_application("flmbox")
    except Exception:
        app = None
    if app is None:
        unavailable("flmbox application is not registered")
    if not _env_flag("FLAME_E2E_REMOTE_RUNSC") and shutil.which("runsc") is None:
        unavailable("runsc is not on PATH")


@pytest.fixture(scope="module")
def has_python():
    """Skip the Python cases when the default sandbox image has no python3."""
    with open_session() as ssn:
        if ssn.run_code("command -v python3").exit_code != 0:
            pytest.skip("the sandbox image has no python3")


def test_session_python_run_code(has_python):
    with open_session() as ssn:
        result = ssn.run_code("print(1 + 2)", language="python")
    assert result.ok
    assert result.stdout_text().strip() == "3"


def test_session_shell_run_code():
    with open_session() as ssn:
        result = ssn.run_code("echo hello")
    assert result.stdout_text().strip() == "hello"


def test_session_image_runs_python():
    """A session can choose its sandbox image; set FLAME_E2E_AGENT_IMAGE to an image with python3."""
    image = os.environ.get("FLAME_E2E_AGENT_IMAGE", "").strip()
    if not image:
        pytest.skip("FLAME_E2E_AGENT_IMAGE is not set")
    with open_session(image=image) as ssn:
        assert ssn.attr.image == image
        result = ssn.run_code("print(1 + 2)", language="python")
    assert result.ok, result.stderr_text()
    assert result.stdout_text().strip() == "3"


def test_session_failing_script_is_a_result():
    with open_session() as ssn:
        result = ssn.run_code("echo oops >&2; exit 3")
    assert result.exit_code == 3
    assert not result.ok
    assert result.stderr_text().strip() == "oops"


def test_session_open_restores_attr():
    ssn = open_session()
    try:
        other = open_session(ssn_id=ssn.id)
        assert other.id == ssn.id
        assert other.attr.image is None
        assert other.run_code("echo ready").stdout_text().strip() == "ready"
    finally:
        ssn.close()


def test_session_python_stdin(has_python):
    with open_session() as ssn:
        result = ssn.run_code("import sys; print(sys.stdin.read().upper())", language="python", input=b"flame")
    assert result.stdout_text().strip() == "FLAME"


def test_session_submit_code():
    with open_session() as ssn:
        futures = [ssn.submit_code(f"echo $(({i} * {i}))") for i in range(4)]
        assert [future.result().stdout_text().strip() for future in futures] == ["0", "1", "4", "9"]


def test_session_close_prevents_reopen():
    ssn = open_session()
    ssn_id = ssn.id
    ssn.close()

    with pytest.raises(FlameError) as exc:
        open_session(ssn_id=ssn_id)
    assert exc.value.code == FlameErrorCode.INVALID_ARGUMENT
    assert "not open" in str(exc.value).lower()

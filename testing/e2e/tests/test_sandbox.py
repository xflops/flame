"""
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.

E2E tests for suspending and resuming sessions (RFE601).

This module tests:
- Suspending an idle session and resuming it
- Suspending a session while a task is running
- Tasks submitted to a suspended session stay pending until the resume
- Rejected transitions (suspend a non-open session, resume a closed session)
- Closing a suspended session
- The flmctl suspend/resume commands
- The flmbox sandbox application through flamepy.agent.Sandbox (#605): commands,
  code, files, suspend/resume and close. These classes are
  skipped when flmbox is not registered or runsc is not installed, unless CI asks
  for them (see the environment variables below).

Environment variables for the sandbox classes:
- FLAME_E2E_REQUIRE_SANDBOX=1: a missing prerequisite fails instead of skipping.
  CI sets it, so the runtime tests run or fail and never skip silently.
- FLAME_E2E_REMOTE_RUNSC=1: runsc runs inside the executor manager (Kubernetes),
  not on the machine running pytest, so the local runsc check is not made.

suspend_session only signals: the session is returned in SUSPENDING and the
scheduler moves it to SUSPENDED in a later cycle, so the tests poll for states.
"""

import os
import shutil
import subprocess
import time

import flamepy
import pytest
from flamepy import FlameError, FlameErrorCode, SessionState, TaskState

try:
    from flamepy.agent import Sandbox
except ImportError:  # An SDK without flamepy.agent.Sandbox: the sandbox classes skip.
    Sandbox = None

from e2e.api import TestRequest
from e2e.helpers import deserialize_response, invoke_task, serialize_request
from tests.utils import deploy_e2e_application, random_string

FLM_SUSPEND_APP = f"flme2e-suspend-{random_string(6)}"

STATE_TIMEOUT = 60.0
TASK_TIMEOUT = 90.0
POLL_INTERVAL = 0.2


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    """Deploy the E2E application and clean up its sessions afterwards."""
    deploy_e2e_application(FLM_SUSPEND_APP)

    yield

    for sess in flamepy.list_sessions():
        if sess.application != FLM_SUSPEND_APP:
            continue
        try:
            flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(FLM_SUSPEND_APP)


@pytest.fixture
def session():
    """A fresh session that is closed (from any state) after the test."""
    sess = flamepy.create_session(
        application=FLM_SUSPEND_APP,
        session_id=f"suspend-{random_string(8)}",
    )
    yield sess
    try:
        flamepy.close_session(sess.id)
    except Exception:
        pass


def _state(session_id):
    return flamepy.get_session(session_id).state


def _wait_for_state(session_id, expected, timeout=STATE_TIMEOUT):
    """Poll until the session reaches `expected`; fail with the last observed state."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = _state(session_id)
        if last == expected:
            return
        time.sleep(POLL_INTERVAL)
    pytest.fail(f"Session {session_id} did not reach {expected.name} within {timeout}s; last state: {last.name if last is not None else None}")


def _wait_for_task(session, task_id, expected=TaskState.SUCCEED, timeout=TASK_TIMEOUT):
    """Poll until the task reaches `expected`; fail with the last observed state."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = session.get_task(task_id)
        if last.state == expected:
            return last
        if last.state in (TaskState.FAILED, TaskState.CANCELLED) and last.state != expected:
            pytest.fail(f"Task {task_id} ended in {last.state.name}: {last.events}")
        time.sleep(POLL_INTERVAL)
    pytest.fail(f"Task {task_id} did not reach {expected.name} within {timeout}s; last state: {last.state.name if last is not None else None}")


def _bound_executors(session_id):
    return [e for e in flamepy.list_executors() if e.status.session_id == session_id]


def _wait_for_no_executors(session_id, timeout=STATE_TIMEOUT):
    deadline = time.monotonic() + timeout
    bound = _bound_executors(session_id)
    while bound and time.monotonic() < deadline:
        time.sleep(POLL_INTERVAL)
        bound = _bound_executors(session_id)
    assert not bound, f"Session {session_id} still holds {len(bound)} executor(s)"


def _submit(session, text, sleep_ms=0):
    return session.create_task(serialize_request(TestRequest(input=text, sleep_ms=sleep_ms)))


def _output(task):
    return deserialize_response(task.output).output


def _assert_rejected(fn, *args):
    with pytest.raises(FlameError) as excinfo:
        fn(*args)
    return excinfo.value


class TestSuspendResume:
    """Suspend and resume of an idle session."""

    def test_suspend_idle_session_and_resume(self, session):
        """An idle session suspends, releases its executors, resumes and runs tasks."""
        # Warm up so that an executor is bound before the suspend.
        assert invoke_task(session, TestRequest(input="warmup")).output == "warmup"

        suspended = flamepy.suspend_session(session.id)
        assert suspended.state in (SessionState.SUSPENDING, SessionState.SUSPENDED)

        _wait_for_state(session.id, SessionState.SUSPENDED)
        _wait_for_no_executors(session.id)

        resumed = flamepy.resume_session(session.id)
        assert resumed.state == SessionState.OPEN
        assert _state(session.id) == SessionState.OPEN

        assert invoke_task(session, TestRequest(input="after-resume")).output == "after-resume"

    def test_suspend_session_without_tasks(self, session):
        """A session that never ran a task can be suspended and resumed."""
        flamepy.suspend_session(session.id)
        _wait_for_state(session.id, SessionState.SUSPENDED)
        assert _bound_executors(session.id) == []

        flamepy.resume_session(session.id)
        assert _state(session.id) == SessionState.OPEN
        assert invoke_task(session, TestRequest(input="hello")).output == "hello"

    def test_session_methods(self, session):
        """Session.suspend() and Session.resume() mirror the module functions."""
        session.suspend()
        _wait_for_state(session.id, SessionState.SUSPENDED)

        session.resume()
        assert _state(session.id) == SessionState.OPEN

    def test_suspend_waits_for_running_task(self, session):
        """A running task finishes first; the session is SUSPENDED only afterwards."""
        task = _submit(session, "running", sleep_ms=3000)

        # Wait for the task to start so the suspend really overlaps it.
        deadline = time.monotonic() + TASK_TIMEOUT
        while session.get_task(task.id).state == TaskState.PENDING:
            assert time.monotonic() < deadline, "Task never left PENDING"
            time.sleep(POLL_INTERVAL)

        flamepy.suspend_session(session.id)

        # While the task runs the session must not be SUSPENDED.
        if session.get_task(task.id).state == TaskState.RUNNING:
            assert _state(session.id) != SessionState.SUSPENDED

        done = _wait_for_task(session, task.id)
        assert _output(done) == "running"

        _wait_for_state(session.id, SessionState.SUSPENDED)
        _wait_for_no_executors(session.id)

        flamepy.resume_session(session.id)
        assert _state(session.id) == SessionState.OPEN
        assert invoke_task(session, TestRequest(input="again")).output == "again"

    def test_tasks_stay_pending_until_resume(self, session):
        """Tasks submitted to a suspended session wait for the resume and keep their order."""
        flamepy.suspend_session(session.id)
        _wait_for_state(session.id, SessionState.SUSPENDED)

        tasks = [_submit(session, f"pending-{i}") for i in range(3)]
        ids = [t.id for t in tasks]
        assert len(set(ids)) == len(ids)

        # Give the scheduler a few cycles; nothing may run while suspended.
        time.sleep(3)
        assert _state(session.id) == SessionState.SUSPENDED
        for task_id in ids:
            assert session.get_task(task_id).state == TaskState.PENDING
        assert _bound_executors(session.id) == []

        flamepy.resume_session(session.id)

        for i, task_id in enumerate(ids):
            done = _wait_for_task(session, task_id)
            assert done.id == task_id
            assert _output(done) == f"pending-{i}"

    def test_tasks_submitted_while_suspending_run_after_resume(self, session):
        """Tasks submitted during SUSPENDING stay pending and run after the resume."""
        running = _submit(session, "running", sleep_ms=2000)
        deadline = time.monotonic() + TASK_TIMEOUT
        while session.get_task(running.id).state == TaskState.PENDING:
            assert time.monotonic() < deadline, "Task never left PENDING"
            time.sleep(POLL_INTERVAL)

        flamepy.suspend_session(session.id)
        queued = _submit(session, "queued")

        _wait_for_task(session, running.id)
        _wait_for_state(session.id, SessionState.SUSPENDED)
        assert session.get_task(queued.id).state == TaskState.PENDING

        flamepy.resume_session(session.id)
        assert _output(_wait_for_task(session, queued.id)) == "queued"


class TestSuspendRejections:
    """Transitions that the service must refuse or treat as no-ops."""

    def test_suspend_suspended_session_fails(self, session):
        flamepy.suspend_session(session.id)
        _wait_for_state(session.id, SessionState.SUSPENDED)

        err = _assert_rejected(flamepy.suspend_session, session.id)
        assert err.code == FlameErrorCode.INVALID_ARGUMENT
        assert _state(session.id) == SessionState.SUSPENDED

    def test_suspend_closed_session_fails(self, session):
        flamepy.close_session(session.id)

        err = _assert_rejected(flamepy.suspend_session, session.id)
        assert err.code == FlameErrorCode.INVALID_ARGUMENT
        assert _state(session.id) == SessionState.CLOSED

    def test_resume_open_session_is_noop(self, session):
        resumed = flamepy.resume_session(session.id)
        assert resumed.state == SessionState.OPEN
        assert _state(session.id) == SessionState.OPEN
        assert invoke_task(session, TestRequest(input="still-open")).output == "still-open"

    def test_resume_closed_session_fails(self, session):
        flamepy.close_session(session.id)

        _assert_rejected(flamepy.resume_session, session.id)
        assert _state(session.id) == SessionState.CLOSED

    def test_suspend_unknown_session_fails(self):
        _assert_rejected(flamepy.suspend_session, f"no-such-session-{random_string(8)}")


class TestCloseSuspended:
    """Closing works from a suspended session."""

    def test_close_suspended_session(self, session):
        flamepy.suspend_session(session.id)
        _wait_for_state(session.id, SessionState.SUSPENDED)

        flamepy.close_session(session.id)
        assert _state(session.id) == SessionState.CLOSED

        # A closed session cannot be resumed.
        _assert_rejected(flamepy.resume_session, session.id)


class TestFlmctl:
    """The flmctl suspend and resume commands."""

    @staticmethod
    def _flmctl(*args):
        return subprocess.run(["flmctl", *args], capture_output=True, text=True, check=False, timeout=60)

    def test_flmctl_suspend_and_resume(self, session):
        if shutil.which("flmctl") is None:
            pytest.skip("flmctl is not on PATH")

        result = self._flmctl("suspend", session.id)
        assert result.returncode == 0, result.stderr or result.stdout
        _wait_for_state(session.id, SessionState.SUSPENDED)

        result = self._flmctl("resume", session.id)
        assert result.returncode == 0, result.stderr or result.stdout
        assert _state(session.id) == SessionState.OPEN

        assert invoke_task(session, TestRequest(input="via-flmctl")).output == "via-flmctl"

    def test_flmctl_suspend_unknown_session_fails(self):
        if shutil.which("flmctl") is None:
            pytest.skip("flmctl is not on PATH")

        result = self._flmctl("suspend", f"no-such-session-{random_string(8)}")
        assert result.returncode != 0


# --- flmbox sandboxes (#605) ---------------------------------------------------

FIRST_COMMAND_TIMEOUT = 180.0  # The first command of a node pulls the image.
COMMAND_TIMEOUT = 60.0
AFTER_RESUME_TIMEOUT = 90.0


def _env_flag(name):
    return os.environ.get(name, "").strip().lower() in ("1", "true", "yes")


@pytest.fixture(scope="module")
def flmbox_env():
    """Skip the sandbox tests unless flmbox is registered and runsc is available.

    With FLAME_E2E_REQUIRE_SANDBOX=1 a missing prerequisite fails instead. With
    FLAME_E2E_REMOTE_RUNSC=1 runsc is not looked up locally: it lives in the
    executor manager (Kubernetes), and a missing runsc shows up as a failed command.
    """
    required = _env_flag("FLAME_E2E_REQUIRE_SANDBOX")

    def unavailable(reason):
        (pytest.fail if required else pytest.skip)(reason)

    if Sandbox is None:
        unavailable("flamepy.agent.Sandbox is not available")
    try:
        app = flamepy.get_application("flmbox")
    except Exception:
        app = None
    if app is None:
        unavailable("flmbox application is not registered")
    if not _env_flag("FLAME_E2E_REMOTE_RUNSC") and shutil.which("runsc") is None:
        unavailable("runsc is not on PATH")


@pytest.fixture
def make_sandbox(flmbox_env):
    """Factory of sandboxes with the default image; all are closed after the test.

    The sandbox is warmed up with a first command, which may pull the image.
    """
    created = []

    def make():
        sandbox = Sandbox.create()
        created.append(sandbox)
        result = _run(sandbox, "echo ready", wait=FIRST_COMMAND_TIMEOUT)
        assert result.exit_code == 0, result.stderr_text()
        return sandbox

    yield make

    for sandbox in created:
        try:
            sandbox.close()
        except Exception:
            pass


@pytest.fixture
def sandbox(make_sandbox):
    return make_sandbox()


def _run(sandbox, cmd, wait=COMMAND_TIMEOUT, **kwargs):
    return sandbox.submit(cmd, **kwargs).result(timeout=wait)


def _run_code(sandbox, code, wait=COMMAND_TIMEOUT, **kwargs):
    return sandbox.submit_code(code, **kwargs).result(timeout=wait)


class TestSandboxCommands:
    """Commands and code run inside a sandbox."""

    def test_run_echo(self, sandbox):
        result = _run(sandbox, "echo hello")
        assert result.exit_code == 0
        assert result.stdout_text() == "hello\n"
        assert result.stderr == b""

    def test_nonzero_exit_is_a_normal_result(self, sandbox):
        result = _run(sandbox, "sh -c 'echo err >&2; exit 3'")
        assert result.exit_code == 3
        assert result.stderr_text() == "err\n"
        assert result.stdout == b""

    def test_run_code_shell(self, sandbox):
        result = _run_code(sandbox, "echo $((6 * 7))", language="shell")
        assert result.exit_code == 0
        assert result.stdout_text().strip() == "42"

    def test_run_code_python(self, sandbox):
        if _run(sandbox, "command -v python3").exit_code != 0:
            pytest.skip("the sandbox image has no python3")
        result = _run_code(sandbox, "print(6 * 7)", language="python")
        assert result.exit_code == 0
        assert result.stdout_text().strip() == "42"

    def test_stdin_input(self, sandbox):
        result = _run_code(sandbox, "cat", language="shell", input=b"from stdin")
        assert result.exit_code == 0
        assert result.stdout == b"from stdin"

    def test_cwd_and_envs(self, sandbox):
        result = _run(sandbox, "pwd; echo $FLM_E2E_VALUE", cwd="/tmp", envs={"FLM_E2E_VALUE": "abc"})
        assert result.exit_code == 0
        assert result.stdout_text().split() == ["/tmp", "abc"]

    def test_timeout_exit_code(self, sandbox):
        started = time.monotonic()
        result = _run(sandbox, "sleep 30", timeout=2)
        assert result.exit_code == 124
        assert time.monotonic() - started < 25

    def test_submit_future(self, sandbox):
        future = sandbox.submit("echo from-future")
        result = future.result(timeout=COMMAND_TIMEOUT)
        assert result.exit_code == 0
        assert result.stdout_text() == "from-future\n"


class TestSandboxFilesystem:
    """The filesystem belongs to one sandbox."""

    def test_file_persists_between_runs_of_one_sandbox(self, sandbox):
        assert _run(sandbox, "echo data > /tmp/flm-e2e-file").exit_code == 0
        result = _run(sandbox, "cat /tmp/flm-e2e-file")
        assert result.exit_code == 0
        assert result.stdout_text() == "data\n"

    def test_file_is_not_visible_in_another_sandbox(self, make_sandbox):
        first = make_sandbox()
        second = make_sandbox()
        assert _run(first, "echo data > /tmp/flm-e2e-private").exit_code == 0

        assert _run(second, "test -e /tmp/flm-e2e-private").exit_code != 0
        assert _run(first, "test -e /tmp/flm-e2e-private").exit_code == 0


class TestSandboxLifecycle:
    """Suspend, resume and close."""

    def test_suspend_and_resume(self, sandbox):
        assert sandbox.state == "open"
        assert _run(sandbox, "echo data > /tmp/flm-e2e-before-suspend").exit_code == 0

        sandbox.suspend()
        assert sandbox.state == "suspended"

        with pytest.raises(FlameError) as excinfo:
            sandbox.run("echo nope")
        assert excinfo.value.code == FlameErrorCode.INVALID_STATE

        sandbox.resume()
        assert sandbox.state == "open"

        result = _run(sandbox, "echo after-resume", wait=AFTER_RESUME_TIMEOUT)
        assert result.exit_code == 0
        assert result.stdout_text() == "after-resume\n"

        # Phase 1 keeps no state across a suspend: the resumed sandbox starts fresh.
        # This assertion changes when checkpoint/restore persistence lands (#605).
        assert _run(sandbox, "test -e /tmp/flm-e2e-before-suspend").exit_code != 0

    def test_close(self, sandbox):
        sandbox_id = sandbox.id
        sandbox.close()
        assert flamepy.get_session(sandbox_id).state == SessionState.CLOSED

        with pytest.raises(FlameError):
            sandbox.run("echo nope")

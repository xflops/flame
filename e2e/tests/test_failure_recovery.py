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

E2E tests for task failure handling and recovery scenarios.

This module tests:
- Task failure propagation
- Error message recording
- Partial failures in parallel execution
- Session behavior after task failures
- Recovery after failures
"""

import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import flamepy
import pytest
from flamepy import TaskState

from e2e.api import TestRequest
from e2e.helpers import FAIL_INPUT, invoke_task, serialize_request
from tests.utils import random_string

FLM_TEST_SVC_APP = "flme2e-svc"
FLM_ERROR_SVC_APP = "flme2e-error-svc"
FLM_ENTER_FAILURE_APP = "flme2e-enter-failure-svc"
ENTER_FAILURE_SERVICE_ENTRYPOINT = "import flamepy; from e2e.error_svc import EnterFailureTestService; flamepy.run(EnterFailureTestService())"
TASK_FAILED_EVENT_CODE = int(TaskState.FAILED)
SESSION_BIND_FAILED_EVENT_CODE = 1001


def _wait_for_terminal_task(session, task_id):
    for task_update in session.watch_task(task_id):
        if task_update.is_failed() or task_update.is_completed():
            return task_update
    pytest.fail(f"Task {task_id} watcher ended before terminal state")


def _create_failing_basic_task(session):
    task = session.create_task(serialize_request(TestRequest(input=FAIL_INPUT)))
    task_update = _wait_for_terminal_task(session, task.id)
    assert task_update.state == TaskState.FAILED
    return task_update


def _wait_for_session_event(session_id, event_code, timeout_seconds=60):
    deadline = time.monotonic() + timeout_seconds
    last_events = []

    while time.monotonic() < deadline:
        session = flamepy.get_session(session_id)
        last_events = session.events
        matched_events = [event for event in last_events if event.code == event_code]
        if matched_events:
            return session, matched_events
        time.sleep(0.5)

    pytest.fail(f"Session {session_id} did not record event {event_code}; last events: {[(e.code, e.message) for e in last_events]}")


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    """Setup test environment with test services."""
    # Register the basic test service
    flamepy.register_application(
        FLM_TEST_SVC_APP,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG", "PYTHONPATH": "/opt/e2e/src"},
            arguments=["src/e2e/basic_svc.py", "src/e2e/api.py"],
            installer="python",
        ),
    )

    # Register the error service for failure tests
    flamepy.register_application(
        FLM_ERROR_SVC_APP,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG", "PYTHONPATH": "/opt/e2e/src"},
            arguments=["src/e2e/error_svc.py"],
            installer="python",
        ),
    )

    flamepy.register_application(
        FLM_ENTER_FAILURE_APP,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={
                "FLAME_LOG_LEVEL": "DEBUG",
                "PYTHONPATH": "/opt/e2e/src",
            },
            arguments=["-c", ENTER_FAILURE_SERVICE_ENTRYPOINT],
            installer="python",
        ),
    )

    yield

    # Clean up sessions owned by this module before unregistering.
    sessions = flamepy.list_sessions()
    for sess in sessions:
        if sess.application not in {FLM_TEST_SVC_APP, FLM_ERROR_SVC_APP, FLM_ENTER_FAILURE_APP}:
            continue
        try:
            flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(FLM_TEST_SVC_APP)
    flamepy.unregister_application(FLM_ERROR_SVC_APP)
    flamepy.unregister_application(FLM_ENTER_FAILURE_APP)


# =============================================================================
# Task Failure Tests
# =============================================================================


class TestTaskFailure:
    """Tests for task failure handling."""

    def test_task_failure_recorded_in_state(self):
        """Test that task failure is recorded in task state."""
        session = flamepy.create_session(application=FLM_ERROR_SVC_APP, common_data=None)

        try:
            # Create a task that will fail
            input_data = b"test input"
            task = session.create_task(input_data)

            failed_task = _wait_for_terminal_task(session, task.id)

            # Verify the task failed
            assert failed_task is not None, "Task should have failed"
            assert failed_task.state == TaskState.FAILED

        finally:
            session.close()

    def test_task_failure_has_error_events(self):
        """Test that failed tasks have error events recorded."""
        session = flamepy.create_session(application=FLM_ERROR_SVC_APP, common_data=None)

        try:
            input_data = b"test input"
            task = session.create_task(input_data)

            task_update = _wait_for_terminal_task(session, task.id)
            assert task_update.state == TaskState.FAILED

            # Get the task again to ensure we have the latest events
            refreshed_task = session.get_task(task.id)

            # Check events for error information
            error_events = [e for e in refreshed_task.events if e.code == TASK_FAILED_EVENT_CODE]
            assert len(error_events) > 0, f"Should have at least one FAILED event, got events: {[(e.code, e.message) for e in refreshed_task.events]}"

            # Check that the error message is present
            error_message = error_events[0].message
            assert error_message is not None, "Error event should have a message"

        finally:
            session.close()

    def test_session_continues_after_task_failure(self):
        """Test that session can continue processing tasks after a failure."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            _create_failing_basic_task(session)

            # Session should still be usable for more tasks
            request = TestRequest(input="success_after_failure")
            response = invoke_task(session, request)
            assert response.output == "success_after_failure"

            # Verify session counters
            updated_session = flamepy.get_session(session.id)
            assert updated_session.failed >= 1
            assert updated_session.succeed >= 1

        finally:
            session.close()


# =============================================================================
# Session Bind Failure Tests
# =============================================================================


class TestSessionBindFailureRecovery:
    """Tests for session bind failure recovery."""

    def test_on_session_enter_failure_records_session_event(self):
        """Test that on_session_enter failure is reported as a session event."""
        session_id = f"test-enter-failure-{random_string(8)}"
        session = flamepy.create_session(
            application=FLM_ENTER_FAILURE_APP,
            session_id=session_id,
            common_data=None,
            max_instances=1,
        )

        try:
            session.create_task(b"trigger session bind")

            current_session, bind_failed_events = _wait_for_session_event(
                session_id,
                SESSION_BIND_FAILED_EVENT_CODE,
            )

            assert current_session.state == flamepy.SessionState.OPEN
            event_messages = [event.message or "" for event in bind_failed_events]
            assert any("failed to bind session" in message for message in event_messages)
            assert any("intentional session enter failure" in message for message in event_messages)

        finally:
            flamepy.close_session(session_id)


# =============================================================================
# Partial Failure Tests
# =============================================================================


class TestPartialFailures:
    """Tests for partial failure scenarios in parallel execution."""

    def test_some_tasks_fail_others_succeed(self):
        """Test mixed success/failure in parallel task execution."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            inputs = ["task_0", FAIL_INPUT, "task_2", FAIL_INPUT, "task_4"]

            def run_task(input_value):
                try:
                    response = invoke_task(session, TestRequest(input=input_value))
                    return ("succeed", response.output)
                except Exception as exc:
                    return ("failed", str(exc))

            results = []
            with ThreadPoolExecutor(max_workers=len(inputs)) as executor:
                futures = [executor.submit(run_task, input_value) for input_value in inputs]
                for future in as_completed(futures):
                    results.append(future.result())

            successful_outputs = sorted(value for state, value in results if state == "succeed")
            failures = [value for state, value in results if state == "failed"]

            assert successful_outputs == ["task_0", "task_2", "task_4"]
            assert len(failures) == 2

            updated_session = flamepy.get_session(session.id)
            assert updated_session.succeed >= 3
            assert updated_session.failed >= 2

        finally:
            session.close()

    def test_failure_does_not_affect_other_tasks(self):
        """Test that one task's failure doesn't affect others in the session."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            # Run several successful tasks
            for i in range(3):
                request = TestRequest(input=f"before_failure_{i}")
                response = invoke_task(session, request)
                assert response.output == f"before_failure_{i}"

            mid_session = flamepy.get_session(session.id)
            success_count_before = mid_session.succeed

            _create_failing_basic_task(session)

            # Run more successful tasks
            for i in range(3):
                request = TestRequest(input=f"after_check_{i}")
                response = invoke_task(session, request)
                assert response.output == f"after_check_{i}"

            # Verify success count increased
            final_session = flamepy.get_session(session.id)
            assert final_session.succeed > success_count_before
            assert final_session.failed >= 1

        finally:
            session.close()


# =============================================================================
# Session State After Failures Tests
# =============================================================================


class TestSessionStateAfterFailures:
    """Tests for session state management after failures."""

    def test_session_remains_open_after_task_failure(self):
        """Test that session remains OPEN after task failures."""
        session = flamepy.create_session(application=FLM_ERROR_SVC_APP, common_data=None)

        try:
            # Create a task that will fail
            input_data = b"test input"
            task = session.create_task(input_data)

            task_update = _wait_for_terminal_task(session, task.id)
            assert task_update.state == TaskState.FAILED

            # Session should still be OPEN
            current_session = flamepy.get_session(session.id)
            assert current_session.state == flamepy.SessionState.OPEN

        finally:
            session.close()

    def test_session_failed_counter_increments(self):
        """Test that session's failed counter increments on task failure."""
        session = flamepy.create_session(application=FLM_ERROR_SVC_APP, common_data=None)

        try:
            # Get initial failed count
            initial_session = flamepy.get_session(session.id)
            initial_failed = initial_session.failed

            # Create a task that will fail
            input_data = b"test input"
            task = session.create_task(input_data)

            task_update = _wait_for_terminal_task(session, task.id)
            assert task_update.state == TaskState.FAILED

            # Failed count should have increased
            updated_session = flamepy.get_session(session.id)
            assert updated_session.failed > initial_failed

        finally:
            session.close()

    def test_session_can_close_with_failed_tasks(self):
        """Test that session can be closed even with failed tasks."""
        session = flamepy.create_session(application=FLM_ERROR_SVC_APP, common_data=None)

        # Create a task that will fail
        input_data = b"test input"
        task = session.create_task(input_data)

        task_update = _wait_for_terminal_task(session, task.id)
        assert task_update.state == TaskState.FAILED

        # Close should work
        session.close()

        # Verify closed
        closed_session = flamepy.get_session(session.id)
        assert closed_session.state == flamepy.SessionState.CLOSED


# =============================================================================
# Recovery After Failure Tests
# =============================================================================


class TestRecoveryAfterFailure:
    """Tests for recovery patterns after task failures."""

    def test_retry_pattern_with_new_task(self):
        """Test retrying a failed operation by creating a new task."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            _create_failing_basic_task(session)

            retry_request = TestRequest(input="retry_attempt")
            retry_response = invoke_task(session, retry_request)
            assert retry_response.output == "retry_attempt"

            updated_session = flamepy.get_session(session.id)
            assert updated_session.failed >= 1
            assert updated_session.succeed >= 1

        finally:
            session.close()

    def test_new_session_after_failures(self):
        """Test creating a new session after previous session had failures."""
        # First session with error service
        session1 = flamepy.create_session(application=FLM_ERROR_SVC_APP, common_data=None)

        try:
            # Create a failing task
            input_data = b"test input"
            task = session1.create_task(input_data)

            task_update = _wait_for_terminal_task(session1, task.id)
            assert task_update.state == TaskState.FAILED

        finally:
            session1.close()

        # Second session with working service should work fine
        session2 = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            request = TestRequest(input="new_session_success")
            response = invoke_task(session2, request)
            assert response.output == "new_session_success"

        finally:
            session2.close()

    def test_multiple_failures_then_recovery(self):
        """Test that session can recover from multiple consecutive failures."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            _create_failing_basic_task(session)
            _create_failing_basic_task(session)

            for i in range(3):
                request = TestRequest(input=f"recovery_test_{i}")
                response = invoke_task(session, request)
                assert response.output == f"recovery_test_{i}"

            final_session = flamepy.get_session(session.id)
            assert final_session.succeed >= 3
            assert final_session.failed >= 2

        finally:
            session.close()


# =============================================================================
# Task Watch Timeout Tests
# =============================================================================


class TestTaskWatchTimeout:
    """Tests for task watch timeout behavior."""

    def test_watch_task_returns_updates(self):
        """Test that watch_task returns task updates."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            request = TestRequest(input="watch_test")
            request_bytes = _serialize_request(request)
            task = session.create_task(request_bytes)

            # Watch for updates
            updates = []
            watcher = session.watch_task(task.id)
            for task_update in watcher:
                updates.append(task_update.state)
                if task_update.is_completed() or task_update.is_failed():
                    break

            # Should have received at least one update
            assert len(updates) >= 1
            # Final state should be SUCCEED
            assert updates[-1] == TaskState.SUCCEED

        finally:
            session.close()

    def test_watch_completed_task(self):
        """Test watching a task that has already completed."""
        session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

        try:
            # Run task to completion
            request = TestRequest(input="already_done")
            response = invoke_task(session, request)
            assert response.output == "already_done"

            # Get the task ID
            tasks = list(session.list_tasks())
            assert len(tasks) >= 1

            # Watch completed task - should return final state immediately
            watcher = session.watch_task(tasks[0].id)
            for task_update in watcher:
                assert task_update.state == TaskState.SUCCEED
                break

        finally:
            session.close()


# =============================================================================
# Shim Selection Tests (from test_shim_selection.py)
# =============================================================================

"""
E2E tests for shim selection feature (Issue #379).

These tests verify the shim selection logic that ensures applications are
matched with executors that support the required shim type (Host or Wasm).

Test scenarios:
1. Positive case: App with default shim (Host) matches executor with shim "Host"
2. Negative case: App with shim "Wasm" fails to match executor with shim "Host"
3. Default behavior: App without explicit shim defaults to "Host"
"""

FLM_SHIM_TEST_APP = "flme2e-shim-test"


@pytest.fixture(scope="class")
def setup_shim_test_app():
    """Setup test application for shim selection tests."""
    flamepy.register_application(
        FLM_SHIM_TEST_APP,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG", "PYTHONPATH": "/opt/e2e/src"},
            arguments=["src/e2e/basic_svc.py", "src/e2e/api.py"],
            installer="python",
        ),
    )

    yield

    sessions = flamepy.list_sessions()
    for sess in sessions:
        if sess.application == FLM_SHIM_TEST_APP:
            flamepy.close_session(sess.id)

    flamepy.unregister_application(FLM_SHIM_TEST_APP)


class TestShimSelectionPositive:
    """Test positive case: App with Host shim matches Host executor."""

    def test_host_app_on_host_executor(self, setup_shim_test_app):
        """Test that an application with default shim (Host) successfully runs on an executor with Host shim."""
        session = flamepy.create_session(application=FLM_SHIM_TEST_APP, common_data=None)

        try:
            assert session is not None
            assert session.application == FLM_SHIM_TEST_APP
            assert session.state == flamepy.SessionState.OPEN

            request = TestRequest(input="shim_test_input")
            response = invoke_task(session, request)

            assert response.output == "shim_test_input"

        finally:
            session.close()


class TestShimSelectionNegative:
    """Test negative case: App with Wasm shim fails to match Host executor."""

    def test_wasm_app_on_host_executor_stays_pending(self, setup_shim_test_app):
        """Test that an application with Wasm shim cannot be scheduled on an executor with Host shim."""
        import time

        app_name = "flmtest-shim-wasm"

        try:
            flamepy.register_application(
                app_name,
                flamepy.ApplicationAttributes(
                    shim=flamepy.Shim.WASM,
                    command="/bin/echo",
                    arguments=["hello", "from", "wasm", "shim"],
                    description="Test app for Wasm shim selection",
                ),
            )

            session = flamepy.create_session(application=app_name)

            try:
                assert session is not None
                assert session.id is not None

                deadline = time.time() + 3
                session_status = None
                while time.time() < deadline:
                    sessions = flamepy.list_sessions()
                    session_status = next((s for s in sessions if s.id == session.id), None)
                    if session_status is not None and session_status.state == flamepy.SessionState.OPEN:
                        break
                    time.sleep(0.5)

                assert session_status is not None
                assert session_status.state == flamepy.SessionState.OPEN

            finally:
                session.close()

        finally:
            try:
                flamepy.unregister_application(app_name)
            except Exception:
                pass

    def test_wasm_app_task_stays_pending(self, setup_shim_test_app):
        """Test that a task created for a Wasm application stays pending when only Host executors are available."""
        import time

        app_name = "flmtest-shim-wasm-task"

        try:
            flamepy.register_application(
                app_name,
                flamepy.ApplicationAttributes(
                    shim=flamepy.Shim.WASM,
                    command="/bin/echo",
                    arguments=["hello"],
                    description="Test app for Wasm shim task pending",
                ),
            )

            session = flamepy.create_session(application=app_name)

            try:
                input_data = b"test input for wasm"
                task = session.create_task(input_data)

                deadline = time.time() + 3
                task_status = session.get_task(task.id)
                while time.time() < deadline:
                    task_status = session.get_task(task.id)
                    assert task_status.state == flamepy.TaskState.PENDING, f"Task should remain PENDING when no compatible executor available, got {task_status.state}"
                    time.sleep(0.5)

                assert task_status.state == flamepy.TaskState.PENDING, f"Task should remain PENDING when no compatible executor available, got {task_status.state}"

            finally:
                session.close()

        finally:
            try:
                flamepy.unregister_application(app_name)
            except Exception:
                pass


class TestShimSelectionDefault:
    """Test default behavior: App without shim defaults to Host."""

    def test_app_without_shim_defaults_to_host(self, setup_shim_test_app):
        """Test that an application registered without explicit shim defaults to Host and successfully matches Host executors."""
        session = flamepy.create_session(application=FLM_SHIM_TEST_APP, common_data=None)

        try:
            assert session is not None
            assert session.application == FLM_SHIM_TEST_APP

            request = TestRequest(input="default_shim_test")
            response = invoke_task(session, request)

            assert response.output == "default_shim_test"

        finally:
            session.close()

    def test_existing_apps_work_without_shim(self, setup_shim_test_app):
        """Test that pre-existing applications (flmexec, flmping, flmrun) continue to work after the shim selection feature is added."""
        apps = flamepy.list_applications()

        app_names = [app.name for app in apps]
        standard_apps = ["flmexec", "flmping", "flmrun"]

        found_app = None
        for app_name in standard_apps:
            if app_name in app_names:
                found_app = app_name
                break

        assert found_app is not None, f"At least one standard app should exist: {standard_apps}"

        session = flamepy.create_session(application=found_app)

        try:
            assert session is not None
            assert session.application == found_app
            assert session.state == flamepy.SessionState.OPEN

        finally:
            session.close()


class TestShimSelectionIntegration:
    """Integration tests for shim selection with real workloads."""

    def test_host_shim_task_execution(self, setup_shim_test_app):
        """Test a complete workflow with Host shim."""
        session = flamepy.create_session(application=FLM_SHIM_TEST_APP, common_data=None)

        try:
            request = TestRequest(input="integration_test_input")
            response = invoke_task(session, request)

            assert response.output == "integration_test_input"
            assert response.service_state is not None
            assert response.service_state["task_count"] == 1

        finally:
            session.close()


# =============================================================================
# Helper Functions
# =============================================================================


def _serialize_request(request: TestRequest) -> bytes:
    """Serialize a TestRequest to bytes using JSON."""
    import json
    from dataclasses import asdict

    request_dict = asdict(request)
    return json.dumps(request_dict).encode("utf-8")

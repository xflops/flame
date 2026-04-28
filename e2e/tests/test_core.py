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
"""

import flamepy
import pytest

from e2e.api import TestContext, TestRequest
from e2e.helpers import (
    invoke_task,
    serialize_common_data,
)
from tests.utils import random_string

FLM_TEST_SVC_APP = "flme2e-svc"


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    """Setup test environment with BasicTestService."""
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

    yield

    # Clean up all sessions before unregistering
    sessions = flamepy.list_sessions()
    for sess in sessions:
        try:
            flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(FLM_TEST_SVC_APP)


def test_basic_service_invoke():
    """Test basic invocation of BasicTestService."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    input_data = random_string()
    request = TestRequest(input=input_data)

    response = invoke_task(session, request)

    assert response.output == input_data
    assert response.common_data is None
    assert response.service_state is not None
    assert response.service_state["task_count"] == 1
    assert response.service_state["session_enter_count"] == 1

    session.close()


def test_task_context_info():
    """Test that task context information is correctly returned."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    input_data = random_string()
    request = TestRequest(
        input=input_data,
        request_task_context=True,
    )

    response = invoke_task(session, request)

    # Check basic response
    assert response.output == input_data

    # Check task context is present
    assert response.task_context is not None
    assert response.task_context.task_id is not None
    assert response.task_context.session_id == session.id
    assert response.task_context.has_input is True
    assert response.task_context.input_type == "TestRequest"

    session.close()


def test_session_context_info():
    """Test that session context information is correctly returned."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    request = TestRequest(
        input="test",
        request_session_context=True,
    )

    response = invoke_task(session, request)

    # Check session context is present
    assert response.session_context is not None
    assert response.session_context.session_id == session.id
    assert response.session_context.has_common_data is False
    assert response.session_context.common_data_type is None

    session.close()


def test_application_context_info():
    """Test that application context information is correctly returned."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    request = TestRequest(
        input="test",
        request_application_context=True,
    )

    response = invoke_task(session, request)

    # Check application context is present
    assert response.application_context is not None
    assert response.application_context.name == FLM_TEST_SVC_APP
    assert response.application_context.command == "python3"
    assert response.application_context.working_directory == "/opt/e2e"

    session.close()


def test_all_context_info():
    """Test that all context information is correctly returned together."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    input_data = random_string(16)
    request = TestRequest(
        input=input_data,
        request_task_context=True,
        request_session_context=True,
        request_application_context=True,
    )

    response = invoke_task(session, request)

    # Check output
    assert response.output == input_data

    # Check all contexts are present
    assert response.task_context is not None
    assert response.session_context is not None
    assert response.application_context is not None

    # Check task context details
    assert response.task_context.task_id is not None
    assert response.task_context.session_id == session.id

    # Check session context details
    assert response.session_context.session_id == session.id
    assert response.session_context.application is not None
    assert response.session_context.application.name == FLM_TEST_SVC_APP

    # Check application context details
    assert response.application_context.name == FLM_TEST_SVC_APP
    assert response.application_context.command == "python3"
    assert response.application_context.working_directory == "/opt/e2e"

    session.close()


def test_service_state_tracking():
    """Test that service state is maintained across multiple tasks."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    num_tasks = 5
    for i in range(1, num_tasks + 1):
        request = TestRequest(input=f"task_{i}")
        response = invoke_task(session, request)

        # Check service state increments
        assert response.service_state is not None
        assert response.service_state["task_count"] == i
        assert response.service_state["session_enter_count"] == 1
        assert response.service_state["session_leave_count"] == 0

    session.close()


def test_list_tasks():
    """Test that list_tasks returns all tasks in a session as an iterator."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    num_tasks = 5

    # Create multiple tasks
    for i in range(num_tasks):
        request = TestRequest(input=f"list_task_{i}")
        response = invoke_task(session, request)
        assert response.output == f"list_task_{i}"

    # Get all tasks using list_tasks iterator
    tasks = list(session.list_tasks())

    # Verify we got all tasks
    assert len(tasks) == num_tasks, f"Expected {num_tasks} tasks, got {len(tasks)}"

    # Verify each task has valid properties
    for task in tasks:
        assert task.id is not None
        assert task.session_id == session.id
        assert task.state is not None
        assert task.creation_time is not None
    session.close()


def test_common_data_without_context_request():
    """Test common data handling without requesting context info."""
    sys_context = random_string()
    test_context = TestContext(common_data=sys_context)
    common_data_bytes = serialize_common_data(test_context, FLM_TEST_SVC_APP)

    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=common_data_bytes)

    input_data = random_string()
    request = TestRequest(input=input_data)
    response = invoke_task(session, request)

    assert response.output == input_data
    assert response.common_data == sys_context

    session.close()


def test_common_data_with_session_context():
    """Test that common data information is correctly reported in session context."""
    common_data = TestContext(common_data=random_string())
    common_data_bytes = serialize_common_data(common_data, FLM_TEST_SVC_APP)

    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=common_data_bytes)

    request = TestRequest(
        input="test",
        request_session_context=True,
    )

    response = invoke_task(session, request)

    # Check session context reports common data
    assert response.session_context is not None
    assert response.session_context.has_common_data is True
    assert response.session_context.common_data_type == "TestContext"

    session.close()


def test_update_common_data():
    """Test updating common data through BasicTestService.

    Note: Since update_common_data() was removed from SessionContext,
    this test verifies that the service can read the update request,
    but the update won't persist across tasks. For persistent updates,
    recreate the session with new common_data.
    """
    sys_context = random_string()

    test_context = TestContext(common_data=sys_context)
    common_data_bytes = serialize_common_data(test_context, FLM_TEST_SVC_APP)

    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=common_data_bytes)

    _previous_common_data = sys_context
    for i in range(3):
        new_input_data = random_string()
        request = TestRequest(input=new_input_data, update_common_data=True)
        response = invoke_task(session, request)

        assert response.output == new_input_data
        # Note: The service returns the updated value in response (the new input),
        # but it doesn't persist. The next call will still see the original common_data.
        # So the response shows the "updated" value (which is just the input),
        # but the actual common_data in SessionContext remains unchanged.
        assert response.common_data == new_input_data

        # Verify that the original common_data is still in the session context
        # by checking it in the next iteration (if we request session context)
        if i < 2:  # Don't check on last iteration
            # Make a request that reads the session context to verify it's unchanged
            check_request = TestRequest(input="check", request_session_context=True)
            check_response = invoke_task(session, check_request)
            # The session context should still show the original common_data exists
            assert check_response.session_context.has_common_data is True

        # For this test, we expect the response to show the update was processed
        # but it won't persist to the next iteration
        _previous_common_data = new_input_data

    session.close()


def test_multiple_sessions_isolation():
    """Test that service state is isolated across different sessions."""
    # First session
    session1 = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    for i in range(3):
        request = TestRequest(input=f"session1_task_{i}")
        response = invoke_task(session1, request)
        assert response.service_state["task_count"] == i + 1

    session1.close()

    # Second session should reset state
    session2 = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    request = TestRequest(input="session2_task_1")
    response = invoke_task(session2, request)

    # Task count should be reset for new session
    assert response.service_state["task_count"] == 1
    assert response.service_state["session_enter_count"] == 1

    session2.close()


def test_context_info_selective_request():
    """Test that context info is only returned when explicitly requested."""
    session = flamepy.create_session(application=FLM_TEST_SVC_APP, common_data=None)

    # Request only task context
    request = TestRequest(
        input="test",
        request_task_context=True,
        request_session_context=False,
        request_application_context=False,
    )
    response = invoke_task(session, request)

    assert response.task_context is not None
    assert response.session_context is None
    assert response.application_context is None

    # Request only session context
    request = TestRequest(
        input="test",
        request_task_context=False,
        request_session_context=True,
        request_application_context=False,
    )
    response = invoke_task(session, request)

    assert response.task_context is None
    assert response.session_context is not None
    assert response.application_context is None

    # Request only application context
    request = TestRequest(
        input="test",
        request_task_context=False,
        request_session_context=False,
        request_application_context=True,
    )
    response = invoke_task(session, request)

    assert response.task_context is None
    assert response.session_context is None
    assert response.application_context is not None

    session.close()


def test_task_invoke_exception_handling():
    """Test that exceptions in on_task_invoke are properly handled and recorded."""
    flm_error_svc_app = "flme2e-error-svc"

    # Register the error service application
    flamepy.register_application(
        flm_error_svc_app,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG", "PYTHONPATH": "/opt/e2e/src"},
            arguments=["src/e2e/error_svc.py"],
            installer="python",
        ),
    )

    try:
        session = flamepy.create_session(application=flm_error_svc_app, common_data=None)

        # Create a task that will fail
        input_data = b"test input"
        task = session.create_task(input_data)

        # Watch the task to see its updates
        watcher = session.watch_task(task.id)

        failed_task = None
        for task_update in watcher:
            if task_update.is_failed():
                failed_task = task_update
                break
            elif task_update.is_completed():
                # Should not succeed
                pytest.fail("Task should have failed but it succeeded")

        # Verify the task failed
        assert failed_task is not None, "Task should have failed"
        assert failed_task.is_failed(), "Task state should be Failed"
        assert failed_task.state == flamepy.TaskState.FAILED, f"Task state should be FAILED, got {failed_task.state}"

        # Get the task again to ensure we have the latest events
        refreshed_task = session.get_task(task.id)

        # Verify error message is recorded in events

        # Check events from refreshed task
        error_events = [e for e in refreshed_task.events if e.code == flamepy.TaskState.FAILED or e.code == 3]
        assert len(error_events) > 0, f"Should have at least one FAILED event, got events: {[(e.code, e.message) for e in refreshed_task.events]}"

        # Check that the error message contains the exception message from Python service
        error_message = error_events[0].message
        assert error_message is not None, "Error event should have a message"
        assert "Test error in task" in error_message, f"Error message should contain exception info from Python service, got: {error_message}"

        session.close()
    finally:
        # Clean up
        try:
            sessions = flamepy.list_sessions()
            for sess in sessions:
                try:
                    if sess.application == flm_error_svc_app:
                        flamepy.close_session(sess.id)
                except Exception:
                    pass
        except Exception:
            pass
        flamepy.unregister_application(flm_error_svc_app)

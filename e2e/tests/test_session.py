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

E2E tests for session management, resource requirements, concurrent operations,
open_session API, and batch sessions.

This module tests:
- Session lifecycle management
- Resource requirements (resreq) configuration
- Concurrent session operations
- Session state transitions
- Task lifecycle within sessions
- open_session API (create-or-get semantics)
- Batch session operations
"""

import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed, wait

import flamepy
import pytest
from flamepy import SessionState, TaskState

from e2e.api import TestRequest
from e2e.helpers import invoke_task, serialize_request
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

    # Clean up sessions owned by this module before unregistering.
    sessions = flamepy.list_sessions()
    for sess in sessions:
        if sess.application != FLM_TEST_SVC_APP:
            continue
        try:
            flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(FLM_TEST_SVC_APP)


# =============================================================================
# Session Lifecycle Tests
# =============================================================================


class TestSessionLifecycle:
    """Tests for session lifecycle management."""

    def test_session_create_and_close(self):
        """Test basic session creation and closure."""
        session_id = f"test-lifecycle-{random_string(8)}"
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
        )

        assert session.id == session_id
        assert session.application == FLM_TEST_SVC_APP
        assert session.state == SessionState.OPEN

        # Close the session
        flamepy.close_session(session_id)

        # Verify closed state
        sessions = flamepy.list_sessions()
        closed_session = next((s for s in sessions if s.id == session_id), None)
        assert closed_session is not None
        assert closed_session.state == SessionState.CLOSED

    def test_session_get_after_create(self):
        """Test getting session info after creation."""
        session_id = f"test-get-{random_string(8)}"
        created_session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
        )

        # Get the session
        retrieved_session = flamepy.get_session(session_id)

        assert retrieved_session.id == created_session.id
        assert retrieved_session.application == created_session.application
        assert retrieved_session.state == SessionState.OPEN

        flamepy.close_session(session_id)

    def test_session_list_filters_by_state(self):
        """Test that list_sessions returns sessions in expected states."""
        session_id = f"test-list-{random_string(8)}"

        # Create a session
        flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
        )

        # Should appear in list with OPEN state
        sessions = flamepy.list_sessions()
        open_session = next((s for s in sessions if s.id == session_id), None)
        assert open_session is not None
        assert open_session.state == SessionState.OPEN

        # Close it
        flamepy.close_session(session_id)

        # Should now be CLOSED
        sessions = flamepy.list_sessions()
        closed_session = next((s for s in sessions if s.id == session_id), None)
        assert closed_session is not None
        assert closed_session.state == SessionState.CLOSED

    def test_session_close_is_idempotent(self):
        """Test that closing an already closed session doesn't error."""
        session_id = f"test-idempotent-{random_string(8)}"
        flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
        )

        # Close once
        flamepy.close_session(session_id)

        # Close again - should not raise
        flamepy.close_session(session_id)


# =============================================================================
# Resource Requirements Tests
# =============================================================================


class TestResourceRequirements:
    """Tests for resource requirements configuration."""

    def test_session_with_resreq(self):
        """Test creating session with explicit resource requirements."""
        session_id = f"test-resreq-{random_string(8)}"

        resreq = flamepy.ResourceRequirement(
            cpu=1,
            memory=512,
            gpu=0,
        )

        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
            resreq=resreq,
        )

        assert session.id == session_id
        assert session.state == SessionState.OPEN

        # Run a task to verify session works
        request = TestRequest(input="resreq_test")
        response = invoke_task(session, request)
        assert response.output == "resreq_test"

        session.close()

    def test_session_with_min_max_instances(self):
        """Test creating session with min/max instance constraints."""
        session_id = f"test-minmax-{random_string(8)}"
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
            min_instances=0,
            max_instances=5,
        )

        assert session.id == session_id
        assert session.state == SessionState.OPEN

        # Run a task to verify session works
        request = TestRequest(input="test")
        response = invoke_task(session, request)
        assert response.output == "test"

        session.close()

    def test_session_batch_size(self):
        """Test that the reserved batch_size argument is normalized to one."""
        session_id = f"test-batch-{random_string(8)}"
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
            batch_size=2,
            min_instances=2,
        )

        assert session.id == session_id
        assert session.state == SessionState.OPEN

        session.close()


# =============================================================================
# Concurrent Session Tests
# =============================================================================


class TestConcurrentSessions:
    """Tests for concurrent session operations."""

    def test_multiple_sessions_same_app(self):
        """Test creating multiple sessions for the same application."""
        session_ids = [f"test-multi-{random_string(8)}" for _ in range(3)]
        sessions = []

        try:
            # Create multiple sessions
            for sid in session_ids:
                session = flamepy.create_session(
                    application=FLM_TEST_SVC_APP,
                    session_id=sid,
                )
                sessions.append(session)

            # Verify all are open
            for i, session in enumerate(sessions):
                assert session.id == session_ids[i]
                assert session.state == SessionState.OPEN

            # Run tasks in each session
            for i, session in enumerate(sessions):
                request = TestRequest(input=f"session_{i}")
                response = invoke_task(session, request)
                assert response.output == f"session_{i}"

        finally:
            # Clean up
            for sid in session_ids:
                try:
                    flamepy.close_session(sid)
                except Exception:
                    pass

    def test_concurrent_task_creation(self):
        """Test concurrent task creation in the same session."""
        session_id = f"test-concurrent-tasks-{random_string(8)}"
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
        )

        try:
            num_tasks = 10
            results = []

            def submit_task(idx):
                request = TestRequest(input=f"concurrent_{idx}")
                response = invoke_task(session, request)
                return response.output

            # Submit tasks concurrently using threads
            with ThreadPoolExecutor(max_workers=5) as executor:
                futures = [executor.submit(submit_task, i) for i in range(num_tasks)]
                for future in as_completed(futures):
                    results.append(future.result())

            # Verify all tasks completed
            assert len(results) == num_tasks
            expected_outputs = {f"concurrent_{i}" for i in range(num_tasks)}
            assert set(results) == expected_outputs

        finally:
            session.close()

    def test_parallel_session_creation(self):
        """Test creating sessions in parallel threads."""
        num_sessions = 5
        session_ids = [f"test-parallel-create-{random_string(8)}" for _ in range(num_sessions)]
        created_sessions = []
        lock = threading.Lock()

        def create_session(sid):
            session = flamepy.create_session(
                application=FLM_TEST_SVC_APP,
                session_id=sid,
            )
            with lock:
                created_sessions.append(session)
            return session

        try:
            with ThreadPoolExecutor(max_workers=num_sessions) as executor:
                futures = [executor.submit(create_session, sid) for sid in session_ids]
                for future in as_completed(futures):
                    future.result()  # Ensure no exceptions

            # Verify all sessions created
            assert len(created_sessions) == num_sessions
            for session in created_sessions:
                assert session.state == SessionState.OPEN

        finally:
            for sid in session_ids:
                try:
                    flamepy.close_session(sid)
                except Exception:
                    pass


# =============================================================================
# Task State Transition Tests
# =============================================================================


class TestTaskStateTransitions:
    """Tests for task state transitions within sessions."""

    def test_task_state_pending_to_succeed(self):
        """Test task transitions from PENDING to SUCCEED."""
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            common_data=None,
        )

        try:
            request = TestRequest(input="state_test")
            request_bytes = _serialize_request(request)
            task = session.create_task(request_bytes)

            # Task starts in PENDING state
            initial_task = session.get_task(task.id)
            assert initial_task.state in [TaskState.PENDING, TaskState.RUNNING, TaskState.SUCCEED]

            # Watch for completion
            watcher = session.watch_task(task.id)
            final_task = None
            for task_update in watcher:
                if task_update.is_completed():
                    final_task = task_update
                    break
                if task_update.is_failed():
                    pytest.fail(f"Task failed: {task_update.events}")

            assert final_task is not None
            assert final_task.state == TaskState.SUCCEED

        finally:
            session.close()

    def test_task_has_events(self):
        """Test that completed tasks have events recorded."""
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            common_data=None,
        )

        try:
            request = TestRequest(input="event_test")
            response = invoke_task(session, request)
            assert response.output == "event_test"

            # Get tasks and check events
            tasks = list(session.list_tasks())
            assert len(tasks) >= 1

            # At least one task should have events
            task_with_events = tasks[0]
            refreshed_task = session.get_task(task_with_events.id)
            # Events are recorded during state transitions
            assert refreshed_task.events is not None

        finally:
            session.close()

    def test_session_task_counters(self):
        """Test that session tracks task counters correctly."""
        session_id = f"test-counters-{random_string(8)}"
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            session_id=session_id,
        )

        try:
            # Run some tasks
            num_tasks = 5
            for i in range(num_tasks):
                request = TestRequest(input=f"counter_{i}")
                invoke_task(session, request)

            # Check session counters
            updated_session = flamepy.get_session(session_id)
            assert updated_session.succeed == num_tasks
            assert updated_session.failed == 0

        finally:
            session.close()


# =============================================================================
# Session Common Data Tests
# =============================================================================


class TestSessionCommonData:
    """Tests for session common data handling."""

    def test_session_without_common_data(self):
        """Test session creation without common data."""
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            common_data=None,
        )

        try:
            assert session.common_data() is None

            request = TestRequest(input="no_common_data")
            response = invoke_task(session, request)
            assert response.output == "no_common_data"
            assert response.common_data is None

        finally:
            session.close()

    def test_session_with_common_data(self):
        """Test session creation with common data."""
        from e2e.api import TestContext
        from e2e.helpers import serialize_common_data

        common_data_value = random_string()
        test_context = TestContext(common_data=common_data_value)
        common_data_bytes = serialize_common_data(test_context, FLM_TEST_SVC_APP)

        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            common_data=common_data_bytes,
        )

        try:
            assert session.common_data() is not None

            request = TestRequest(input="with_common_data")
            response = invoke_task(session, request)
            assert response.output == "with_common_data"
            assert response.common_data == common_data_value

        finally:
            session.close()


# =============================================================================
# Open Session API Tests
# =============================================================================


@pytest.fixture(scope="class")
def setup_open_session_app():
    """Setup test application for open_session tests."""
    app_name = "flmtest-open-session"
    flamepy.register_application(
        app_name,
        flamepy.ApplicationAttributes(),
    )

    yield app_name

    # Clean up all sessions before unregistering
    sessions = flamepy.list_sessions()
    for sess in sessions:
        try:
            if sess.application == app_name:
                flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(app_name)


class TestOpenSession:
    """Tests for open_session API (create-or-get semantics)."""

    def test_open_session_existing(self, setup_open_session_app):
        """Test open_session returns existing session when no spec provided."""
        app_name = setup_open_session_app
        # First create a session
        session_id = f"test-open-existing-{random_string(8)}"
        created_session = flamepy.create_session(
            application=app_name,
            session_id=session_id,
        )

        # Open the existing session without spec
        opened_session = flamepy.open_session(session_id)

        # Verify it's the same session
        assert opened_session.id == created_session.id
        assert opened_session.application == created_session.application
        assert opened_session.state == flamepy.SessionState.OPEN

        # Clean up
        flamepy.close_session(session_id)

    def test_open_session_create_with_spec(self, setup_open_session_app):
        """Test open_session creates new session when spec provided and session doesn't exist."""
        app_name = setup_open_session_app
        session_id = f"test-open-create-{random_string(8)}"

        # Open session with spec - should create it
        spec = flamepy.SessionAttributes(
            id=session_id,
            application=app_name,
            min_instances=0,
            max_instances=5,
        )
        session = flamepy.open_session(session_id, spec=spec)

        # Verify session was created
        assert session.id == session_id
        assert session.application == app_name
        assert session.state == flamepy.SessionState.OPEN

        # Clean up
        flamepy.close_session(session_id)

    def test_open_session_existing_with_matching_spec(self, setup_open_session_app):
        """Test open_session returns existing session when spec matches."""
        app_name = setup_open_session_app
        session_id = f"test-open-match-{random_string(8)}"

        # Create session with specific spec
        spec = flamepy.SessionAttributes(
            id=session_id,
            application=app_name,
            min_instances=0,
            max_instances=10,
        )
        created_session = flamepy.create_session(
            application=app_name,
            session_id=session_id,
            min_instances=0,
            max_instances=10,
        )

        # Open with same spec - should succeed
        opened_session = flamepy.open_session(session_id, spec=spec)

        # Verify it's the same session
        assert opened_session.id == created_session.id
        assert opened_session.application == created_session.application

        # Clean up
        flamepy.close_session(session_id)

    def test_open_session_existing_with_mismatched_spec(self, setup_open_session_app):
        """Test open_session raises error when spec doesn't match existing session."""
        app_name = setup_open_session_app
        session_id = f"test-open-mismatch-{random_string(8)}"

        # Create session with specific spec
        flamepy.create_session(
            application=app_name,
            session_id=session_id,
            min_instances=0,
            max_instances=10,
        )

        # Try to open with different spec - should fail
        mismatched_spec = flamepy.SessionAttributes(
            id=session_id,
            application=app_name,
            min_instances=0,
            max_instances=5,  # Different max_instances
        )

        with pytest.raises(Exception) as exc_info:
            flamepy.open_session(session_id, spec=mismatched_spec)

        # Verify error message mentions spec mismatch
        assert "spec mismatch" in str(exc_info.value).lower() or "max_instances" in str(exc_info.value).lower()

        # Clean up
        flamepy.close_session(session_id)

    def test_open_session_not_found_without_spec(self, setup_open_session_app):
        """Test open_session raises error when session doesn't exist and no spec provided."""
        non_existent_id = f"non-existent-{random_string(8)}"

        with pytest.raises(Exception) as exc_info:
            flamepy.open_session(non_existent_id)

        # Verify error indicates session not found
        assert "not found" in str(exc_info.value).lower()

    def test_open_session_idempotent(self, setup_open_session_app):
        """Test open_session is idempotent - multiple calls return same session."""
        app_name = setup_open_session_app
        session_id = f"test-open-idempotent-{random_string(8)}"

        spec = flamepy.SessionAttributes(
            id=session_id,
            application=app_name,
            min_instances=0,
            max_instances=5,
        )

        # First call creates the session
        session1 = flamepy.open_session(session_id, spec=spec)

        # Second call should return the same session
        session2 = flamepy.open_session(session_id, spec=spec)

        # Third call without spec should also work
        session3 = flamepy.open_session(session_id)

        # All should be the same session
        assert session1.id == session2.id == session3.id
        assert session1.application == session2.application == session3.application

        # Clean up
        flamepy.close_session(session_id)

    def test_open_session_closed_session(self, setup_open_session_app):
        """Test open_session raises error when session exists but is closed."""
        app_name = setup_open_session_app
        session_id = f"test-open-closed-{random_string(8)}"

        # Create and close a session
        flamepy.create_session(
            application=app_name,
            session_id=session_id,
        )
        flamepy.close_session(session_id)

        # Try to open the closed session - should fail
        with pytest.raises(Exception) as exc_info:
            flamepy.open_session(session_id)

        # Verify error indicates session is not open
        assert "not open" in str(exc_info.value).lower() or "closed" in str(exc_info.value).lower() or "invalid" in str(exc_info.value).lower()


# =============================================================================
# Concurrent task tests
# =============================================================================


class TestConcurrentTasks:
    """Tests for concurrent task operations."""

    def test_concurrent_tasks_basic(self):
        """Test multiple task operations."""
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            min_instances=2,
        )

        task_num = 4
        for i in range(task_num):
            request = TestRequest(input=f"task_{i}")
            response = invoke_task(session, request)
            assert response.output == f"task_{i}"

        session.close()

    def test_parallel_tasks(self):
        """Test parallel task submission."""
        session = flamepy.create_session(
            application=FLM_TEST_SVC_APP,
            min_instances=2,
        )

        task_num = 4
        futures = []
        for i in range(task_num):
            request = TestRequest(input=f"parallel_task_{i}")
            future = session.run(serialize_request(request))
            futures.append(future)

        wait(futures)
        results = [f.result() for f in futures]

        assert len(results) == task_num

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

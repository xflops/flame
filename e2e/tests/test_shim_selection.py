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

E2E tests for shim selection feature (Issue #379).

These tests verify the shim selection logic that ensures applications are
matched with executors that support the required shim type (Host or Wasm).

Test scenarios:
1. Positive case: App with default shim (Host) matches executor with shim "Host"
2. Negative case: App with shim "Wasm" fails to match executor with shim "Host"
3. Default behavior: App without explicit shim defaults to "Host"
"""

import time

import flamepy
import pytest

from e2e.api import TestRequest
from e2e.helpers import invoke_task

FLM_SHIM_TEST_APP = "flme2e-shim-test"


@pytest.fixture(scope="module", autouse=True)
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

    def test_host_app_on_host_executor(self):
        """
        Test that an application with default shim (Host) successfully
        runs on an executor with Host shim.
        """
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

    def test_wasm_app_on_host_executor_stays_pending(self):
        """
        Test that an application with Wasm shim cannot be scheduled
        on an executor with Host shim.

        The session should remain in pending state because no compatible
        executor is available (the e2e environment only has Host executors).
        """
        app_name = "flmtest-shim-wasm"

        try:
            # Register app with Wasm shim using flamepy SDK
            flamepy.register_application(
                app_name,
                flamepy.ApplicationAttributes(
                    shim=flamepy.Shim.WASM,
                    command="/bin/echo",
                    arguments=["hello", "from", "wasm", "shim"],
                    description="Test app for Wasm shim selection",
                ),
            )

            # Create session for the Wasm app
            session = flamepy.create_session(application=app_name)

            try:
                assert session is not None
                assert session.id is not None

                # Wait a bit for scheduler to attempt allocation
                time.sleep(3)

                # Get session status - should still be OPEN (not allocated)
                sessions = flamepy.list_sessions()
                session_status = next((s for s in sessions if s.id == session.id), None)

                assert session_status is not None
                assert session_status.state == flamepy.SessionState.OPEN

            finally:
                session.close()

        finally:
            try:
                flamepy.unregister_application(app_name)
            except Exception:
                pass

    def test_wasm_app_task_stays_pending(self):
        """
        Test that a task created for a Wasm application stays pending
        when only Host executors are available.

        This verifies the scheduler correctly filters out incompatible executors.
        """
        app_name = "flmtest-shim-wasm-task"

        try:
            # Register app with Wasm shim using flamepy SDK
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

                time.sleep(3)

                task_status = session.get_task(task.id)

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

    def test_app_without_shim_defaults_to_host(self):
        """
        Test that an application registered without explicit shim
        defaults to Host and successfully matches Host executors.
        """
        session = flamepy.create_session(application=FLM_SHIM_TEST_APP, common_data=None)

        try:
            assert session is not None
            assert session.application == FLM_SHIM_TEST_APP

            request = TestRequest(input="default_shim_test")
            response = invoke_task(session, request)

            assert response.output == "default_shim_test"

        finally:
            session.close()

    def test_existing_apps_work_without_shim(self):
        """
        Test that pre-existing applications (flmexec, flmping, flmrun)
        continue to work after the shim selection feature is added.

        This is a regression test to ensure backward compatibility.
        """
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

    def test_host_shim_task_execution(self):
        """
        Test a complete workflow with Host shim.
        """
        session = flamepy.create_session(application=FLM_SHIM_TEST_APP, common_data=None)

        try:
            request = TestRequest(input="integration_test_input")
            response = invoke_task(session, request)

            assert response.output == "integration_test_input"
            assert response.service_state is not None
            assert response.service_state["task_count"] == 1

        finally:
            session.close()

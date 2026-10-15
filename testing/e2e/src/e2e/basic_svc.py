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

import logging
import time
from typing import Optional

import flamepy

from e2e.api import (
    ApplicationContextInfo,
    SessionContextInfo,
    TaskContextInfo,
    TestContext,
    TestRequest,
    TestResponse,
)
from e2e.helpers import (
    deserialize_common_data,
    deserialize_request,
    serialize_response,
)

logger = logging.getLogger(__name__)


class BasicTestService(flamepy.FlameService):
    """Service that implements FlameService for testing with context information."""

    def __init__(self):
        self._session_context: Optional[flamepy.SessionContext] = None
        self._task_count = 0
        self._session_enter_count = 0
        self._session_leave_count = 0
        self._test_context = TestContext()
        logger.info("BasicTestService initialized")

    def on_session_enter(self, context: flamepy.SessionContext):
        """Handle session enter and store context."""
        logger.info(f"Session entered: session_id={context.session_id}, app_name={context.application.name if context.application else None}")
        self._session_context = context
        self._test_context = deserialize_common_data(context.common_data()) or TestContext()
        self._session_enter_count += 1
        self._task_count = 0
        logger.debug(f"Session enter count: {self._session_enter_count}, task count reset to: {self._task_count}")

        if self._test_context.fail_on_session_enter:
            logger.info(f"Failing session enter by common-data flag: session_id={context.session_id}")
            raise RuntimeError("intentional session enter failure")

    def on_task_invoke(self, context: flamepy.TaskContext) -> Optional[flamepy.TaskOutput]:
        """Handle task invoke and return response with optional context information."""
        logger.info(f"Task invoked: task_id={context.task_id}, session_id={context.session_id}, has_input={context.input is not None}, input_size={len(context.input) if context.input else 0}")
        self._task_count += 1
        logger.debug(f"Task count incremented to: {self._task_count}")

        if self._test_context.fail_on_task:
            error_message = f"Test error in task {context.task_id}"
            logger.error(f"Raising exception by common-data flag: {error_message}")
            raise ValueError(error_message)

        # Deserialize task input from bytes using helper function
        request: TestRequest = None
        if context.input is not None:
            logger.debug(f"Deserializing request from {len(context.input)} bytes")
            request = deserialize_request(context.input)
            logger.debug(
                f"Request deserialized: input={request.input}, update_common_data={request.update_common_data}, "
                f"output={request.output}, sleep_ms={request.sleep_ms}, fail_on_task={request.fail_on_task}, "
                f"request_task_context={request.request_task_context}, "
                f"request_session_context={request.request_session_context}, "
                f"request_application_context={request.request_application_context}"
            )
        else:
            logger.debug("No input provided for this task")

        if request and request.fail_on_task:
            error_message = f"Test error in task {context.task_id}"
            logger.error(f"Raising exception by request flag: {error_message}")
            raise ValueError(error_message)

        if request and request.sleep_ms > 0:
            logger.info(f"Sleeping for requested E2E task duration: {request.sleep_ms}ms")
            time.sleep(request.sleep_ms / 1000.0)

        # Get common data using helper function
        common_data = None
        if self._session_context is not None:
            logger.debug("Retrieving common data from session context")
            common_data_bytes = self._session_context.common_data()
            if common_data_bytes is not None:
                logger.debug(f"Common data bytes found: {len(common_data_bytes)} bytes")
                common_data = self._test_context.common_data
                logger.debug(f"Common data extracted: {common_data}")
            else:
                logger.debug("No common data bytes in session context")
        else:
            logger.warning("Session context is None, cannot retrieve common data")

        # Update common data if requested
        # Note: Since update_common_data() was removed from SessionContext,
        # we can't update it directly. This test service stores the update locally
        # but it won't persist across tasks. For production use, use service module.
        updated_context = None
        if request and request.update_common_data and self._session_context is not None:
            logger.info(f"Updating common data (local only): {request.input}")
            # Store updated context locally for this response
            updated_context = TestContext(common_data=request.input)
            # Note: This won't persist - SessionContext doesn't support updates anymore
            # For persistent updates, the client should recreate the session with new common_data
        elif request and request.update_common_data:
            logger.warning("Cannot update common data: session context is None")

        # Use updated context if available, otherwise use original
        response_common_data = updated_context.common_data if updated_context else common_data
        logger.debug(f"Response common data set to: {response_common_data}")

        # Build response
        response = TestResponse(
            output=request.output if request and request.output is not None else request.input if request else None,
            common_data=response_common_data,
            service_state={
                "task_count": self._task_count,
                "session_enter_count": self._session_enter_count,
                "session_leave_count": self._session_leave_count,
            },
        )

        # Add task context information if requested
        if request and request.request_task_context:
            logger.debug("Adding task context information to response")
            response.task_context = TaskContextInfo(
                task_id=context.task_id,
                session_id=context.session_id,
                has_input=context.input is not None,
                input_type=type(request).__name__ if request else None,
            )
            logger.debug(f"Task context added: {response.task_context}")

        # Add session context information if requested
        if request and request.request_session_context and self._session_context is not None:
            logger.debug("Adding session context information to response")
            common_data_bytes = self._session_context.common_data()
            response.session_context = SessionContextInfo(
                session_id=self._session_context.session_id,
                has_common_data=common_data_bytes is not None,
                common_data_type=type(self._test_context).__name__ if common_data_bytes is not None else None,
            )
            logger.debug(f"Session context added: session_id={response.session_context.session_id}, has_common_data={response.session_context.has_common_data}, common_data_type={response.session_context.common_data_type}")

        # Add application context information if requested
        if request and request.request_application_context and self._session_context is not None:
            logger.debug("Adding application context information to response")
            app_ctx = self._session_context.application

            app_info = ApplicationContextInfo(
                name=app_ctx.name,
                image=app_ctx.image,
                command=app_ctx.command,
                working_directory=app_ctx.working_directory,
                url=app_ctx.url,
            )

            response.application_context = app_info
            logger.debug(f"Application context added: name={app_info.name}, image={app_info.image}")

            # Also add to session_context if it exists
            if response.session_context is not None:
                response.session_context.application = app_info
                logger.debug("Application context also added to session_context")

        # Serialize response to bytes using helper function
        logger.debug("Serializing response to bytes")
        response_bytes = serialize_response(response)
        logger.info(f"Task completed successfully: task_id={context.task_id}, response_size={len(response_bytes)} bytes")
        return flamepy.TaskOutput(response_bytes)

    def on_session_leave(self):
        """Handle session leave."""
        session_id = self._session_context.session_id if self._session_context else None
        logger.info(f"Session leaving: session_id={session_id}, total_tasks={self._task_count}, session_leave_count={self._session_leave_count + 1}")
        self._session_leave_count += 1
        self._session_context = None
        self._test_context = TestContext()
        logger.debug("Session context cleared")


if __name__ == "__main__":
    flamepy.run(BasicTestService())

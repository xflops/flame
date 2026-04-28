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
from typing import Optional

import flamepy

logger = logging.getLogger(__name__)


class ErrorTestService(flamepy.FlameService):
    """Service that raises exceptions in on_task_invoke for testing error handling."""

    def __init__(self):
        self._session_context: Optional[flamepy.SessionContext] = None
        logger.info("ErrorTestService initialized")

    def on_session_enter(self, context: flamepy.SessionContext):
        """Handle session enter and store context."""
        logger.info(f"Session entered: session_id={context.session_id}")
        self._session_context = context

    def on_task_invoke(self, context: flamepy.TaskContext) -> Optional[flamepy.TaskOutput]:
        """Handle task invoke by raising an exception."""
        logger.info(f"Task invoked: task_id={context.task_id}, session_id={context.session_id}")

        # Raise an exception with a specific error message
        error_message = f"Test error in task {context.task_id}"
        logger.error(f"Raising exception: {error_message}")
        raise ValueError(error_message)

    def on_session_leave(self):
        """Handle session leave."""
        session_id = self._session_context.session_id if self._session_context else None
        logger.info(f"Session leaving: session_id={session_id}")
        self._session_context = None


if __name__ == "__main__":
    flamepy.run(ErrorTestService())

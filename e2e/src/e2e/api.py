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

from dataclasses import dataclass
from typing import Any, Dict, Optional


@dataclass
class TestContext:
    common_data: Optional[str] = None


@dataclass
class ApplicationContextInfo:
    """Information about the application context."""

    name: Optional[str] = None
    image: Optional[str] = None
    command: Optional[str] = None
    working_directory: Optional[str] = None
    url: Optional[str] = None


@dataclass
class SessionContextInfo:
    """Information about the session context."""

    session_id: Optional[str] = None
    application: Optional[ApplicationContextInfo] = None
    has_common_data: bool = False
    common_data_type: Optional[str] = None


@dataclass
class TaskContextInfo:
    """Information about the task context."""

    task_id: Optional[str] = None
    session_id: Optional[str] = None
    has_input: bool = False
    input_type: Optional[str] = None


@dataclass
class TestRequest:
    update_common_data: bool = False
    input: Optional[str] = None
    # Flags to control what context information to return
    request_task_context: bool = False
    request_session_context: bool = False
    request_application_context: bool = False


@dataclass
class TestResponse:
    output: Optional[str] = None
    common_data: Optional[str] = None
    # Context information fields
    task_context: Optional[TaskContextInfo] = None
    session_context: Optional[SessionContextInfo] = None
    application_context: Optional[ApplicationContextInfo] = None
    # Service state information
    service_state: Optional[Dict[str, Any]] = None

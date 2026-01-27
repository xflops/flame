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

# Cache classes and functions
from .cache import (
    ObjectRef,
    get_object,
    put_object,
    update_object,
)

# Client classes
# Client functions
from .client import (
    Connection,
    ConnectionInstance,
    Session,
    TaskWatcher,
    close_session,
    connect,
    create_session,
    get_application,
    get_session,
    list_applications,
    list_sessions,
    open_session,
    register_application,
    unregister_application,
)

# Instance service
from .instance import FlameInstance

# Runner classes
from .runner import ObjectFuture, ObjectFutureIterator, Runner, RunnerService

# Runpy service
from .runpy import FlameRunpyService

# Service functions
# Service implementation classes
# Service base classes
# Service context classes
# Service constants
from .service import (
    FLAME_INSTANCE_ENDPOINT,
    ApplicationContext,
    FlameInstanceServer,
    FlameInstanceServicer,
    FlameService,
    SessionContext,
    TaskContext,
    run,
)

# Utility functions
# Context and utility classes
# Data classes
# Exception classes
# Enums
# Constants
# Type aliases
from .types import (
    DEFAULT_FLAME_CACHE_ENDPOINT,
    DEFAULT_FLAME_CONF,
    DEFAULT_FLAME_ENDPOINT,
    Application,
    ApplicationAttributes,
    ApplicationID,
    ApplicationSchema,
    ApplicationState,
    CommonData,
    Event,
    FlameContext,
    FlameContextRunner,
    FlameError,
    FlameErrorCode,
    FlamePackage,
    Message,
    RunnerContext,
    RunnerRequest,
    SessionAttributes,
    SessionID,
    SessionState,
    Shim,
    Task,
    TaskID,
    TaskInformer,
    TaskInput,
    TaskOutput,
    TaskState,
    short_name,
)

__all__ = [
    # Type aliases
    "TaskID",
    "SessionID",
    "ApplicationID",
    "Message",
    "TaskInput",
    "TaskOutput",
    "CommonData",
    # Constants
    "DEFAULT_FLAME_CONF",
    "DEFAULT_FLAME_ENDPOINT",
    "DEFAULT_FLAME_CACHE_ENDPOINT",
    # Enums
    "SessionState",
    "TaskState",
    "ApplicationState",
    "Shim",
    "FlameErrorCode",
    # Exception classes
    "FlameError",
    # Data classes
    "Event",
    "SessionAttributes",
    "ApplicationSchema",
    "ApplicationAttributes",
    "Task",
    "Application",
    "FlamePackage",
    "FlameContextRunner",
    "RunnerContext",
    "RunnerRequest",
    # Context and utility classes
    "TaskInformer",
    "FlameContext",
    # Utility functions
    "short_name",
    # Client functions
    "connect",
    "create_session",
    "open_session",
    "register_application",
    "unregister_application",
    "list_applications",
    "get_application",
    "list_sessions",
    "get_session",
    "close_session",
    # Client classes
    "ConnectionInstance",
    "Connection",
    "Session",
    "TaskWatcher",
    # Service constants
    "FLAME_INSTANCE_ENDPOINT",
    # Service context classes
    "ApplicationContext",
    "SessionContext",
    "TaskContext",
    # Service base classes
    "FlameService",
    # Service implementation classes
    "FlameInstanceServicer",
    "FlameInstanceServer",
    # Service functions
    "run",
    # Cache classes
    "ObjectRef",
    # Cache functions
    "get_object",
    "put_object",
    "update_object",
    # Runner classes
    "ObjectFuture",
    "ObjectFutureIterator",
    "Runner",
    "RunnerService",
    # Runpy service
    "FlameRunpyService",
    # Instance service
    "FlameInstance",
]

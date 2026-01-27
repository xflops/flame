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

# Import submodules for rl and agent (only as submodules)
from . import agent, rl

# Export all core classes/types at top level
from .core import (  # Type aliases; Constants; Enums; Exception classes; Data classes; Context and utility classes; Client functions; Client classes; Service constants; Service context classes; Service base classes; Service functions; Runner classes; Cache classes
    DEFAULT_FLAME_CACHE_ENDPOINT,
    DEFAULT_FLAME_CONF,
    DEFAULT_FLAME_ENDPOINT,
    FLAME_INSTANCE_ENDPOINT,
    Application,
    ApplicationAttributes,
    ApplicationContext,
    ApplicationID,
    ApplicationSchema,
    ApplicationState,
    CommonData,
    Connection,
    Event,
    FlameContext,
    FlameContextRunner,
    FlameError,
    FlameErrorCode,
    FlameInstance,
    FlamePackage,
    FlameRunpyService,
    FlameService,
    Message,
    ObjectFuture,
    ObjectFutureIterator,
    ObjectRef,
    Runner,
    RunnerContext,
    RunnerRequest,
    RunnerService,
    Session,
    SessionAttributes,
    SessionContext,
    SessionID,
    SessionState,
    Shim,
    Task,
    TaskContext,
    TaskID,
    TaskInformer,
    TaskInput,
    TaskOutput,
    TaskState,
    TaskWatcher,
    close_session,
    connect,
    create_session,
    get_application,
    get_object,
    get_session,
    list_applications,
    list_sessions,
    open_session,
    put_object,
    register_application,
    run,
    unregister_application,
    update_object,
)

__version__ = "0.3.0"

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
    # Service functions
    "run",
    # Cache classes
    "ObjectRef",
    # Cache functions
    "get_object",
    "put_object",
    "update_object",
    # Runner classes
    "Runner",
    "RunnerService",
    "ObjectFuture",
    "ObjectFutureIterator",
    "FlameRunpyService",
    # Instance service
    "FlameInstance",
    # Submodules (rl and agent only)
    "agent",
    "rl",
]

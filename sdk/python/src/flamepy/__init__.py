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

# ruff: noqa: E402, I001
# isort: skip_file

# Import proto module first to ensure types_pb2 is loaded before frontend_pb2
from flamepy import proto  # noqa: F401

# Import submodules for runner, service, and util (only as submodules)
from . import runner, service, util

# Export all core classes/types at top level
from .core import (  # Type aliases; Constants; Enums; Exception classes; Data classes; Context and utility classes; Client functions; Client classes; Service constants; Service context classes; Service base classes; Service functions
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
    FlamePackage,
    FlameService,
    Message,
    ObjectRef,
    ResourceRequirement,
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

__version__ = "0.5.0"

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
    "FlameErrorCode",
    "Shim",
    # Exception classes
    "FlameError",
    # Data classes
    "Event",
    "ResourceRequirement",
    "SessionAttributes",
    "ApplicationSchema",
    "ApplicationAttributes",
    "Task",
    "Application",
    "FlamePackage",
    "FlameContextRunner",
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
    # Submodules
    "runner",
    "service",
    "util",
]

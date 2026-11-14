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
    WILDCARD_SESSION,
    ObjectKey,
    ObjectRef,
    Ref,
    ValueRef,
    download_package,
    get_object,
    patch_object,
    put_object,
    upload_package,
)

# Client classes
# Client functions
from .client import (
    Connection,
    ConnectionInstance,
    Session,
    TaskFuture,
    TaskWatcher,
    close_session,
    connect,
    create_session,
    delete_session,
    get_application,
    get_session,
    iter_applications_pages,
    iter_executors_pages,
    iter_nodes_pages,
    iter_sessions_pages,
    iter_tasks_pages,
    list_applications,
    list_applications_page,
    list_executors,
    list_executors_page,
    list_nodes,
    list_nodes_page,
    list_sessions,
    list_sessions_page,
    list_tasks_page,
    open_session,
    register_application,
    resume_session,
    suspend_session,
    unregister_application,
)

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
    DEFAULT_FLAME_APP_TEMPLATE,
    DEFAULT_FLAME_CONF,
    Application,
    ApplicationAttributes,
    ApplicationID,
    ApplicationSchema,
    ApplicationState,
    CommonData,
    Event,
    FlameContext,
    FlameError,
    FlameErrorCode,
    FlamePackage,
    Message,
    Page,
    PageInfo,
    PageRequest,
    ResourceRequirement,
    SessionAttributes,
    SessionID,
    SessionState,
    Shim,
    Task,
    TaskID,
    TaskInformer,
    TaskInput,
    TaskOption,
    TaskOptions,
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
    "DEFAULT_FLAME_APP_TEMPLATE",
    # Enums
    "SessionState",
    "TaskState",
    "ApplicationState",
    "FlameErrorCode",
    "Page",
    "PageInfo",
    "PageRequest",
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
    "TaskOption",
    "TaskOptions",
    "Application",
    "FlamePackage",
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
    "list_executors",
    "list_nodes",
    "get_application",
    "list_applications_page",
    "iter_applications_pages",
    "list_sessions_page",
    "iter_sessions_pages",
    "list_nodes_page",
    "iter_nodes_pages",
    "list_executors_page",
    "iter_executors_pages",
    "list_tasks_page",
    "iter_tasks_pages",
    "list_sessions",
    "get_session",
    "close_session",
    "delete_session",
    "resume_session",
    "suspend_session",
    # Client classes
    "ConnectionInstance",
    "Connection",
    "Session",
    "TaskFuture",
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
    "WILDCARD_SESSION",
    "ObjectKey",
    "ObjectRef",
    "Ref",
    "ValueRef",
    # Cache functions
    "download_package",
    "upload_package",
    "get_object",
    "patch_object",
    "put_object",
]

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

import inspect
import os
import random
import string
from dataclasses import dataclass, field
from datetime import datetime
from enum import IntEnum
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import yaml

# Type aliases
TaskID = str
SessionID = str
ApplicationID = str
Message = bytes
TaskInput = Message
TaskOutput = Message
CommonData = Message


# Constants
DEFAULT_FLAME_CONF = "flame.yaml"
DEFAULT_FLAME_ENDPOINT = "http://127.0.0.1:8080"
DEFAULT_FLAME_CACHE_ENDPOINT = "grpc://127.0.0.1:9090"
DEFAULT_FLAME_RUNNER_TEMPLATE = "flmrun"


class SessionState(IntEnum):
    """Session state enumeration."""

    OPEN = 0
    CLOSED = 1


class TaskState(IntEnum):
    """Task state enumeration."""

    PENDING = 0
    RUNNING = 1
    SUCCEED = 2
    FAILED = 3


class ApplicationState(IntEnum):
    """Application state enumeration."""

    ENABLED = 0
    DISABLED = 1


class Shim(IntEnum):
    """Shim type enumeration."""

    Host = 0
    Wasm = 1


class FlameErrorCode(IntEnum):
    """Flame error code enumeration."""

    INVALID_CONFIG = 0
    INVALID_STATE = 1
    INVALID_ARGUMENT = 2
    INTERNAL = 3


class FlameError(Exception):
    """Flame SDK error exception."""

    def __init__(self, code: FlameErrorCode, message: str):
        self.code = code
        self.message = message
        super().__init__(f"{message} (code: {code})")


@dataclass
class Event:
    """Event for a task."""

    code: int
    message: Optional[str] = None
    creation_time: datetime = None


@dataclass
class SessionAttributes:
    """Attributes for creating a session."""

    application: str
    slots: int
    id: Optional[str] = None
    common_data: Any = None
    min_instances: int = 0  # Minimum number of instances (default: 0)
    max_instances: Optional[int] = None  # Maximum number of instances (None = unlimited)


@dataclass
class ApplicationSchema:
    """Attributes for an application schema."""

    input: Optional[str] = None
    output: Optional[str] = None
    common_data: Optional[str] = None


@dataclass
class ApplicationAttributes:
    """Attributes for an application."""

    shim: Shim
    image: Optional[str] = None
    description: Optional[str] = None
    labels: Optional[List[str]] = None
    command: Optional[str] = None
    arguments: Optional[List[str]] = None
    environments: Optional[Dict[str, str]] = None
    working_directory: Optional[str] = None
    max_instances: Optional[int] = None
    delay_release: Optional[int] = None
    schema: Optional[ApplicationSchema] = None
    url: Optional[str] = None


@dataclass
class Task:
    """Represents a computing task."""

    id: TaskID
    session_id: SessionID
    state: TaskState
    creation_time: datetime
    input: Any = None
    output: Any = None
    completion_time: Optional[datetime] = None
    events: Optional[List[Event]] = None

    def is_completed(self) -> bool:
        """Check if the task is completed."""
        return self.state in (TaskState.SUCCEED, TaskState.FAILED)

    def is_failed(self) -> bool:
        """Check if the task is failed."""
        return self.state == TaskState.FAILED


@dataclass
class Application:
    """Represents a distributed application."""

    id: ApplicationID
    name: str
    shim: Shim
    state: ApplicationState
    creation_time: datetime
    image: Optional[str] = None
    description: Optional[str] = None
    labels: Optional[List[str]] = None
    command: Optional[str] = None
    arguments: Optional[List[str]] = None
    environments: Optional[Dict[str, str]] = None
    working_directory: Optional[str] = None
    max_instances: Optional[int] = None
    delay_release: Optional[int] = None
    schema: Optional[ApplicationSchema] = None
    url: Optional[str] = None


class TaskInformer:
    """Interface for task updates."""

    def on_update(self, task: Task) -> None:
        """Called when a task is updated."""
        pass

    def on_error(self, error: FlameError) -> None:
        """Called when an error occurs."""
        pass


def short_name(prefix: str, length: int = 6) -> str:
    """Generate a short name with a prefix."""
    alphabet = string.ascii_letters + string.digits
    sn = "".join(random.SystemRandom().choice(alphabet) for _ in range(length))
    return f"{prefix}-{sn}"


@dataclass
class FlamePackage:
    """Package configuration for Flame applications.

    Attributes:
        storage: The URL specifying where the application package should be persisted.
                 Currently, only the file:// schema is supported.
        excludes: A list of custom patterns to exclude from the package.
                  By default, includes .venv, __pycache__, .gitignore, and *.pyc.
    """

    storage: str
    excludes: List[str] = field(
        default_factory=lambda: [
            ".venv",
            "__pycache__",
            ".gitignore",
            "*.pyc",
        ]
    )


@dataclass
class FlameContextRunner:
    """Runner configuration for Flame applications.

    Attributes:
        template: The name of the application template to use for runners.
                  If not specified, defaults to 'flmrun'.
    """

    template: Optional[str] = None


@dataclass
class RunnerContext:
    """Context for runner session containing the shared execution object.

    This class encapsulates data shared within a session, including the
    execution object specific to the session.

    Attributes:
        execution_object: The execution object for the customized session. This can be
                          any Python object (function, class, instance, etc.) that will
                          be used to execute tasks within the session.
        stateful: If True, persist the execution object state back to flame-cache
                  after each task. If False, do not persist state.
        autoscale: If True, create instances dynamically based on pending tasks (min=0, max=None).
                   If False, create exactly one instance (min=1, max=1).
        min_instances: Minimum number of instances (computed from autoscale)
        max_instances: Maximum number of instances (computed from autoscale)
    """

    execution_object: Any
    stateful: bool = False
    autoscale: bool = True
    min_instances: int = field(init=False, repr=False)
    max_instances: Optional[int] = field(init=False, repr=False)

    def __post_init__(self) -> None:
        """Compute min/max instances and validate configuration."""
        # Compute min/max instances based on autoscale
        if self.autoscale:
            self.min_instances = 0
            self.max_instances = None  # Unlimited
        else:
            self.min_instances = 1
            self.max_instances = 1  # Single instance

        # Validation: classes cannot be stateful (only instances can)
        if self.stateful and inspect.isclass(self.execution_object):
            raise ValueError("Cannot set stateful=True for a class. Classes themselves cannot maintain state; only instances can. Pass an instance instead, or set stateful=False.")


@dataclass
class RunnerRequest:
    """Request for runner task invocation.

    This class defines the input for each task and contains information about
    which method to invoke and what arguments to pass.

    Attributes:
        method: The name of the method to invoke within the customized application.
                Should be None if the execution object itself is a function or callable.
        args: A tuple containing positional arguments for the method. Optional.
                Can contain ObjectRef instances that will be resolved at runtime.
        kwargs: A dictionary of keyword arguments for the method. Optional.
                Can contain ObjectRef instances that will be resolved at runtime.

    Note: If both args and kwargs are None, the method will be called without arguments.
    """

    method: Optional[str] = None
    args: Optional[Tuple] = None
    kwargs: Optional[Dict[str, Any]] = None

    def __post_init__(self):
        """Validate RunnerRequest fields."""
        if self.method is not None and not isinstance(self.method, str):
            raise ValueError(f"method must be a string or None, got {type(self.method)}")
        if self.args is not None and not isinstance(self.args, (tuple, list)):
            raise ValueError(f"args must be a tuple or list, got {type(self.args)}")
        if self.kwargs is not None and not isinstance(self.kwargs, dict):
            raise ValueError(f"kwargs must be a dict, got {type(self.kwargs)}")


class FlameContext:
    """Flame configuration."""

    _endpoint = None
    _cache = None
    _package = None
    _runner = None

    def __init__(self):
        # Initialize runner with default values
        self._runner = FlameContextRunner(template=DEFAULT_FLAME_RUNNER_TEMPLATE)

        home = Path.home()
        config_file = home / ".flame" / DEFAULT_FLAME_CONF
        if config_file.exists():
            with open(config_file, "r") as f:
                config = yaml.safe_load(f)
                cc = config.get("current-cluster")
                if cc is None:
                    raise FlameError(FlameErrorCode.INVALID_CONFIG, "current-cluster is not set")
                for cluster in config.get("clusters", []):
                    if cc == cluster["name"]:
                        self._endpoint = cluster.get("endpoint")
                        self._cache = cluster.get("cache")

                        # Parse package configuration if present
                        package_config = cluster.get("package")
                        if package_config is not None:
                            storage = package_config.get("storage")
                            if storage is not None:
                                excludes = package_config.get("excludes", [])
                                # Merge with default excludes
                                default_excludes = [".venv", "__pycache__", ".gitignore", "*.pyc"]
                                all_excludes = list(set(default_excludes + excludes))
                                self._package = FlamePackage(storage=storage, excludes=all_excludes)

                        # Parse runner configuration if present
                        runner_config = cluster.get("runner")
                        if runner_config is not None:
                            template = runner_config.get("template")
                            self._runner = FlameContextRunner(template=DEFAULT_FLAME_RUNNER_TEMPLATE if template is None else template)
                        break
                else:
                    raise FlameError(FlameErrorCode.INVALID_CONFIG, f"cluster <{cc}> not found")

        endpoint = os.getenv("FLAME_ENDPOINT")
        if endpoint is not None:
            self._endpoint = endpoint

        cache_endpoint = os.getenv("FLAME_CACHE_ENDPOINT")
        if cache_endpoint is not None:
            # Environment variable overrides config
            if isinstance(self._cache, dict):
                self._cache["endpoint"] = cache_endpoint
            else:
                self._cache = {"endpoint": cache_endpoint}

        cache_storage = os.getenv("FLAME_CACHE_STORAGE")
        if cache_storage is not None:
            # Environment variable overrides config
            if isinstance(self._cache, dict):
                self._cache["storage"] = cache_storage
            elif self._cache is not None:
                self._cache = {"endpoint": self._cache, "storage": cache_storage}
            else:
                self._cache = {"storage": cache_storage}

    @property
    def endpoint(self) -> str:
        """Get the Flame cluster endpoint."""
        return self._endpoint if self._endpoint is not None else DEFAULT_FLAME_ENDPOINT

    @property
    def package(self) -> Optional[FlamePackage]:
        """Get the package configuration."""
        return self._package

    @property
    def cache(self) -> Optional[Any]:
        """Get the cache configuration (dict with endpoint and optional storage, or string for legacy)."""
        return self._cache if self._cache is not None else {"endpoint": DEFAULT_FLAME_CACHE_ENDPOINT}

    @property
    def cache_endpoint(self) -> str:
        """Get the cache endpoint (legacy property for backward compatibility)."""
        if isinstance(self._cache, dict):
            return self._cache.get("endpoint", DEFAULT_FLAME_CACHE_ENDPOINT)
        elif self._cache is not None:
            return self._cache
        return DEFAULT_FLAME_CACHE_ENDPOINT

    @property
    def runner(self) -> FlameContextRunner:
        """Get the runner configuration."""
        return self._runner

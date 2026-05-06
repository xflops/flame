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

import os
import random
import string
from dataclasses import dataclass, field
from datetime import datetime
from enum import IntEnum
from pathlib import Path
from typing import Any, Dict, List, Optional

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

    HOST = 0
    WASM = 1


class FlameErrorCode(IntEnum):
    """Flame error code enumeration."""

    INVALID_CONFIG = 0
    INVALID_STATE = 1
    INVALID_ARGUMENT = 2
    INTERNAL = 3
    ALREADY_EXISTS = 4
    NOT_FOUND = 5


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
class ResourceRequirement:
    """Resource requirements for a session."""

    cpu: int = 0
    memory: int = 0
    gpu: int = 0

    @classmethod
    def from_string(cls, s: str) -> "ResourceRequirement":
        """Parse resource requirements from string format: cpu=N,mem=SIZE,gpu=N"""
        cpu = 0
        memory = 0
        gpu = 0
        for part in s.split(","):
            key, _, value = part.partition("=")
            key = key.strip().lower()
            value = value.strip()
            if key == "cpu":
                cpu = int(value)
            elif key in ("mem", "memory"):
                memory = cls._parse_memory(value)
            elif key == "gpu":
                gpu = int(value)
        return cls(cpu=cpu, memory=memory, gpu=gpu)

    @staticmethod
    def _parse_memory(s: str) -> int:
        """Parse memory string like '16g' into bytes."""
        s = s.lower().strip()
        if not s:
            return 0
        multipliers = {"k": 1024, "m": 1024**2, "g": 1024**3}
        if s[-1] in multipliers:
            return int(s[:-1]) * multipliers[s[-1]]
        return int(s)


@dataclass
class SessionAttributes:
    """Attributes for creating a session."""

    application: str
    slots: int = 0
    id: Optional[str] = None
    common_data: Any = None
    min_instances: int = 0
    max_instances: Optional[int] = None
    batch_size: int = 1
    resreq: Optional[ResourceRequirement] = None


@dataclass
class ApplicationSchema:
    """Attributes for an application schema."""

    input: Optional[str] = None
    output: Optional[str] = None
    common_data: Optional[str] = None


@dataclass
class ApplicationAttributes:
    """Attributes for an application."""

    shim: Optional[Shim] = None
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
    installer: Optional[str] = None


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
    state: ApplicationState
    creation_time: datetime
    shim: Optional[Shim] = None
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
    installer: Optional[str] = None


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
class FlameClientTls:
    """Client TLS configuration for connecting to Flame services.

    Attributes:
        ca_file: Path to CA certificate for server verification.
                 For self-signed certificates, this must point to the CA
                 certificate that signed the server certificate.

    Note:
        To disable TLS for development, use http:// instead of https://
        in the endpoint URL.
    """

    ca_file: Optional[str] = None


@dataclass
class FlameClientCache:
    """Cache configuration for the client.

    Attributes:
        endpoint: Cache endpoint URL (e.g., "grpcs://flame-object-cache:9090").
        tls: TLS configuration for cache (optional, separate from cluster TLS).
        storage: Local storage path for cache (optional).
    """

    endpoint: Optional[str] = None
    tls: Optional[FlameClientTls] = None
    storage: Optional[str] = None


@dataclass
class FlameClusterConfig:
    """Cluster configuration within a context.

    Attributes:
        endpoint: Cluster endpoint URL (e.g., "https://flame-session-manager:8080").
        tls: TLS configuration for cluster connection (optional).
    """

    endpoint: str = ""
    tls: Optional[FlameClientTls] = None


class FlameContext:
    """Flame configuration.

    Reads configuration from ~/.flame/flame.yaml with the following structure:

    ```yaml
    current-context: flame
    contexts:
      - name: flame
        cluster:
          endpoint: "https://flame-session-manager:8080"
          tls:
            ca_file: "/etc/flame/certs/ca.crt"
        cache:
          endpoint: "grpcs://flame-object-cache:9090"
          tls:
            ca_file: "/etc/flame/certs/cache-ca.crt"
        package:
          storage: "file:///var/lib/flame/packages"
        runner:
          template: "flmrun"
    ```
    """

    _endpoint = None
    _cluster_tls = None
    _cache = None
    _cache_tls = None
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
                current_context = config.get("current-context")
                if current_context is None:
                    raise FlameError(FlameErrorCode.INVALID_CONFIG, "current-context is not set")

                for ctx in config.get("contexts", []):
                    if current_context == ctx["name"]:
                        # Parse cluster configuration
                        cluster_config = ctx.get("cluster", {})
                        self._endpoint = cluster_config.get("endpoint")

                        # Parse cluster TLS configuration
                        cluster_tls = cluster_config.get("tls")
                        if cluster_tls is not None:
                            self._cluster_tls = FlameClientTls(
                                ca_file=cluster_tls.get("ca_file"),
                            )

                        # Parse cache configuration
                        cache_config = ctx.get("cache")
                        if cache_config is not None:
                            cache_tls = cache_config.get("tls")
                            if cache_tls is not None:
                                self._cache_tls = FlameClientTls(
                                    ca_file=cache_tls.get("ca_file"),
                                )
                            self._cache = FlameClientCache(
                                endpoint=cache_config.get("endpoint"),
                                tls=self._cache_tls,
                                storage=cache_config.get("storage"),
                            )

                        # Parse package configuration if present
                        package_config = ctx.get("package")
                        if package_config is not None:
                            storage = package_config.get("storage")
                            if storage is not None:
                                excludes = package_config.get("excludes", [])
                                # Merge with default excludes
                                default_excludes = [".venv", "__pycache__", ".gitignore", "*.pyc"]
                                all_excludes = list(set(default_excludes + excludes))
                                self._package = FlamePackage(storage=storage, excludes=all_excludes)

                        # Parse runner configuration if present
                        runner_config = ctx.get("runner")
                        if runner_config is not None:
                            template = runner_config.get("template")
                            self._runner = FlameContextRunner(template=DEFAULT_FLAME_RUNNER_TEMPLATE if template is None else template)
                        break
                else:
                    raise FlameError(FlameErrorCode.INVALID_CONFIG, f"context <{current_context}> not found")

        # Apply environment variable overrides (these take precedence over config file)
        # This also allows building context entirely from env vars when no config file exists
        self._apply_env_overrides()

    def _apply_env_overrides(self):
        """Apply environment variable overrides to the context.

        This method handles:
        1. Overriding config file values with env vars
        2. Building context entirely from env vars when no config file exists

        Environment variables:
        - FLAME_ENDPOINT: Cluster endpoint URL
        - FLAME_CACHE_ENDPOINT: Cache endpoint URL
        - FLAME_CACHE_STORAGE: Cache storage path
        - FLAME_CA_FILE: CA certificate file for TLS (applies to both cluster and cache)
        """
        # Handle FLAME_CA_FILE first so TLS config is ready for cache
        ca_file = os.getenv("FLAME_CA_FILE")
        if ca_file is not None:
            # Set CA file for cluster TLS
            if self._cluster_tls is None:
                self._cluster_tls = FlameClientTls(ca_file=ca_file)
            elif self._cluster_tls.ca_file is None:
                self._cluster_tls.ca_file = ca_file
            # Set CA file for cache TLS
            if self._cache_tls is None:
                self._cache_tls = FlameClientTls(ca_file=ca_file)
            elif self._cache_tls.ca_file is None:
                self._cache_tls.ca_file = ca_file

        # Override/set endpoint
        endpoint = os.getenv("FLAME_ENDPOINT")
        if endpoint is not None:
            self._endpoint = endpoint

        # Override/set cache endpoint
        cache_endpoint = os.getenv("FLAME_CACHE_ENDPOINT")
        if cache_endpoint is not None:
            if self._cache is None:
                self._cache = FlameClientCache(endpoint=cache_endpoint, tls=self._cache_tls)
            else:
                self._cache.endpoint = cache_endpoint
                # Ensure TLS is set if we have it
                if self._cache.tls is None and self._cache_tls is not None:
                    self._cache.tls = self._cache_tls

        # Override/set cache storage
        cache_storage = os.getenv("FLAME_CACHE_STORAGE")
        if cache_storage is not None:
            if self._cache is None:
                self._cache = FlameClientCache(storage=cache_storage, tls=self._cache_tls)
            else:
                self._cache.storage = cache_storage

        # Ensure cache has TLS config if we have one and cache exists
        if self._cache is not None and self._cache.tls is None and self._cache_tls is not None:
            self._cache.tls = self._cache_tls

    @property
    def endpoint(self) -> str:
        """Get the Flame cluster endpoint."""
        return self._endpoint if self._endpoint is not None else DEFAULT_FLAME_ENDPOINT

    @property
    def tls(self) -> Optional[FlameClientTls]:
        """Get the cluster TLS configuration."""
        return self._cluster_tls

    @property
    def package(self) -> Optional[FlamePackage]:
        """Get the package configuration."""
        return self._package

    @property
    def cache(self) -> Optional[FlameClientCache]:
        """Get the cache configuration."""
        if self._cache is not None:
            return self._cache
        return FlameClientCache(endpoint=DEFAULT_FLAME_CACHE_ENDPOINT)

    @property
    def cache_endpoint(self) -> str:
        """Get the cache endpoint (legacy property for backward compatibility)."""
        if self._cache is not None and self._cache.endpoint is not None:
            return self._cache.endpoint
        return DEFAULT_FLAME_CACHE_ENDPOINT

    @property
    def cache_tls(self) -> Optional[FlameClientTls]:
        """Get the cache TLS configuration."""
        return self._cache_tls

    @property
    def runner(self) -> FlameContextRunner:
        """Get the runner configuration."""
        return self._runner

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

from dataclasses import dataclass, asdict
from enum import IntEnum
from typing import Optional, List, Dict, Any, Union
from datetime import datetime
from pydantic import BaseModel
from pathlib import Path
import yaml
import os
import bson
import string
import random


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
DEFAULT_FLAME_CACHE_ENDPOINT = "http://127.0.0.1:9090"


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

    id: str
    application: str
    slots: int
    common_data: Optional[bytes] = None


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


@dataclass
class Task:
    """Represents a computing task."""

    id: TaskID
    session_id: SessionID
    state: TaskState
    creation_time: datetime
    input: Optional[bytes] = None
    output: Optional[bytes] = None
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


class TaskInformer:
    """Interface for task updates."""

    def on_update(self, task: Task) -> None:
        """Called when a task is updated."""
        pass

    def on_error(self, error: FlameError) -> None:
        """Called when an error occurs."""
        pass


class Request(BaseModel):
    @classmethod
    def from_json(cls, json_data):
        return cls.model_validate(json_data)

    def to_json(self) -> bytes:
        return self.model_dump()


class Response(BaseModel):
    @classmethod
    def from_json(cls, json_data):
        return cls.model_validate(json_data)

    def to_json(self) -> bytes:
        return self.model_dump()


def short_name(prefix: str, length: int = 6) -> str:
    """Generate a short name with a prefix."""
    alphabet = string.ascii_letters + string.digits
    sn = "".join(random.SystemRandom().choice(alphabet) for _ in range(length))
    return f"{prefix}-{sn}"


class FlameContext:
    """Flame configuration."""

    _endpoint = None
    _cache_endpoint = None

    def __init__(self):
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
                        self._cache_endpoint = cluster.get("cache")
                        break
                else:
                    raise FlameError(FlameErrorCode.INVALID_CONFIG, f"cluster <{cc}> not found")

        endpoint = os.getenv("FLAME_ENDPOINT")
        if endpoint is not None:
            self._endpoint = endpoint

        cache_endpoint = os.getenv("FLAME_CACHE_ENDPOINT")
        if cache_endpoint is not None:
            self._cache_endpoint = cache_endpoint


class DataSource(IntEnum):
    """Data location enumeration."""

    LOCAL = 0
    REMOTE = 1


@dataclass
class DataExpr:
    """Data expression."""

    source: DataSource
    url: Optional[str] = None
    version: int = 0
    data: Optional[bytes] = None

    def to_json(self) -> bytes:
        data = asdict(self)
        # For remote data, the data is not included in the JSON
        if self.source == DataSource.REMOTE:
            data["data"] = None

        return bson.dumps(data)

    @classmethod
    def from_json(cls, json_data: bytes) -> "DataExpr":
        data = bson.loads(json_data)
        return cls(**data)

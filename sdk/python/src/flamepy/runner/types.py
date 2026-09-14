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
from dataclasses import dataclass, field
from typing import Any, Dict, Optional, Tuple


@dataclass
class SessionContext:
    """Context for customizing Flame session creation in Runner.service().

    Users can attach this context to their execution objects (classes, instances,
    or functions) via the `_session_context` attribute to customize session
    behavior, particularly the session ID.

    Attributes:
        session_id: Custom session identifier. If provided, this ID will be used
                   when creating the session instead of an auto-generated one.
                   Must be unique across all active sessions. If None (default),
                   a random ID will be generated using short_name(app).
        application_name: Optional application name for logging and debugging.
                         Currently used for local context only, not persisted.

    Example with a class:
        >>> class MyService:
        ...     _session_context = SessionContext(
        ...         session_id="my-session-001",
        ...         application_name="my-app"
        ...     )
        ...     def process(self, data):
        ...         return data * 2

    Example with an instance:
        >>> obj = MyClass()
        >>> obj._session_context = SessionContext(session_id="instance-001")

    Example with a function:
        >>> def my_func(x):
        ...     return x * 2
        >>> my_func._session_context = SessionContext(session_id="func-001")
    """

    session_id: Optional[str] = None
    application_name: Optional[str] = None

    def __post_init__(self) -> None:
        """Validate SessionContext fields."""
        if self.session_id is not None:
            if not isinstance(self.session_id, str):
                raise ValueError(f"session_id must be a string, got {type(self.session_id)}")
            if len(self.session_id) == 0:
                raise ValueError("session_id cannot be empty string")
            if len(self.session_id) > 128:
                raise ValueError(f"session_id too long ({len(self.session_id)} chars, max 128)")

        if self.application_name is not None and not isinstance(self.application_name, str):
            raise ValueError(f"application_name must be a string, got {type(self.application_name)}")


@dataclass
class RunnerContext:
    """Context for runner session containing the shared execution object.

    Attributes:
        execution_object: The execution object for the customized session.
        stateful: Derived from execution_object type unless explicitly provided.
                  Object instances are always stateful. Functions, builtins, and
                  classes are always stateless. A class execution object may be
                  reused in one executor process, but its state is not persisted.
        autoscale: If True, create instances dynamically (min=warmup or 0, max=None).
                   If False, create fixed instances (min=max=warmup or 1). Object
                   instances are always fixed.
        warmup: Number of instances to pre-create. When autoscale=True, sets min_instances.
                When autoscale=False, sets both min_instances and max_instances.
                Object instances only support warmup values 0 and 1.
        min_instances: Minimum number of instances (computed from autoscale and warmup)
        max_instances: Maximum number of instances (computed from autoscale and warmup)
    """

    execution_object: Any
    stateful: Optional[bool] = None
    autoscale: Optional[bool] = None
    warmup: int = 0
    min_instances: int = field(init=False, repr=False)
    max_instances: Optional[int] = field(init=False, repr=False)

    def __post_init__(self) -> None:
        """Compute min/max instances and validate configuration."""
        if self.warmup < 0:
            raise ValueError("warmup must be a non-negative integer.")

        is_function = callable(self.execution_object) and not inspect.isclass(self.execution_object)
        is_class = inspect.isclass(self.execution_object)
        is_object = not is_function and not is_class

        if is_object:
            if self.stateful is False:
                raise ValueError("Object instance services are always stateful. Omit stateful or pass stateful=True.")
            if self.autoscale is True:
                raise ValueError("Object instance services are always fixed. Omit autoscale or pass autoscale=False.")
            if self.warmup not in (0, 1):
                raise ValueError("Object instance services only support warmup=0 or warmup=1.")
            self.stateful = True
            self.autoscale = False
        else:
            if self.stateful is True:
                raise ValueError("Functions, builtins, and classes are always stateless. Omit stateful or pass stateful=False.")
            self.stateful = False
            self.autoscale = True if self.autoscale is None else self.autoscale

        default_min = 0 if self.autoscale else 1
        self.min_instances = self.warmup if self.warmup > 0 else default_min
        self.max_instances = None if self.autoscale else self.min_instances


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

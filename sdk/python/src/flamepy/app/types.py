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

import functools
import inspect
from dataclasses import dataclass, field
from typing import Any, Dict, Optional, Tuple


def _is_function(execution_object: Any) -> bool:
    """Return whether an execution object has function rather than object semantics."""
    return inspect.isroutine(execution_object) or isinstance(
        execution_object,
        functools.partial,
    )


@dataclass
class ServiceContext:
    """Context for app session containing the shared execution object.

    Attributes:
        execution_object: The execution object for the customized session.
        constructor_args: None for a function service. A tuple, including an
                          empty tuple, for a class constructed by the executor.
        constructor_kwargs: Keyword arguments for executor-side construction.
        service_id: Stable identity used to retain a constructed object in an
                    executor across session bindings.
        autoscale: If True, create instances dynamically (min=warmup or 0, max=None).
                   If False, create fixed instances (min=max=warmup or 1).
        warmup: Number of instances to pre-create. When autoscale=True, sets min_instances.
                When autoscale=False, sets both min_instances and max_instances.
        min_instances: Minimum number of instances (computed from autoscale and warmup)
        max_instances: Maximum number of instances (computed from autoscale and warmup)
    """

    execution_object: Any
    constructor_args: Optional[Tuple[Any, ...]] = None
    constructor_kwargs: Dict[str, Any] = field(default_factory=dict)
    service_id: Optional[str] = None
    autoscale: Optional[bool] = None
    warmup: int = 0
    min_instances: int = field(init=False, repr=False)
    max_instances: Optional[int] = field(init=False, repr=False)

    def __post_init__(self) -> None:
        """Compute min/max instances and validate configuration."""
        if self.warmup < 0:
            raise ValueError("warmup must be a non-negative integer.")

        is_function = _is_function(self.execution_object)
        is_class = inspect.isclass(self.execution_object)

        if not is_function and not is_class:
            raise TypeError("execution_object must be a function or class service definition.")
        if is_function:
            if self.constructor_args is not None:
                raise ValueError("Function services do not accept constructor arguments.")
            if self.constructor_kwargs:
                raise ValueError("constructor_kwargs require constructor_args for a class service.")
        elif self.constructor_args is None:
            raise ValueError("Class services require constructor_args; use () for a zero-argument constructor.")

        self.autoscale = True if self.autoscale is None else self.autoscale

        default_min = 0 if self.autoscale else 1
        self.min_instances = self.warmup if self.warmup > 0 else default_min
        self.max_instances = None if self.autoscale else self.min_instances


@dataclass
class ServiceRequest:
    """Request for app task invocation.

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
        """Validate ServiceRequest fields."""
        if self.method is not None and not isinstance(self.method, str):
            raise ValueError(f"method must be a string or None, got {type(self.method)}")
        if self.args is not None and not isinstance(self.args, (tuple, list)):
            raise ValueError(f"args must be a tuple or list, got {type(self.args)}")
        if self.kwargs is not None and not isinstance(self.kwargs, dict):
            raise ValueError(f"kwargs must be a dict, got {type(self.kwargs)}")

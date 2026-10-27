"""
Copyright 2026 The Flame Authors.
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

# Invocation-scoped state used internally by Flame app executors.

from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Iterable, Iterator, Optional

from flamepy.core.service import SessionContext, TaskContext, _AttributeDelta, _AttributeOp, _validate_attributes
from flamepy.proto.types_pb2 import ExecutorAttributes


@dataclass(frozen=True)
class InstanceContext:
    """What the current App invocation knows about its session and task.

    Attributes:
        session_id: The session the invocation runs in.
        task_id: The task being invoked.
        batch_size: The session's gang size, i.e. the number of executors it
            needs together (1 if the session is not a gang). A task that
            blocks until ``batch_size`` members have joined, such as a
            collective ``init_process_group``, can use it as the world size.
    """

    session_id: str
    task_id: Optional[str]
    batch_size: int


@dataclass
class _InvocationContext:
    session_context: SessionContext
    attributes: _AttributeDelta
    task_context: Optional[TaskContext] = None


_current_invocation_context: ContextVar[Optional[_InvocationContext]] = ContextVar(
    "flamepy_app_invocation_context",
    default=None,
)

# The core servicer asks the service for response attributes immediately after
# ``on_task_invoke`` returns. Keeping this handoff in a ContextVar preserves the
# invoking thread/task's attributes until that response is assembled, without
# putting concurrent invocations through the core service's shared publisher.
_pending_response_attributes: ContextVar[Optional[ExecutorAttributes]] = ContextVar(
    "flamepy_app_pending_response_attributes",
    default=None,
)


def session_context() -> SessionContext:
    """Return the session bound to the current App invocation."""
    context = _current_invocation_context.get()
    if context is None:
        raise RuntimeError("App code is not running in a Flame invocation")
    return context.session_context


def context() -> InstanceContext:
    """Return the session id, task id and ``batch_size`` of the current App invocation."""
    invocation = _current_invocation_context.get()
    if invocation is None:
        raise RuntimeError("App code is not running in a Flame invocation")
    task = invocation.task_context
    return InstanceContext(
        session_id=invocation.session_context.session_id,
        task_id=task.task_id if task is not None else None,
        batch_size=invocation.session_context.batch_size,
    )


def _current_invocation() -> _InvocationContext:
    context = _current_invocation_context.get()
    if context is None:
        raise RuntimeError("App code is not running in a Flame invocation")
    return context


def publish_attributes(attributes: Iterable[bytes]) -> None:
    """Publish executor-local affinity attributes for the current response."""
    context = _current_invocation()
    context.attributes.record(_validate_attributes(attributes), _AttributeOp.PUBLISH)


def retract_attributes(attributes: Iterable[bytes]) -> None:
    """Retract affinity attributes that are no longer valid, e.g. after a cache eviction.

    The last of ``publish_attributes``/``retract_attributes`` on a key within
    one invocation wins.
    """
    context = _current_invocation()
    context.attributes.record(_validate_attributes(attributes), _AttributeOp.RETRACT)


@contextmanager
def _bind_invocation_context(
    session: SessionContext,
    task: Optional[TaskContext] = None,
) -> Iterator[_InvocationContext]:
    """Bind runtime capabilities while user App code is executing."""
    invocation_context = _InvocationContext(
        session_context=session,
        attributes=_AttributeDelta(),
        task_context=task,
    )
    token = _current_invocation_context.set(invocation_context)
    try:
        yield invocation_context
    finally:
        _current_invocation_context.reset(token)


def _stage_response_attributes(attributes: _AttributeDelta) -> None:
    """Stage attributes for the current invocation's shim response."""
    _pending_response_attributes.set(attributes.take())


def _take_response_attributes() -> Optional[ExecutorAttributes]:
    """Take attributes staged for the current invocation, if any."""
    attributes = _pending_response_attributes.get()
    if attributes is not None:
        _pending_response_attributes.set(None)
    return attributes

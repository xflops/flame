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
from typing import FrozenSet, Iterable, Iterator, Optional

from flamepy.core.service import SessionContext, _validate_attributes


@dataclass
class _InvocationContext:
    session_context: SessionContext
    attributes: set[bytes]


_current_invocation_context: ContextVar[Optional[_InvocationContext]] = ContextVar(
    "flamepy_app_invocation_context",
    default=None,
)

# The core servicer asks the service for response attributes immediately after
# ``on_task_invoke`` returns. Keeping this handoff in a ContextVar preserves the
# invoking thread/task's attributes until that response is assembled, without
# putting concurrent invocations through the core service's shared publisher.
_pending_response_attributes: ContextVar[Optional[FrozenSet[bytes]]] = ContextVar(
    "flamepy_app_pending_response_attributes",
    default=None,
)


def session_context() -> SessionContext:
    """Return the session bound to the current App invocation."""
    context = _current_invocation_context.get()
    if context is None:
        raise RuntimeError("App code is not running in a Flame invocation")
    return context.session_context


def publish_attributes(attributes: Iterable[bytes]) -> None:
    """Publish executor-local affinity attributes for the current response."""
    context = _current_invocation_context.get()
    if context is None:
        raise RuntimeError("App code is not running in a Flame invocation")
    incoming = _validate_attributes(attributes)
    combined = _validate_attributes((*context.attributes, *incoming))
    context.attributes.clear()
    context.attributes.update(combined)


@contextmanager
def _bind_invocation_context(
    context: SessionContext,
) -> Iterator[_InvocationContext]:
    """Bind runtime capabilities while user App code is executing."""
    invocation_context = _InvocationContext(
        session_context=context,
        attributes=set(),
    )
    token = _current_invocation_context.set(invocation_context)
    try:
        yield invocation_context
    finally:
        _current_invocation_context.reset(token)


def _stage_response_attributes(attributes: Iterable[bytes]) -> None:
    """Stage attributes for the current invocation's shim response."""
    _pending_response_attributes.set(_validate_attributes(attributes))


def _take_response_attributes() -> Optional[FrozenSet[bytes]]:
    """Take attributes staged for the current invocation, if any."""
    attributes = _pending_response_attributes.get()
    if attributes is not None:
        _pending_response_attributes.set(None)
    return attributes

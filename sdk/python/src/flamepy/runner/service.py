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

from typing import Iterable, Optional

from flamepy.core.service import SessionContext, _validate_attributes


class RunnerService:
    """Base class for Runner execution objects that use Flame runtime state."""

    _flame_session_context: Optional[SessionContext] = None

    def session_context(self) -> SessionContext:
        """Return the session context currently bound to this service instance."""
        context = self._flame_session_context
        if context is None:
            raise RuntimeError("Runner service is not bound to a session")
        return context

    def publish_attributes(self, attributes: Iterable[bytes]) -> None:
        """Add locality attributes to the current Runner response."""
        incoming = _validate_attributes(attributes)
        published = getattr(self, "_flame_instance_attributes", None)
        if published is None:
            published = set()
        candidate = _validate_attributes((*published, *incoming))
        self._flame_instance_attributes = set(candidate)

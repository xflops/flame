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

from flamepy.app._context import publish_attributes, session_context
from flamepy.app.types import ServiceContext, ServiceRequest

# The private restoration hook remains available for older serialized proxies
# but stays outside ``__all__``.
from flamepy.app.client import (  # isort: skip
    ObjectFuture,
    ObjectFutureIterator,
    ServiceInstance,
    _restore_service_instance as _restore_service_instance,
    destroy,
    get,
    init,
    put,
    ref,
    select,
    service,
    wait,
)

__all__ = [
    "ObjectFuture",
    "ObjectFutureIterator",
    "ServiceContext",
    "ServiceInstance",
    "ServiceRequest",
    "destroy",
    "get",
    "init",
    "put",
    "publish_attributes",
    "ref",
    "select",
    "service",
    "session_context",
    "wait",
]

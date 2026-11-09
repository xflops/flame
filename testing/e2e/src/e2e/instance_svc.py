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

import base64
import os
import time
from pathlib import Path

from flamepy import serving
from flamepy.core import ObjectRef, patch_object

from e2e.api import TestContext, TestRequest, TestResponse


class Instance(serving.Instance):
    """Serving fixture with optional cache-backed lifecycle observations."""

    def __init__(self):
        super().__init__()
        observer = os.environ.get("FLAME_E2E_LIFECYCLE_EVENTS")
        self.events = ObjectRef.decode(base64.b64decode(observer)) if observer else None
        self.executor_id = None
        if self.events is not None:
            endpoint = Path(os.environ["FLAME_INSTANCE_ENDPOINT"])
            self.executor_id = endpoint.parent.name if endpoint.name == "instance.sock" else endpoint.stem
        self.setup_complete = False
        self.session_id = None

    def record(self, phase, **attributes):
        if self.events is None:
            return
        self.events = patch_object(
            self.events,
            {
                "phase": phase,
                "executor_id": self.executor_id,
                "pid": os.getpid(),
                "session_id": self.session_id,
                "at": time.monotonic(),
                **attributes,
            },
        )

    def on_session_enter(self, context):
        self.session_id = context.session_id
        self.record("session_enter", setup_complete=self.setup_complete)
        if self.events is not None and not self.setup_complete:
            raise RuntimeError("instance setup must finish before session enter")
        super().on_session_enter(context)

    def on_session_leave(self):
        self.record("session_leave")
        super().on_session_leave()
        self.session_id = None


instance = Instance()


@instance.setup
def setup():
    instance.record("setup_started")
    delay = float(os.environ.get("FLAME_E2E_SETUP_DELAY", "0"))
    if delay:
        time.sleep(delay)
    instance.setup_complete = True
    instance.record("setup_complete")


@instance.teardown
def teardown():
    instance.record("teardown")


@instance.entrypoint
def e2e_service_entrypoint(req: TestRequest) -> TestResponse:
    instance.record("task")
    cxt = instance.context()
    data = cxt.common_data if cxt is not None else None

    if req.update_common_data:
        instance.patch_context(TestContext(common_data=req.input))

    return TestResponse(output=req.input, common_data=data)


if __name__ == "__main__":
    instance.run()

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

Longevity test (session API): keep one session active for a sustained period.

    FLAME_E2E_SYSTEM_TESTS=longevity pytest system/longevity/test_session_longevity.py -m longevity
"""

import time
from contextlib import suppress
import flamepy
import pytest
from e2e.api import TestContext
from e2e.api import TestRequest as E2ETestRequest
from e2e.helpers import invoke_task, serialize_common_data
from tests.utils import random_string

from system_common import (
    FLM_SYSTEM_TEST_APP,
    _cluster_snapshot,
    _env_float,
    _env_int,
    _numeric_summary,
    _payload,
    _requires_system_profile,
    _write_system_report,
)


pytestmark = pytest.mark.usefixtures("setup_system_test_env")


@pytest.mark.longevity
@_requires_system_profile("longevity")
def test_single_session_longevity():
    """Keep one session active for a sustained period and verify it stays healthy."""
    duration_seconds = _env_float("FLAME_LONGEVITY_DURATION_SECONDS", 60.0, minimum=1.0)
    interval_seconds = _env_float("FLAME_LONGEVITY_INTERVAL_SECONDS", 1.0, minimum=0.0)
    payload_bytes = _env_int("FLAME_LONGEVITY_PAYLOAD_BYTES", 256)
    common_data = _payload(
        f"longevity-common:{random_string(8)}",
        max(32, min(payload_bytes, 4096)),
    )

    session = flamepy.create_session(
        application=FLM_SYSTEM_TEST_APP,
        session_id=f"system-longevity-{random_string(8)}",
        common_data=serialize_common_data(
            TestContext(common_data=common_data),
            FLM_SYSTEM_TEST_APP,
        ),
        min_instances=0,
        max_instances=1,
    )

    completed_tasks = 0
    started_at = time.monotonic()
    perf_started_at = time.perf_counter()
    deadline = started_at + duration_seconds
    task_latency_ms = []
    cluster_before = _cluster_snapshot()

    try:
        while time.monotonic() < deadline or completed_tasks == 0:
            task_started_at = time.perf_counter()
            input_value = _payload(
                f"{session.id}:input-tick-{completed_tasks}",
                payload_bytes,
            )
            output_value = _payload(
                f"{session.id}:output-tick-{completed_tasks}",
                payload_bytes,
            )
            response = invoke_task(
                session,
                E2ETestRequest(
                    input=input_value,
                    output=output_value,
                ),
            )
            assert response.output == output_value
            assert response.common_data == common_data
            completed_tasks += 1
            task_latency_ms.append(round((time.perf_counter() - task_started_at) * 1000, 3))

            refreshed = flamepy.get_session(session.id)
            assert refreshed.state == flamepy.SessionState.OPEN
            assert refreshed.failed == 0

            if interval_seconds > 0:
                elapsed = time.monotonic() - task_started_at
                remaining = deadline - time.monotonic()
                sleep_seconds = min(max(0.0, interval_seconds - elapsed), max(0.0, remaining))
                if sleep_seconds > 0:
                    time.sleep(sleep_seconds)

        elapsed_seconds = time.monotonic() - started_at
        perf_elapsed_seconds = time.perf_counter() - perf_started_at
        final_session = flamepy.get_session(session.id)
        assert final_session.succeed >= completed_tasks
        assert final_session.failed == 0
        _write_system_report(
            "longevity",
            {
                "cluster": {
                    "after": _cluster_snapshot(),
                    "before": cluster_before,
                },
                "configuration": {
                    "common_data_bytes": len(common_data),
                    "interval_seconds": interval_seconds,
                    "requested_duration_seconds": duration_seconds,
                    "task_payload_bytes": payload_bytes,
                },
                "result": {
                    "completed_tasks": completed_tasks,
                    "duration_seconds": round(elapsed_seconds, 3),
                    "task_rate": round(completed_tasks / perf_elapsed_seconds, 3) if perf_elapsed_seconds > 0 else 0,
                },
                "session": {
                    "actual_succeed": final_session.succeed,
                    "failed": final_session.failed,
                    "session_id": session.id,
                    "state": final_session.state.name,
                },
                "task_latency_ms": _numeric_summary(task_latency_ms),
            },
        )

    finally:
        with suppress(Exception):
            session.close()

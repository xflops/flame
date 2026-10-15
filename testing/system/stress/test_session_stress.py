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

Stress test (session API): parallel sessions with a fuzzed session/task workload.

    FLAME_E2E_SYSTEM_TESTS=stress pytest system/stress/test_session_stress.py -m stress
"""

import json
import random
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import suppress
from dataclasses import replace
import flamepy
import pytest
from e2e.api import TestContext
from e2e.api import TestRequest as E2ETestRequest
from e2e.helpers import invoke_task, serialize_common_data

from system_common import (
    FLM_SYSTEM_TEST_APP,
    FuzzedTaskSpec,
    _cluster_snapshot,
    _env_int,
    _fuzz_stress_workload,
    _numeric_summary,
    _requires_system_profile,
    _stress_profile,
    _write_system_report,
)


pytestmark = pytest.mark.usefixtures("setup_system_test_env")


@pytest.mark.stress
@_requires_system_profile("stress")
def test_parallel_sessions_task_stress():
    """Stress scheduling with a fuzzed session/task workload."""
    profile_name, base_profile = _stress_profile()
    profile = replace(
        base_profile,
        sessions=_env_int("FLAME_STRESS_SESSIONS", base_profile.sessions),
        tasks=_env_int("FLAME_STRESS_TASKS", base_profile.tasks),
        workers=_env_int("FLAME_STRESS_WORKERS", base_profile.workers),
    )
    seed = _env_int("FLAME_STRESS_RANDOM_SEED", random.randrange(1, 2**31))
    rng = random.Random(seed)
    session_specs, task_specs = _fuzz_stress_workload(rng, profile)

    sessions = []
    cluster_before = _cluster_snapshot()
    workload_started_at = time.perf_counter()
    task_latency_ms = []

    try:
        for session_spec in session_specs:
            session = flamepy.create_session(
                application=FLM_SYSTEM_TEST_APP,
                session_id=session_spec.session_id,
                common_data=serialize_common_data(
                    TestContext(common_data=session_spec.common_data),
                    FLM_SYSTEM_TEST_APP,
                ),
                min_instances=0,
                max_instances=session_spec.max_instances,
            )
            sessions.append(session)

        total_tasks = len(task_specs)
        if total_tasks == 0:
            pytest.fail("stress workload generated no tasks")

        worker_count = min(profile.workers, total_tasks)
        expected_counts = Counter(task_spec.session_index for task_spec in task_specs)
        print(
            "FLAME_STRESS_WORKLOAD "
            + json.dumps(
                {
                    "max_instances": profile.max_instances,
                    "max_payload_bytes": profile.max_payload_bytes,
                    "max_sleep_ms": profile.max_sleep_ms,
                    "profile": profile_name,
                    "seed": seed,
                    "sessions": profile.sessions,
                    "tasks": total_tasks,
                    "tasks_per_session_max": max(expected_counts.values()),
                    "tasks_per_session_min": min(expected_counts.values()),
                    "workers": worker_count,
                },
                sort_keys=True,
            )
        )

        def run_task(task_spec: FuzzedTaskSpec) -> tuple[int, str, str, float]:
            session = sessions[task_spec.session_index]
            task_started_at = time.perf_counter()
            response = invoke_task(
                session,
                E2ETestRequest(
                    input=task_spec.input_value,
                    output=task_spec.output_value,
                    sleep_ms=task_spec.sleep_ms,
                ),
            )
            elapsed_ms = (time.perf_counter() - task_started_at) * 1000
            assert response.output == task_spec.output_value
            assert response.common_data == task_spec.common_data
            return task_spec.session_index, session.id, response.output, elapsed_ms

        outputs_by_session = Counter()
        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            futures = [executor.submit(run_task, task_spec) for task_spec in task_specs]
            for future in as_completed(futures):
                session_index, session_id, output, elapsed_ms = future.result()
                assert output.startswith("output:")
                outputs_by_session[session_index] += 1
                task_latency_ms.append(round(elapsed_ms, 3))

        assert outputs_by_session == expected_counts
        session_results = []
        for index, session in enumerate(sessions):
            refreshed = flamepy.get_session(session.id)
            assert refreshed.state == flamepy.SessionState.OPEN
            assert refreshed.succeed >= expected_counts[index]
            assert refreshed.failed == 0
            session_results.append(
                {
                    "actual_succeed": refreshed.succeed,
                    "expected_tasks": expected_counts[index],
                    "failed": refreshed.failed,
                    "common_data_bytes": len(session_specs[index].common_data),
                    "max_instances": session_specs[index].max_instances,
                    "session_id": session.id,
                    "state": refreshed.state.name,
                }
            )

        elapsed_seconds = time.perf_counter() - workload_started_at
        cluster_after = _cluster_snapshot()
        _write_system_report(
            "stress",
            {
                "cluster": {
                    "after": cluster_after,
                    "before": cluster_before,
                },
                "result": {
                    "elapsed_seconds": round(elapsed_seconds, 3),
                    "tasks_per_second": round(total_tasks / elapsed_seconds, 3) if elapsed_seconds > 0 else 0,
                },
                "sessions": session_results,
                "task_latency_ms": _numeric_summary(task_latency_ms),
                "workload": {
                    "max_instances": profile.max_instances,
                    "max_payload_bytes": profile.max_payload_bytes,
                    "max_sleep_ms": profile.max_sleep_ms,
                    "common_data_bytes": _numeric_summary(task_spec.common_data_bytes for task_spec in task_specs),
                    "input_bytes": _numeric_summary(task_spec.input_bytes for task_spec in task_specs),
                    "output_bytes": _numeric_summary(task_spec.output_bytes for task_spec in task_specs),
                    "profile": profile_name,
                    "seed": seed,
                    "service_sleep_ms": _numeric_summary(task_spec.sleep_ms for task_spec in task_specs),
                    "sessions": profile.sessions,
                    "tasks": total_tasks,
                    "tasks_per_session": _numeric_summary(expected_counts.values()),
                    "workers": worker_count,
                },
            },
        )

    finally:
        for session in sessions:
            with suppress(Exception):
                session.close()

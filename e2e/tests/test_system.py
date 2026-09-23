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

Opt-in system tests for heavier Flame workloads.

These tests are skipped by default so the normal E2E suite stays fast and
predictable. Enable one or more profiles with FLAME_E2E_SYSTEM_TESTS:

    FLAME_E2E_SYSTEM_TESTS=stress pytest tests/test_system.py -m stress
    FLAME_E2E_SYSTEM_TESTS=longevity pytest tests/test_system.py -m longevity
    FLAME_E2E_SYSTEM_TESTS=app pytest tests/test_system.py -m app
    FLAME_E2E_SYSTEM_TESTS=all pytest tests/test_system.py
"""

import json
import math
import os
import random
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import suppress
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path

import flamepy
import flamepy.app as app
import pytest
from flamepy.proto import types_pb2

from e2e.api import TestContext
from e2e.api import TestRequest as E2ETestRequest
from e2e.helpers import invoke_task, serialize_common_data
from tests.utils import random_string

FLM_SYSTEM_TEST_APP = "flme2e-system-svc"
SYSTEM_TESTS_ENV = "FLAME_E2E_SYSTEM_TESTS"
SYSTEM_REPORT_DIR_ENV = "FLAME_SYSTEM_REPORT_DIR"
ALL_PROFILE_VALUES = {"1", "true", "yes", "all"}


@dataclass(frozen=True)
class StressProfile:
    sessions: int
    tasks: int
    workers: int
    max_instances: int
    max_payload_bytes: int
    max_sleep_ms: int


@dataclass(frozen=True)
class FuzzedSessionSpec:
    index: int
    session_id: str
    max_instances: int
    common_data: str


@dataclass(frozen=True)
class FuzzedTaskSpec:
    session_index: int
    sequence: int
    input_bytes: int
    output_bytes: int
    common_data_bytes: int
    sleep_ms: int
    input_value: str
    output_value: str
    common_data: str


STRESS_PROFILES = {
    "smoke": StressProfile(
        sessions=4,
        tasks=24,
        workers=8,
        max_instances=2,
        max_payload_bytes=1024,
        max_sleep_ms=100,
    ),
    "default": StressProfile(
        sessions=20,
        tasks=250,
        workers=32,
        max_instances=4,
        max_payload_bytes=16 * 1024,
        max_sleep_ms=500,
    ),
    "heavy": StressProfile(
        sessions=100,
        tasks=2500,
        workers=128,
        max_instances=8,
        max_payload_bytes=64 * 1024,
        max_sleep_ms=2000,
    ),
}


def _system_profile_enabled(profile: str) -> bool:
    raw_value = os.getenv(SYSTEM_TESTS_ENV, "")
    tokens = {token.strip().lower() for token in raw_value.replace(",", " ").split() if token.strip()}
    return bool(tokens & ALL_PROFILE_VALUES) or profile in tokens


def _requires_system_profile(profile: str):
    return pytest.mark.skipif(
        not _system_profile_enabled(profile),
        reason=f"set {SYSTEM_TESTS_ENV}={profile} or all to run this opt-in system test",
    )


def _env_int(name: str, default: int, minimum: int = 1) -> int:
    raw_value = os.getenv(name)
    if raw_value is None:
        return default

    try:
        value = int(raw_value)
    except ValueError:
        pytest.fail(f"{name} must be an integer, got {raw_value!r}")

    if value < minimum:
        pytest.fail(f"{name} must be >= {minimum}, got {value}")

    return value


def _env_float(name: str, default: float, minimum: float = 0.0) -> float:
    raw_value = os.getenv(name)
    if raw_value is None:
        return default

    try:
        value = float(raw_value)
    except ValueError:
        pytest.fail(f"{name} must be a number, got {raw_value!r}")

    if value < minimum:
        pytest.fail(f"{name} must be >= {minimum}, got {value}")

    return value


def _enum_name(enum_type, value: int) -> str:
    try:
        return enum_type.Name(value)
    except ValueError:
        return str(value)


def _numeric_summary(values) -> dict:
    values = list(values)
    if not values:
        return {
            "count": 0,
            "min": None,
            "max": None,
            "avg": None,
            "p50": None,
            "p95": None,
        }

    ordered = sorted(values)

    def percentile(p: float):
        index = min(len(ordered) - 1, max(0, math.ceil(len(ordered) * p) - 1))
        return ordered[index]

    return {
        "count": len(ordered),
        "min": min(ordered),
        "max": max(ordered),
        "avg": round(sum(ordered) / len(ordered), 3),
        "p50": percentile(0.50),
        "p95": percentile(0.95),
    }


def _resource_report(resource) -> dict:
    return {
        "cpu": resource.cpu,
        "memory": resource.memory,
        "gpu": resource.gpu,
    }


def _cluster_snapshot() -> dict:
    nodes = flamepy.list_nodes()
    executors = flamepy.list_executors()

    node_items = []
    aggregate_capacity = Counter()
    aggregate_allocatable = Counter()
    node_states = Counter()
    for node in nodes:
        capacity = _resource_report(node.status.capacity)
        allocatable = _resource_report(node.status.allocatable)
        aggregate_capacity.update(capacity)
        aggregate_allocatable.update(allocatable)
        state = _enum_name(types_pb2.NodeState, node.status.state)
        node_states[state] += 1
        node_items.append(
            {
                "addresses": [
                    {
                        "address": address.address,
                        "type": address.type,
                    }
                    for address in node.status.addresses
                ],
                "allocatable": allocatable,
                "capacity": capacity,
                "hostname": node.spec.hostname,
                "id": node.metadata.id,
                "info": {
                    "arch": node.status.info.arch,
                    "os": node.status.info.os,
                },
                "last_heartbeat_time": node.status.last_heartbeat_time,
                "name": node.metadata.name,
                "state": state,
            }
        )

    executor_states = Counter()
    executors_by_node = {}
    for executor in executors:
        state = _enum_name(types_pb2.ExecutorState, executor.status.state)
        executor_states[state] += 1
        node_name = executor.spec.node
        node_counts = executors_by_node.setdefault(node_name, Counter())
        node_counts[state] += 1

    return {
        "executors": {
            "by_node": {node_name: dict(state_counts) for node_name, state_counts in sorted(executors_by_node.items())},
            "states": dict(executor_states),
            "total": len(executors),
        },
        "nodes": {
            "allocatable": dict(aggregate_allocatable),
            "capacity": dict(aggregate_capacity),
            "items": node_items,
            "states": dict(node_states),
            "total": len(nodes),
        },
    }


def _report_path(report_name: str) -> Path | None:
    explicit_path = os.getenv(f"FLAME_{report_name.upper()}_REPORT_PATH")
    if explicit_path:
        return Path(explicit_path)

    report_dir = os.getenv(SYSTEM_REPORT_DIR_ENV)
    if not report_dir:
        return None

    return Path(report_dir) / f"{report_name}.json"


def _write_system_report(report_name: str, report: dict):
    report = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "report": report_name,
        **report,
    }
    report_json = json.dumps(report, sort_keys=True)
    print(f"FLAME_{report_name.upper()}_REPORT {report_json}")

    path = _report_path(report_name)
    if path is None:
        return

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(report_json + "\n", encoding="utf-8")
    print(f"FLAME_{report_name.upper()}_REPORT_PATH {path}")


def _stress_profile() -> tuple[str, StressProfile]:
    profile_name = os.getenv("FLAME_STRESS_PROFILE", "default").strip().lower()
    if profile_name not in STRESS_PROFILES:
        allowed_profiles = ", ".join(sorted(STRESS_PROFILES))
        pytest.fail(f"FLAME_STRESS_PROFILE must be one of {allowed_profiles}, got {profile_name!r}")
    return profile_name, STRESS_PROFILES[profile_name]


def _fuzz_payload_bytes(rng: random.Random, profile: StressProfile) -> int:
    upper_bound = max(1, profile.max_payload_bytes)
    if upper_bound < 1024:
        return rng.randint(1, upper_bound)

    roll = rng.random()
    if roll < 0.65:
        return rng.randint(16, 1024)
    if roll < 0.9:
        return rng.randint(1024, min(8 * 1024, upper_bound))
    return rng.randint(min(8 * 1024, upper_bound), upper_bound)


def _fuzz_sleep_ms(rng: random.Random, profile: StressProfile) -> int:
    if profile.max_sleep_ms <= 0:
        return 0

    roll = rng.random()
    if roll < 0.45:
        return 0
    if roll < 0.8:
        return rng.randint(1, min(50, profile.max_sleep_ms))
    if roll < 0.95:
        return rng.randint(min(50, profile.max_sleep_ms), min(250, profile.max_sleep_ms))
    return rng.randint(min(250, profile.max_sleep_ms), profile.max_sleep_ms)


def _fuzz_stress_workload(
    rng: random.Random,
    profile: StressProfile,
) -> tuple[list[FuzzedSessionSpec], list[FuzzedTaskSpec]]:
    sessions = []
    for index in range(profile.sessions):
        common_data_bytes = _fuzz_payload_bytes(rng, profile)
        sessions.append(
            FuzzedSessionSpec(
                index=index,
                session_id=f"system-stress-{index}-{rng.getrandbits(32):08x}",
                max_instances=rng.randint(1, profile.max_instances),
                common_data=_payload(
                    f"common:session-{index}:{rng.getrandbits(32):08x}",
                    common_data_bytes,
                ),
            )
        )

    task_counts = [0 for _ in sessions]
    task_specs = []

    session_order = list(range(len(sessions)))
    rng.shuffle(session_order)
    initial_task_sessions = session_order[: min(profile.tasks, len(session_order))]
    remaining_task_sessions = [rng.randrange(len(sessions)) for _ in range(max(0, profile.tasks - len(initial_task_sessions)))]

    for session_index in initial_task_sessions + remaining_task_sessions:
        sequence = task_counts[session_index]
        task_counts[session_index] += 1
        input_bytes = _fuzz_payload_bytes(rng, profile)
        output_bytes = _fuzz_payload_bytes(rng, profile)
        sleep_ms = _fuzz_sleep_ms(rng, profile)
        common_data = sessions[session_index].common_data
        task_specs.append(
            FuzzedTaskSpec(
                session_index=session_index,
                sequence=sequence,
                input_bytes=input_bytes,
                output_bytes=output_bytes,
                common_data_bytes=len(common_data),
                sleep_ms=sleep_ms,
                input_value=_payload(
                    f"input:session-{session_index}:task-{sequence}:sleep-{sleep_ms}",
                    input_bytes,
                ),
                output_value=_payload(
                    f"output:session-{session_index}:task-{sequence}:sleep-{sleep_ms}",
                    output_bytes,
                ),
                common_data=common_data,
            )
        )

    rng.shuffle(task_specs)
    return sessions, task_specs


def _fuzz_app_workload(
    rng: random.Random,
    tasks: int,
    max_payload_bytes: int,
    max_sleep_ms: int,
) -> list[FuzzedTaskSpec]:
    profile = StressProfile(
        sessions=1,
        tasks=tasks,
        workers=tasks,
        max_instances=1,
        max_payload_bytes=max_payload_bytes,
        max_sleep_ms=max_sleep_ms,
    )
    task_specs = []
    for sequence in range(tasks):
        input_bytes = _fuzz_payload_bytes(rng, profile)
        output_bytes = _fuzz_payload_bytes(rng, profile)
        common_data_bytes = _fuzz_payload_bytes(rng, profile)
        sleep_ms = _fuzz_sleep_ms(rng, profile)
        task_specs.append(
            FuzzedTaskSpec(
                session_index=0,
                sequence=sequence,
                input_bytes=input_bytes,
                output_bytes=output_bytes,
                common_data_bytes=common_data_bytes,
                sleep_ms=sleep_ms,
                input_value=_payload(
                    f"app-input:task-{sequence}:sleep-{sleep_ms}",
                    input_bytes,
                ),
                output_value=_payload(
                    f"app-output:task-{sequence}:sleep-{sleep_ms}",
                    output_bytes,
                ),
                common_data=_payload(
                    f"app-common:task-{sequence}",
                    common_data_bytes,
                ),
            )
        )
    rng.shuffle(task_specs)
    return task_specs


def _payload(label: str, size_bytes: int) -> str:
    if size_bytes <= len(label):
        return label
    return f"{label}:{'x' * (size_bytes - len(label) - 1)}"


def _close_system_sessions():
    with suppress(Exception):
        sessions = flamepy.list_sessions()
        for session in sessions:
            if session.application != FLM_SYSTEM_TEST_APP:
                continue
            with suppress(Exception):
                flamepy.close_session(session.id)


def _unregister_system_application():
    with suppress(Exception):
        flamepy.unregister_application(FLM_SYSTEM_TEST_APP)


@pytest.fixture(scope="module", autouse=True)
def setup_system_test_env():
    """Register the reusable service application for opt-in system tests."""
    _close_system_sessions()
    _unregister_system_application()

    flamepy.register_application(
        FLM_SYSTEM_TEST_APP,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG", "PYTHONPATH": "/opt/e2e/src"},
            arguments=["src/e2e/basic_svc.py", "src/e2e/api.py"],
            installer="python",
        ),
    )

    yield

    _close_system_sessions()
    _unregister_system_application()


def _require_app_prerequisites():
    context = flamepy.FlameContext()
    has_package_storage = context.package is not None and getattr(context.package, "storage", None) is not None
    has_cache_endpoint = context.cache is not None
    if not has_package_storage and not has_cache_endpoint:
        pytest.skip("App system test requires cache.endpoint or package.storage in flame.yaml")

    template_name = context.app
    try:
        template_app = flamepy.get_application(template_name)
    except Exception as exc:
        pytest.skip(f"App system test requires registered app template app {template_name!r}: {exc}")

    if template_app is None:
        pytest.skip(f"App system test requires registered app template app {template_name!r}")

    return template_name


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


@pytest.mark.app
@_requires_system_profile("app")
def test_app_fuzzed_task_workload():
    """Exercise the App path with fuzzed input, output, and common-data payloads."""
    template_name = _require_app_prerequisites()
    task_count = _env_int("FLAME_APP_TASKS", 64)
    warmup = _env_int("FLAME_APP_WARMUP", 2, minimum=0)
    max_payload_bytes = _env_int("FLAME_APP_MAX_PAYLOAD_BYTES", 8 * 1024)
    max_sleep_ms = _env_int("FLAME_APP_MAX_SLEEP_MS", 200, minimum=0)
    seed = _env_int("FLAME_APP_RANDOM_SEED", random.randrange(1, 2**31))
    rng = random.Random(seed)
    task_specs = _fuzz_app_workload(
        rng,
        tasks=task_count,
        max_payload_bytes=max_payload_bytes,
        max_sleep_ms=max_sleep_ms,
    )

    cluster_before = _cluster_snapshot()
    app_name = f"system-app-{random_string(8)}"
    started_at = time.perf_counter()

    app.init(app_name)
    try:

        @app.service(warmup=warmup)
        def service(input_value, output_value, common_data, sleep_ms=0):
            if sleep_ms > 0:
                time.sleep(sleep_ms / 1000.0)
            return {
                "common_data": common_data,
                "input": input_value,
                "output": output_value,
            }

        futures = [
            service(
                task_spec.input_value,
                task_spec.output_value,
                task_spec.common_data,
                task_spec.sleep_ms,
            )
            for task_spec in task_specs
        ]
        results = app.get(futures)
    finally:
        app.destroy()

    elapsed_seconds = time.perf_counter() - started_at
    for task_spec, result in zip(task_specs, results):
        assert result == {
            "common_data": task_spec.common_data,
            "input": task_spec.input_value,
            "output": task_spec.output_value,
        }

    _write_system_report(
        "app",
        {
            "cluster": {
                "after": _cluster_snapshot(),
                "before": cluster_before,
            },
            "result": {
                "elapsed_seconds": round(elapsed_seconds, 3),
                "tasks_per_second": round(task_count / elapsed_seconds, 3) if elapsed_seconds > 0 else 0,
            },
            "app": {
                "name": app_name,
                "template": template_name,
            },
            "workload": {
                "common_data_bytes": _numeric_summary(task_spec.common_data_bytes for task_spec in task_specs),
                "input_bytes": _numeric_summary(task_spec.input_bytes for task_spec in task_specs),
                "max_payload_bytes": max_payload_bytes,
                "max_sleep_ms": max_sleep_ms,
                "output_bytes": _numeric_summary(task_spec.output_bytes for task_spec in task_specs),
                "seed": seed,
                "service_sleep_ms": _numeric_summary(task_spec.sleep_ms for task_spec in task_specs),
                "tasks": task_count,
                "warmup": warmup,
            },
        },
    )

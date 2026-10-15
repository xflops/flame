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

Shared helpers for the Flame system tests (stress, longevity and App).
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
from tests.utils import deploy_e2e_application, random_string

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


def _requires_system_profile(*profiles: str):
    return pytest.mark.skipif(
        not any(_system_profile_enabled(profile) for profile in profiles),
        reason=f"set {SYSTEM_TESTS_ENV}={' or '.join(profiles)} (or all) to run this opt-in system test",
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

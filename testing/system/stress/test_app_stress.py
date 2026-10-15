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

App test: fuzzed input, output and common-data payloads through the App path.

    FLAME_E2E_SYSTEM_TESTS=stress pytest system/stress/test_app_stress.py
"""

import random
import time
import flamepy
import flamepy.app as app
import pytest
from tests.utils import random_string

from system_common import (
    _cluster_snapshot,
    _env_int,
    _fuzz_app_workload,
    _numeric_summary,
    _requires_system_profile,
    _write_system_report,
)


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
@pytest.mark.app
@_requires_system_profile("stress", "app")
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
            service.remote(
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

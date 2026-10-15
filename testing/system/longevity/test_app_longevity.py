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

Longevity test (flamepy.app): keep an App service busy with batches of tasks for a
sustained period and verify every result and that throughput does not collapse.

    FLAME_E2E_SYSTEM_TESTS=longevity pytest system/longevity/test_app_longevity.py

    FLAME_LONGEVITY_DURATION_SECONDS   total run time (default 60)
    FLAME_LONGEVITY_BATCH              tasks per batch (default 100)
"""

import time

import flamepy
import flamepy.app as app
import pytest
from tests.utils import random_string

from system_common import (
    _cluster_snapshot,
    _env_float,
    _env_int,
    _numeric_summary,
    _requires_system_profile,
    _write_system_report,
)


@pytest.mark.longevity
@_requires_system_profile("longevity")
def test_app_longevity():
    """Run batches of App tasks until the duration elapses; every task must return its input."""
    context = flamepy.FlameContext()
    if context.cache is None and not (context.package is not None and getattr(context.package, "storage", None) is not None):
        pytest.skip("App system test requires cache.endpoint or package.storage in flame.yaml")

    duration_seconds = _env_float("FLAME_LONGEVITY_DURATION_SECONDS", 60.0, minimum=1.0)
    batch_size = _env_int("FLAME_LONGEVITY_BATCH", 100)
    cluster_before = _cluster_snapshot()
    app_name = f"system-longevity-{random_string(8)}"
    batch_seconds = []
    started_at = time.perf_counter()

    app.init(app_name)
    try:

        @app.service(warmup=2)
        def echo(value):
            return value

        batch = 0
        while time.perf_counter() - started_at < duration_seconds:
            batch += 1
            inputs = [f"{batch}:{index}" for index in range(batch_size)]
            batch_started = time.perf_counter()
            results = app.get([echo.remote(value) for value in inputs])
            batch_seconds.append(time.perf_counter() - batch_started)
            assert results == inputs, f"batch {batch} returned wrong results"
    finally:
        app.destroy()

    elapsed_seconds = time.perf_counter() - started_at
    first, last = batch_seconds[: max(1, len(batch_seconds) // 4)], batch_seconds[-max(1, len(batch_seconds) // 4) :]
    # A slow leak shows up as later batches taking far longer than early ones.
    assert sum(last) / len(last) < 5 * sum(first) / len(first) + 1.0, "batch time grew more than 5x"

    _write_system_report(
        "app-longevity",
        {
            "app": {"name": app_name},
            "cluster": {"before": cluster_before, "after": _cluster_snapshot()},
            "result": {
                "batches": len(batch_seconds),
                "batch_seconds": _numeric_summary(batch_seconds),
                "elapsed_seconds": round(elapsed_seconds, 3),
                "tasks": len(batch_seconds) * batch_size,
            },
        },
    )

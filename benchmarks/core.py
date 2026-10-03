# Copyright 2026 The Flame Authors.
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#     http://www.apache.org/licenses/LICENSE-2.0
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Benchmark flamepy.core against the Rust benchmark's flmping case matrix.

Run from the benchmarks directory with a configured Flame client:
    python3 core.py

Each timed sample creates and closes its sessions. Incorrect responses fail
the benchmark.
"""

import json
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass

from flamepy import core


@dataclass(frozen=True)
class Case:
    phase: str
    sessions: int
    tasks_per_session: int
    repetitions: int


COLD_CASE = Case("cold-start", 1, 1, 1)
SCALE_OUT_CASE = Case("scale-out", 10, 1, 1)
WARMUP_CASE = Case("warm-up", 4, 100, 1)
STEADY_CASES = (
    Case("steady-state", 1, 1, 5),
    Case("steady-state", 1, 1000, 3),
    Case("steady-state", 10, 1, 5),
    Case("steady-state", 10, 1000, 3),
)


def run_session(tasks: int) -> None:
    session = core.create_session("flmping", session=f"python-core-benchmark-{uuid.uuid4().hex}")
    try:
        futures = [session.submit(b"{}") for _ in range(tasks)]
        outputs = [json.loads(future.result()) for future in futures]
        if any(not isinstance(output.get("message"), str) for output in outputs):
            raise RuntimeError(f"session returned incorrect flmping results for {tasks} tasks")
    finally:
        session.close()


def run_sample(pool: ThreadPoolExecutor, case: Case) -> float:
    started = time.perf_counter()
    sessions = [pool.submit(run_session, case.tasks_per_session) for _ in range(case.sessions)]
    for session in sessions:
        session.result()
    elapsed = time.perf_counter() - started
    if elapsed >= 600:
        raise RuntimeError(f"{case.sessions} x {case.tasks_per_session} exceeded 10 minutes")
    return elapsed


def format_row(case: Case, samples: list[float]) -> str:
    durations = sorted(samples)
    total_tasks = case.sessions * case.tasks_per_session
    throughputs = sorted(total_tasks / duration for duration in samples)
    middle = len(samples) // 2
    wall_time = "/".join(f"{value * 1000:.2f}" for value in (durations[0], durations[middle], durations[-1]))
    throughput = "/".join(f"{value:.2f}" for value in (throughputs[0], throughputs[middle], throughputs[-1]))
    return f"| {case.phase} | {case.sessions} | {case.tasks_per_session} | {len(samples)} | {wall_time} | {throughput} |"


def main() -> None:
    with ThreadPoolExecutor(max_workers=10) as pool:
        results = [
            (COLD_CASE, [run_sample(pool, COLD_CASE)]),
            (SCALE_OUT_CASE, [run_sample(pool, SCALE_OUT_CASE)]),
        ]
        run_sample(pool, WARMUP_CASE)
        for case in STEADY_CASES:
            results.append((case, [run_sample(pool, case) for _ in range(case.repetitions)]))

    print("## flamepy.core benchmark", flush=True)
    print("Worker: flmping (same as Rust); Python submits a two-byte JSON request and decodes each response.\n", flush=True)
    print("| Phase | Sessions | Tasks/session | Samples | Wall time ms (min/p50/max) | Tasks/sec (min/p50/max) |", flush=True)
    print("| --- | ---: | ---: | ---: | ---: | ---: |", flush=True)
    for case, samples in results:
        print(format_row(case, samples), flush=True)


if __name__ == "__main__":
    main()

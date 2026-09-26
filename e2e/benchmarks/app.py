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

"""End-to-end flamepy.app benchmark matching the Rust benchmark case matrix.

Run from the repository root with a configured Flame client and flmrun:
    python3 e2e/benchmarks/app.py

Each timed sample uses ``app.remote(echo)`` to create and close its
own service sessions, like the Rust benchmark. App registration happens before
timing. Results are informational; correctness errors fail the benchmark.
"""

import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass

import flamepy.app as app

APP_NAME = f"flamepy-app-benchmark-{uuid.uuid4().hex[:8]}"
app.init(APP_NAME)


@app.service(autoscale=True, warmup=0)
def echo(payload: bytes, index: int) -> tuple[bytes, int]:
    return payload, index


@dataclass(frozen=True)
class Case:
    phase: str
    sessions: int
    tasks_per_session: int
    repetitions: int


SMALL_PAYLOAD = b"flame-app-benchmark"
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
    service = app.remote(echo)
    try:
        futures = [service(SMALL_PAYLOAD, index) for index in range(tasks)]
        results = app.get(futures)
        expected = [(SMALL_PAYLOAD, index) for index in range(tasks)]
        if results != expected:
            raise RuntimeError(f"session returned incorrect echo results for {tasks} tasks")
    finally:
        service.close()


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
    throughputs = sorted(total_tasks / elapsed for elapsed in samples)
    median = len(samples) // 2
    wall_time = "/".join(f"{duration * 1000:.2f}" for duration in (durations[0], durations[median], durations[-1]))
    throughput = "/".join(f"{rate:.2f}" for rate in (throughputs[0], throughputs[median], throughputs[-1]))
    return f"| {case.phase} | {case.sessions} | {case.tasks_per_session} | {len(samples)} | {wall_time} | {throughput} |"


def main() -> None:
    try:
        with ThreadPoolExecutor(max_workers=10) as pool:
            results = [
                (COLD_CASE, [run_sample(pool, COLD_CASE)]),
                (SCALE_OUT_CASE, [run_sample(pool, SCALE_OUT_CASE)]),
            ]
            run_sample(pool, WARMUP_CASE)
            for case in STEADY_CASES:
                results.append((case, [run_sample(pool, case) for _ in range(case.repetitions)]))

        print("## flamepy.app benchmark", flush=True)
        print(f"App: `{APP_NAME}`; echo payload: {len(SMALL_PAYLOAD)} bytes; app registration excluded from timing.\n", flush=True)
        print("| Phase | Sessions | Tasks/session | Samples | Wall time ms (min/p50/max) | Tasks/sec (min/p50/max) |", flush=True)
        print("| --- | ---: | ---: | ---: | ---: | ---: |", flush=True)
        for case, samples in results:
            print(format_row(case, samples), flush=True)
    finally:
        app.destroy()


if __name__ == "__main__":
    main()

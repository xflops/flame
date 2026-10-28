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

"""Benchmark flamepy.agent (Sandbox and Session) with the case matrix of core.py and app.py.

Run from the benchmarks directory with a configured Flame client, on a cluster whose
executors run the host shim and can start flmbox sandboxes (runsc and the sandbox image):
    python3 agent.py

Both APIs run on the flmbox application. Each task is `echo ok` run in a sandbox with
`runsc exec`, far slower than the flmping or echo task of the other suites, so the
1000-task cases of core.py and app.py use FLAME_AGENT_BENCHMARK_HEAVY_TASKS tasks
(default 100). The other case sizes are the same. Each timed sample creates and closes
its sandboxes or sessions, so the first task of a sample includes binding an executor
and starting the sandbox (`runsc create`). Incorrect results fail the benchmark.

Environment:
    FLAME_AGENT_BENCHMARK_HEAVY_TASKS  tasks per session in the large cases (default: 100)
"""

import os
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass

from flamepy import agent

# The linux/amd64 manifest of ubuntu:26.04. The image is pinned by digest because every creation
# resolves a tag with the registry, the benchmark creates more than 200 sandboxes, and Docker Hub
# answers an unauthenticated address with "reached your unauthenticated pull rate limit" after 100
# pulls; a digest costs no registry request once the image is cached. Use the digest of your
# platform on another architecture.
IMAGE = "docker.io/library/ubuntu@sha256:157dc3d6e4a3e4f2c68d12df9df9ed6e808b59af6d4c3757b432b4f519916725"
HEAVY_TASKS = int(os.environ.get("FLAME_AGENT_BENCHMARK_HEAVY_TASKS", "100"))
COMMAND = "echo ok"
EXPECTED = b"ok\n"
# A task that does not finish (a sandbox that cannot start never fails its tasks) fails the
# benchmark here, instead of at the timeout of the job.
TASK_TIMEOUT = 300


@dataclass(frozen=True)
class Case:
    phase: str
    sessions: int
    tasks_per_session: int
    repetitions: int


COLD_CASE = Case("cold-start", 1, 1, 1)
SCALE_OUT_CASE = Case("scale-out", 10, 1, 1)
WARMUP_CASE = Case("warm-up", 4, HEAVY_TASKS, 1)
STEADY_CASES = (
    Case("steady-state", 1, 1, 5),
    Case("steady-state", 1, HEAVY_TASKS, 3),
    Case("steady-state", 10, 1, 5),
    Case("steady-state", 10, HEAVY_TASKS, 3),
)


def check(result: "agent.CommandResult") -> None:
    if not result.ok or result.stdout != EXPECTED:
        raise RuntimeError(f"unexpected command result: exit={result.exit_code} stdout={result.stdout!r} stderr={result.stderr!r}")


def run_sandbox(tasks: int) -> None:
    sandbox = agent.Sandbox.create(image=IMAGE)
    try:
        futures = [sandbox.submit(COMMAND) for _ in range(tasks)]
        for future in futures:
            check(future.result(timeout=TASK_TIMEOUT))
    finally:
        sandbox.close()


def run_session(tasks: int) -> None:
    session = agent.open_session(image=IMAGE)
    try:
        futures = [session.submit_code(COMMAND) for _ in range(tasks)]
        for future in futures:
            check(future.result(timeout=TASK_TIMEOUT))
    finally:
        session.close()


def run_sample(pool: ThreadPoolExecutor, run_one, case: Case) -> float:
    started = time.perf_counter()
    sessions = [pool.submit(run_one, case.tasks_per_session) for _ in range(case.sessions)]
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


def run_suite(run_one) -> list[tuple[Case, list[float]]]:
    with ThreadPoolExecutor(max_workers=10) as pool:
        results = [
            (COLD_CASE, [run_sample(pool, run_one, COLD_CASE)]),
            (SCALE_OUT_CASE, [run_sample(pool, run_one, SCALE_OUT_CASE)]),
        ]
        run_sample(pool, run_one, WARMUP_CASE)
        for case in STEADY_CASES:
            results.append((case, [run_sample(pool, run_one, case) for _ in range(case.repetitions)]))
    return results


def main() -> None:
    for name, run_one in (("Sandbox", run_sandbox), ("Session", run_session)):
        results = run_suite(run_one)
        print(f"## flamepy.agent {name} benchmark", flush=True)
        print(f"Worker: flmbox, image `{IMAGE}`; each task runs `{COMMAND}` in the sandbox; sandboxes are created and closed inside each sample; the large cases run {HEAVY_TASKS} tasks per session.\n", flush=True)
        print("| Phase | Sessions | Tasks/session | Samples | Wall time ms (min/p50/max) | Tasks/sec (min/p50/max) |", flush=True)
        print("| --- | ---: | ---: | ---: | ---: | ---: |", flush=True)
        for case, samples in results:
            print(format_row(case, samples), flush=True)
        print(flush=True)


if __name__ == "__main__":
    main()

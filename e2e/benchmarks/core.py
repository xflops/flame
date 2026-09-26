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

"""Python core client control for the Rust flmping benchmark.

This uses the same flmping application as the Rust benchmark, bypassing App,
runpy, and object-cache result storage. The two-byte JSON request represents
the default PingRequest; Rust sends no task input for the same behavior.
"""

import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from statistics import median

from flamepy import core

TASK_INPUT = b"{}"


@dataclass(frozen=True)
class Case:
    sessions: int
    tasks_per_session: int
    repetitions: int


@dataclass(frozen=True)
class SessionTiming:
    setup: float
    submit: float
    wait: float
    close: float


@dataclass(frozen=True)
class Sample:
    elapsed: float
    sessions: tuple[SessionTiming, ...]


def run_session(tasks: int) -> SessionTiming:
    started = time.perf_counter()
    session = core.create_session("flmping", session_id=f"python-core-benchmark-{uuid.uuid4().hex}")
    opened = time.perf_counter()
    try:
        futures = [session.submit(TASK_INPUT) for _ in range(tasks)]
        submitted = time.perf_counter()
        outputs = [future.result() for future in futures]
        if any(not isinstance(output, bytes) or not output for output in outputs):
            raise RuntimeError("flmping returned an empty or non-bytes result")
        completed = time.perf_counter()
    finally:
        session.close()
    closed = time.perf_counter()
    return SessionTiming(opened - started, submitted - opened, completed - submitted, closed - completed)


def run_sample(pool: ThreadPoolExecutor, case: Case) -> Sample:
    started = time.perf_counter()
    handles = [pool.submit(run_session, case.tasks_per_session) for _ in range(case.sessions)]
    sessions = tuple(handle.result() for handle in handles)
    elapsed = time.perf_counter() - started
    if elapsed >= 600:
        raise RuntimeError(f"{case.sessions} x {case.tasks_per_session} exceeded 10 minutes")
    return Sample(elapsed, sessions)


def format_row(case: Case, samples: list[Sample]) -> str:
    durations = sorted(sample.elapsed for sample in samples)
    total_tasks = case.sessions * case.tasks_per_session
    rates = sorted(total_tasks / duration for duration in durations)
    middle = len(samples) // 2
    wall_time = "/".join(f"{value * 1000:.2f}" for value in (durations[0], durations[middle], durations[-1]))
    throughput = "/".join(f"{value:.2f}" for value in (rates[0], rates[middle], rates[-1]))
    return f"| {case.sessions} | {case.tasks_per_session} | {len(samples)} | {wall_time} | {throughput} |"


def format_phase_row(case: Case, samples: list[Sample]) -> str:
    values = [median(median(getattr(session, phase) for session in sample.sessions) for sample in samples) * 1000 for phase in ("setup", "submit", "wait", "close")]
    return f"| {case.sessions} | {case.tasks_per_session} | " + " | ".join(f"{value:.2f}" for value in values) + " |"


def main() -> None:
    cases = (Case(1, 1000, 3), Case(10, 1000, 3))
    with ThreadPoolExecutor(max_workers=10) as pool:
        run_sample(pool, Case(4, 100, 1))
        results = [(case, [run_sample(pool, case) for _ in range(case.repetitions)]) for case in cases]

    print("## flamepy.core flmping control", flush=True)
    print("Same flmping worker as Rust; default PingRequest encoded as 2 JSON bytes.\n", flush=True)
    print("| Sessions | Tasks/session | Samples | Wall time ms (min/p50/max) | Tasks/sec (min/p50/max) |", flush=True)
    print("| ---: | ---: | ---: | ---: | ---: |", flush=True)
    for case, samples in results:
        print(format_row(case, samples), flush=True)
    print("\nPer-session phase time in ms (median across sessions and samples; concurrent phases overlap):", flush=True)
    print("| Sessions | Tasks/session | Setup | Submit | Wait | Close |", flush=True)
    print("| ---: | ---: | ---: | ---: | ---: | ---: |", flush=True)
    for case, samples in results:
        print(format_phase_row(case, samples), flush=True)


if __name__ == "__main__":
    main()

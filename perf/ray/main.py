"""
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
"""

import argparse
import time
from dataclasses import dataclass

import ray

NUM_SESSIONS = 3
TASKS_PER_SESSION = 1000
TOTAL_TASKS = NUM_SESSIONS * TASKS_PER_SESSION
TIMEOUT_SECS = 600


@dataclass
class BenchmarkResult:
    succeeded: int
    failed: int
    duration_secs: float

    @property
    def throughput(self) -> float:
        return self.succeeded / self.duration_secs if self.duration_secs > 0 else 0


@ray.remote
def benchmark_task(session_id: int, task_id: int, input: bytes) -> bytes:
    return b"ok" 


@ray.remote
def run_session(session_id: int, tasks_per_session: int) -> tuple[int, int]:
    task_refs = [
        benchmark_task.remote(session_id, task_id, b"benchmark")
        for task_id in range(tasks_per_session)
    ]

    succeeded = 0
    failed = 0

    results = ray.get(task_refs)
    for result in results:
        if result:
            succeeded += 1
        else:
            failed += 1

    return succeeded, failed


def run_benchmark(
    num_sessions: int = NUM_SESSIONS,
    tasks_per_session: int = TASKS_PER_SESSION,
) -> BenchmarkResult:
    total_tasks = num_sessions * tasks_per_session

    print("\n" + "=" * 60)
    print(
        f"BENCHMARK: {num_sessions} sessions × {tasks_per_session} tasks = {total_tasks} total"
    )
    print("=" * 60 + "\n")

    start = time.perf_counter()

    session_refs = [
        run_session.remote(session_id, tasks_per_session)
        for session_id in range(num_sessions)
    ]

    results = ray.get(session_refs)

    duration = time.perf_counter() - start

    total_succeeded = sum(r[0] for r in results)
    total_failed = sum(r[1] for r in results)

    return BenchmarkResult(
        succeeded=total_succeeded,
        failed=total_failed,
        duration_secs=duration,
    )


def print_results(result: BenchmarkResult, total_tasks: int) -> None:
    print("\n" + "=" * 60)
    print("BENCHMARK RESULTS")
    print("=" * 60)
    print(f"Duration:        {result.duration_secs:.2f}s")
    print(f"Succeeded:       {result.succeeded}/{total_tasks}")
    print(f"Failed:          {result.failed}")
    print(f"Throughput:      {result.throughput:.2f} tasks/sec")
    print("=" * 60 + "\n")


def main():
    parser = argparse.ArgumentParser(description="Ray cluster benchmark")
    parser.add_argument(
        "--address",
        default="auto",
        help="Ray cluster address (default: auto, use 'ray://<ip>:10001' for Ray Client)",
    )
    parser.add_argument(
        "--sessions",
        type=int,
        default=NUM_SESSIONS,
        help=f"Number of concurrent sessions (default: {NUM_SESSIONS})",
    )
    parser.add_argument(
        "--tasks",
        type=int,
        default=TASKS_PER_SESSION,
        help=f"Tasks per session (default: {TASKS_PER_SESSION})",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=TIMEOUT_SECS,
        help=f"Timeout in seconds (default: {TIMEOUT_SECS})",
    )
    args = parser.parse_args()

    total_tasks = args.sessions * args.tasks

    if args.address == "auto":
        ray.init()
    else:
        ray.init(args.address)

    try:
        result = run_benchmark(
            num_sessions=args.sessions,
            tasks_per_session=args.tasks,
        )

        print_results(result, total_tasks)

        assert result.failed == 0, f"Benchmark had {result.failed} failed tasks"
        assert result.succeeded == total_tasks, (
            f"Not all tasks succeeded: {result.succeeded}/{total_tasks}"
        )
        assert result.duration_secs < args.timeout, (
            f"Benchmark exceeded {args.timeout}s timeout: {result.duration_secs:.2f}s"
        )

        print("✓ Benchmark PASSED")

    finally:
        ray.shutdown()


if __name__ == "__main__":
    main()

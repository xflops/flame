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

TOTAL_TASKS = 900
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
def benchmark_task(task_id: int, input: bytes) -> bytes:
    return b"ok"


def run_benchmark(
    total_tasks: int = TOTAL_TASKS,
) -> BenchmarkResult:
    print("\n" + "=" * 60)
    print(f"BENCHMARK: {total_tasks} tasks")
    print("=" * 60 + "\n")

    start = time.perf_counter()

    task_refs = [
        benchmark_task.remote(task_id, b"benchmark")
        for task_id in range(total_tasks)
    ]

    results = ray.get(task_refs)

    duration = time.perf_counter() - start

    succeeded = sum(1 for r in results if r)
    failed = sum(1 for r in results if not r)

    return BenchmarkResult(
        succeeded=succeeded,
        failed=failed,
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
        "--tasks",
        type=int,
        default=TOTAL_TASKS,
        help=f"Total number of tasks (default: {TOTAL_TASKS})",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=TIMEOUT_SECS,
        help=f"Timeout in seconds (default: {TIMEOUT_SECS})",
    )
    args = parser.parse_args()

    if args.address == "auto":
        ray.init()
    else:
        ray.init(args.address)

    try:
        result = run_benchmark(total_tasks=args.tasks)

        print_results(result, args.tasks)

        assert result.failed == 0, f"Benchmark had {result.failed} failed tasks"
        assert result.succeeded == args.tasks, (
            f"Not all tasks succeeded: {result.succeeded}/{args.tasks}"
        )
        assert result.duration_secs < args.timeout, (
            f"Benchmark exceeded {args.timeout}s timeout: {result.duration_secs:.2f}s"
        )

        print("✓ Benchmark PASSED")

    finally:
        ray.shutdown()


if __name__ == "__main__":
    main()

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
import csv
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime
from typing import List, Optional

import matplotlib.pyplot as plt

from flamepy import (
    ApplicationAttributes,
    get_application,
    get_session,
    register_application,
    unregister_application,
)
from flamepy.core.types import TaskState
from flamepy.runner import Runner
from flamepy.runner.runner import RunnerService

NUM_SESSIONS = 6
TASKS_PER_SESSION = 1000
WARMUP_SECS = 30
TIMEOUT_SECS = 600

BENCHMARK_APP = "flamepy-bench"
RUNNER_TEMPLATE = "flmrun-perf"


@dataclass
class TaskLifecycle:
    """Tracks the lifecycle timestamps for a single task."""

    task_id: str
    session_id: str
    creation_time: Optional[datetime] = None
    start_time: Optional[datetime] = None
    completion_time: Optional[datetime] = None
    final_state: Optional[TaskState] = None
    session_creation_time: Optional[datetime] = None

    @property
    def queue_duration_ms(self) -> Optional[float]:
        """Time spent waiting in queue (creation -> start)."""
        if self.creation_time and self.start_time:
            return (self.start_time - self.creation_time).total_seconds() * 1000
        return None

    @property
    def execution_duration_ms(self) -> Optional[float]:
        """Time spent executing (start -> completion)."""
        if self.start_time and self.completion_time:
            return (self.completion_time - self.start_time).total_seconds() * 1000
        return None

    @property
    def total_duration_ms(self) -> Optional[float]:
        """Total time from creation to completion."""
        if self.creation_time and self.completion_time:
            return (self.completion_time - self.creation_time).total_seconds() * 1000
        return None


def benchmark_task(data: bytes) -> bytes:
    return b"ok"


@dataclass
class SessionResult:
    succeeded: int
    failed: int
    session_id: str = ""
    service: Optional[RunnerService] = None


@dataclass
class BenchmarkResult:
    succeeded: int
    failed: int
    duration_secs: float
    session_ids: List[str] = field(default_factory=list)
    services: List[RunnerService] = field(default_factory=list)

    @property
    def throughput(self) -> float:
        return self.succeeded / self.duration_secs if self.duration_secs > 0 else 0

    def close_services(self) -> None:
        for service in self.services:
            try:
                service.close()
            except Exception:
                pass


def setup_runner_template() -> bool:
    if get_application(RUNNER_TEMPLATE) is not None:
        return False

    register_application(
        RUNNER_TEMPLATE,
        ApplicationAttributes(
            command="/usr/bin/python3",
            arguments=["-m", "flamepy.runner.runpy"],
        ),
    )
    return True


def teardown_runner_template(registered: bool) -> None:
    if registered:
        unregister_application(RUNNER_TEMPLATE)


def create_service(runner: Runner) -> RunnerService:
    """Create a service without submitting tasks."""
    return runner.service(benchmark_task, warmup=2)


def run_tasks_on_service(
    runner: Runner, service: RunnerService, tasks_per_session: int
) -> SessionResult:
    """Submit tasks to an existing service and wait for completion."""
    session_id = service._session.id
    futures = [service(b"benchmark") for _ in range(tasks_per_session)]

    succeeded = 0
    failed = 0
    for future in runner.select(futures):
        try:
            future.get()
            succeeded += 1
        except Exception:
            failed += 1

    return SessionResult(
        succeeded=succeeded, failed=failed, session_id=session_id, service=service
    )


def run_benchmark(
    runner: Runner,
    num_sessions: int = NUM_SESSIONS,
    tasks_per_session: int = TASKS_PER_SESSION,
    warmup_secs: int = WARMUP_SECS,
) -> BenchmarkResult:
    total_tasks = num_sessions * tasks_per_session

    print("\n" + "=" * 60)
    print(
        f"BENCHMARK: {num_sessions} sessions × {tasks_per_session} tasks = {total_tasks} total"
    )
    print("=" * 60 + "\n")

    # Phase 1: Create all services (sessions) in parallel
    services: List[RunnerService] = []
    services_lock = threading.Lock()

    def create_service_worker():
        service = create_service(runner)
        with services_lock:
            services.append(service)

    create_threads = [
        threading.Thread(target=create_service_worker) for _ in range(num_sessions)
    ]
    for t in create_threads:
        t.start()
    for t in create_threads:
        t.join()

    print(f"Created {len(services)} sessions, waiting {warmup_secs}s for warmup...")
    time.sleep(warmup_secs)

    # Phase 2: Submit tasks and measure (excluding warmup time)
    results: List[SessionResult] = []
    results_lock = threading.Lock()

    def task_worker(service: RunnerService):
        result = run_tasks_on_service(runner, service, tasks_per_session)
        with results_lock:
            results.append(result)

    start = time.perf_counter()

    task_threads = [
        threading.Thread(target=task_worker, args=(svc,)) for svc in services
    ]
    for t in task_threads:
        t.start()

    print(f"Started {num_sessions} task threads, waiting for completion...")

    for t in task_threads:
        t.join()

    duration = time.perf_counter() - start

    succeeded = sum(r.succeeded for r in results)
    failed = sum(r.failed for r in results)
    session_ids = [r.session_id for r in results]
    result_services = [r.service for r in results if r.service is not None]

    return BenchmarkResult(
        succeeded=succeeded,
        failed=failed,
        duration_secs=duration,
        session_ids=session_ids,
        services=result_services,
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


def collect_task_lifecycles(
    session_ids: List[str], debug: bool = False
) -> List[TaskLifecycle]:
    """Collect task lifecycle data from all sessions by listing tasks and parsing events."""
    lifecycles = []
    events_found = {"pending": 0, "running": 0, "succeed": 0, "failed": 0, "other": 0}

    for session_id in session_ids:
        try:
            session = get_session(session_id)
            session_creation_time = session.creation_time

            for task in session.list_tasks():
                full_task = session.get_task(task.id)

                lifecycle = TaskLifecycle(
                    task_id=full_task.id,
                    session_id=session_id,
                    creation_time=full_task.creation_time,
                    completion_time=full_task.completion_time,
                    final_state=full_task.state,
                    session_creation_time=session_creation_time,
                )

                if full_task.events:
                    for event in full_task.events:
                        if event.code == 0:
                            events_found["pending"] += 1
                        elif event.code == 1:
                            events_found["running"] += 1
                            lifecycle.start_time = event.creation_time
                        elif event.code == 2:
                            events_found["succeed"] += 1
                        elif event.code == 3:
                            events_found["failed"] += 1
                        else:
                            events_found["other"] += 1

                lifecycles.append(lifecycle)
        except Exception as e:
            print(f"Warning: Failed to collect tasks from session {session_id}: {e}")

    if debug:
        print(f"Events found: {events_found}")
        tasks_with_start = sum(1 for lc in lifecycles if lc.start_time)
        print(f"Tasks with start_time: {tasks_with_start}/{len(lifecycles)}")

    return lifecycles


def print_lifecycle_stats(lifecycles: List[TaskLifecycle]) -> None:
    """Print statistics about task lifecycles."""
    if not lifecycles:
        print("No task lifecycle data collected.")
        return

    queue_times = [
        lc.queue_duration_ms for lc in lifecycles if lc.queue_duration_ms is not None
    ]
    exec_times = [
        lc.execution_duration_ms
        for lc in lifecycles
        if lc.execution_duration_ms is not None
    ]
    total_times = [
        lc.total_duration_ms for lc in lifecycles if lc.total_duration_ms is not None
    ]

    print("\n" + "=" * 60)
    print("TASK LIFECYCLE STATISTICS")
    print("=" * 60)
    print(f"Tasks analyzed:  {len(lifecycles)}")

    if queue_times:
        print(f"\nQueue Time (creation -> start):")
        print(f"  Min:    {min(queue_times):.2f} ms")
        print(f"  Max:    {max(queue_times):.2f} ms")
        print(f"  Avg:    {sum(queue_times) / len(queue_times):.2f} ms")

    if exec_times:
        print(f"\nExecution Time (start -> completion):")
        print(f"  Min:    {min(exec_times):.2f} ms")
        print(f"  Max:    {max(exec_times):.2f} ms")
        print(f"  Avg:    {sum(exec_times) / len(exec_times):.2f} ms")

    if total_times:
        print(f"\nTotal Time (creation -> completion):")
        print(f"  Min:    {min(total_times):.2f} ms")
        print(f"  Max:    {max(total_times):.2f} ms")
        print(f"  Avg:    {sum(total_times) / len(total_times):.2f} ms")

    print("=" * 60 + "\n")


def export_lifecycle_csv(lifecycles: List[TaskLifecycle], output_file: str) -> None:
    """Export task lifecycle data to CSV file."""
    if not lifecycles:
        print("No task lifecycle data to export.")
        return

    with open(output_file, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(
            [
                "session_id",
                "task_id",
                "session_creation_time",
                "task_creation_time",
                "task_running_time",
                "task_completion_time",
                "final_state",
                "queue_duration_ms",
                "execution_duration_ms",
                "total_duration_ms",
            ]
        )

        for lc in lifecycles:
            writer.writerow(
                [
                    lc.session_id,
                    lc.task_id,
                    lc.session_creation_time.isoformat()
                    if lc.session_creation_time
                    else "",
                    lc.creation_time.isoformat() if lc.creation_time else "",
                    lc.start_time.isoformat() if lc.start_time else "",
                    lc.completion_time.isoformat() if lc.completion_time else "",
                    lc.final_state.name if lc.final_state else "",
                    f"{lc.queue_duration_ms:.3f}" if lc.queue_duration_ms else "",
                    f"{lc.execution_duration_ms:.3f}"
                    if lc.execution_duration_ms
                    else "",
                    f"{lc.total_duration_ms:.3f}" if lc.total_duration_ms else "",
                ]
            )

    print(f"Lifecycle data exported to: {output_file}")


def draw_timeline_diagram(
    lifecycles: List[TaskLifecycle], output_file: str, max_tasks: int = 100
) -> None:
    """Draw a Gantt-style timeline diagram showing task execution phases."""
    if not lifecycles:
        print("No task lifecycle data to visualize.")
        return

    valid_lifecycles = [
        lc
        for lc in lifecycles
        if lc.creation_time and lc.completion_time and lc.session_creation_time
    ]

    if not valid_lifecycles:
        print("No tasks with complete lifecycle data to visualize.")
        return

    tasks_with_start = [lc for lc in valid_lifecycles if lc.start_time]
    print(f"Tasks with running event: {len(tasks_with_start)}/{len(valid_lifecycles)}")

    session_ids = sorted(set(lc.session_id for lc in valid_lifecycles))
    print(f"Generating diagrams for {len(session_ids)} sessions...")

    base_name = output_file.rsplit(".", 1)[0] if "." in output_file else output_file
    ext = output_file.rsplit(".", 1)[1] if "." in output_file else "png"

    for session_id in session_ids:
        session_lifecycles = [
            lc for lc in valid_lifecycles if lc.session_id == session_id
        ]
        if not session_lifecycles:
            continue

        session_file = f"{base_name}_{session_id}.{ext}"
        _draw_session_timeline(session_lifecycles, session_file, max_tasks, session_id)


def _draw_session_timeline(
    lifecycles: List[TaskLifecycle], output_file: str, max_tasks: int, session_id: str
) -> None:
    """Draw timeline for a single session with time relative to session creation."""

    def get_creation_time(lc: TaskLifecycle) -> datetime:
        assert lc.creation_time is not None
        return lc.creation_time

    if len(lifecycles) > max_tasks:
        print(
            f"  [{session_id}] Limiting to {max_tasks} tasks (out of {len(lifecycles)})"
        )
        lifecycles = sorted(lifecycles, key=get_creation_time)[:max_tasks]

    lifecycles = sorted(lifecycles, key=get_creation_time)

    assert lifecycles[0].session_creation_time is not None
    session_start = lifecycles[0].session_creation_time

    fig, ax = plt.subplots(figsize=(16, max(8, len(lifecycles) * 0.25)))

    for idx, lc in enumerate(lifecycles):
        y_pos = idx
        assert lc.creation_time is not None
        assert lc.completion_time is not None

        creation_offset = (lc.creation_time - session_start).total_seconds()
        completion_offset = (lc.completion_time - session_start).total_seconds()

        ax.plot(creation_offset, y_pos, "bo", markersize=8, zorder=5)

        if lc.start_time:
            start_offset = (lc.start_time - session_start).total_seconds()
            ax.plot(start_offset, y_pos, "g^", markersize=10, zorder=5)

            ax.hlines(
                y=y_pos,
                xmin=creation_offset,
                xmax=start_offset,
                colors="#FFB347",
                linewidth=3,
                label="Queue" if idx == 0 else "",
            )

            exec_color = "#FF6B6B" if lc.final_state == TaskState.FAILED else "#87CEEB"
            ax.hlines(
                y=y_pos,
                xmin=start_offset,
                xmax=completion_offset,
                colors=exec_color,
                linewidth=3,
            )
        else:
            exec_color = "#FF6B6B" if lc.final_state == TaskState.FAILED else "#87CEEB"
            ax.hlines(
                y=y_pos,
                xmin=creation_offset,
                xmax=completion_offset,
                colors=exec_color,
                linewidth=3,
            )

        ax.plot(completion_offset, y_pos, "rv", markersize=10, zorder=5)

    ax.axvline(x=0, color="black", linestyle="--", linewidth=1, label="Session Start")

    ax.set_xlabel("Time (seconds from session creation)")
    ax.set_ylabel("Task Index")
    ax.set_title(
        f"Task Execution Timeline - Session: {session_id}\n"
        f"(● Created, ▲ Running, ▼ Completed)"
    )

    legend_elements = [
        plt.Line2D(
            [0],
            [0],
            color="black",
            linestyle="--",
            linewidth=1,
            label="Session Creation (t=0)",
        ),
        plt.Line2D(
            [0],
            [0],
            marker="o",
            color="w",
            markerfacecolor="blue",
            markersize=10,
            label="Task Created",
        ),
        plt.Line2D(
            [0],
            [0],
            marker="^",
            color="w",
            markerfacecolor="green",
            markersize=10,
            label="Task Running",
        ),
        plt.Line2D(
            [0],
            [0],
            marker="v",
            color="w",
            markerfacecolor="red",
            markersize=10,
            label="Task Completed",
        ),
        plt.Line2D([0], [0], color="#FFB347", linewidth=3, label="Queue Time"),
        plt.Line2D([0], [0], color="#87CEEB", linewidth=3, label="Execution Time"),
    ]
    ax.legend(handles=legend_elements, loc="upper right")

    ax.set_ylim(-0.5, len(lifecycles) - 0.5)
    ax.invert_yaxis()

    plt.tight_layout()
    plt.savefig(output_file, dpi=150, bbox_inches="tight")
    plt.close()

    print(f"  Timeline saved: {output_file}")


def main():
    parser = argparse.ArgumentParser(
        description="Flame cluster benchmark using runner.service API"
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
    parser.add_argument(
        "--warmup",
        type=int,
        default=WARMUP_SECS,
        help=f"Warmup time in seconds before submitting tasks (default: {WARMUP_SECS})",
    )
    parser.add_argument(
        "--timeline",
        type=str,
        default=None,
        metavar="FILE",
        help="Output file for timeline diagram (e.g., timeline.png)",
    )
    parser.add_argument(
        "--max-timeline-tasks",
        type=int,
        default=100,
        help="Maximum tasks to show in timeline diagram (default: 100)",
    )
    parser.add_argument(
        "--lifecycle-stats",
        action="store_true",
        help="Print detailed task lifecycle statistics",
    )
    parser.add_argument(
        "--csv",
        type=str,
        default=None,
        metavar="FILE",
        help="Export lifecycle data to CSV file (e.g., lifecycle.csv)",
    )
    args = parser.parse_args()

    total_tasks = args.sessions * args.tasks
    collect_lifecycle = args.timeline or args.lifecycle_stats or args.csv
    lifecycles: List[TaskLifecycle] = []

    registered = setup_runner_template()
    try:
        with Runner(BENCHMARK_APP) as runner:
            result = run_benchmark(
                runner=runner,
                num_sessions=args.sessions,
                tasks_per_session=args.tasks,
                warmup_secs=args.warmup,
            )

            print_results(result, total_tasks)

            if collect_lifecycle and result.session_ids:
                print("Collecting task lifecycle data...")
                lifecycles = collect_task_lifecycles(result.session_ids)

            result.close_services()
    finally:
        teardown_runner_template(registered)

    if lifecycles:
        if args.lifecycle_stats:
            print_lifecycle_stats(lifecycles)

        if args.timeline:
            draw_timeline_diagram(lifecycles, args.timeline, args.max_timeline_tasks)

        if args.csv:
            export_lifecycle_csv(lifecycles, args.csv)

    assert result.failed == 0, f"Benchmark had {result.failed} failed tasks"
    assert result.succeeded == total_tasks, (
        f"Not all tasks succeeded: {result.succeeded}/{total_tasks}"
    )
    assert result.duration_secs < args.timeout, (
        f"Benchmark exceeded {args.timeout}s timeout: {result.duration_secs:.2f}s"
    )

    print("✓ Benchmark PASSED")


if __name__ == "__main__":
    main()

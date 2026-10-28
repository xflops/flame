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

"""Benchmark flamepy.agent: Sandbox and Session, both backed by the flmbox application.

Run from the benchmarks directory with a configured Flame client, on a cluster whose
executor managers can run flmbox sandboxes (runsc and the sandbox image):
    python3 agent.py

Environment:
    FLAME_AGENT_BENCHMARK_IMAGE     sandbox image (default: the flmbox default image)
    FLAME_AGENT_BENCHMARK_SAMPLES   samples per case (default: 5)
    FLAME_AGENT_BENCHMARK_PARALLEL  sandboxes or sessions in the scale-out case (default: 4)
    FLAME_AGENT_BENCHMARK_SUSPEND   set to 0 to skip the Sandbox suspend/resume case

Every sample creates and closes its own sandbox or session, so the first command of a
sample pays for binding an executor and for starting the sandbox (`runsc create`, which
is what the sandbox startup work changes). Commands are shell `echo`, so the image needs
no Python. An incorrect result fails the benchmark.
"""

import os
import time
from concurrent.futures import ThreadPoolExecutor

from flamepy import agent

IMAGE = os.environ.get("FLAME_AGENT_BENCHMARK_IMAGE") or None
SAMPLES = int(os.environ.get("FLAME_AGENT_BENCHMARK_SAMPLES", "5"))
PARALLEL = int(os.environ.get("FLAME_AGENT_BENCHMARK_PARALLEL", "4"))
SUSPEND = os.environ.get("FLAME_AGENT_BENCHMARK_SUSPEND", "1") != "0"
STEADY_COMMANDS = 20
EXPECTED = b"ok\n"
COMMAND = "echo ok"


def check(result: "agent.CommandResult") -> None:
    if not result.ok or result.stdout != EXPECTED:
        raise RuntimeError(f"unexpected command result: exit={result.exit_code} stdout={result.stdout!r} stderr={result.stderr!r}")


def timed(fn, *args) -> float:
    """Seconds taken by fn(*args)."""
    started = time.perf_counter()
    fn(*args)
    return time.perf_counter() - started


def timed_run(run, command: str) -> float:
    """Seconds taken by run(command), whose result is checked."""
    started = time.perf_counter()
    result = run(command)
    elapsed = time.perf_counter() - started
    check(result)
    return elapsed


# ----------------------------------------------------------------------------- sandbox


def sandbox_create_first_second() -> list[list[float]]:
    columns: list[list[float]] = [[], [], []]
    for _ in range(SAMPLES):
        started = time.perf_counter()
        box = agent.Sandbox.create(image=IMAGE)
        created = time.perf_counter() - started
        try:
            first = timed_run(box.run, COMMAND)
            second = timed_run(box.run, COMMAND)
        finally:
            box.close()
        for column, value in zip(columns, (created, first, second)):
            column.append(value)
    return columns


def sandbox_steady() -> list[float]:
    """One sandbox, commands back to back after a warm-up command."""
    samples: list[float] = []
    box = agent.Sandbox.create(image=IMAGE)
    try:
        check(box.run(COMMAND))
        for _ in range(STEADY_COMMANDS):
            samples.append(timed_run(box.run, COMMAND))
    finally:
        box.close()
    return samples


def sandbox_suspend_resume() -> list[list[float]]:
    columns: list[list[float]] = [[], []]
    for _ in range(SAMPLES):
        box = agent.Sandbox.create(image=IMAGE)
        try:
            check(box.run(COMMAND))
            columns[0].append(timed(box.suspend))
            columns[1].append(timed(box.resume))
            # The resumed sandbox starts fresh (no state is preserved yet), but must work.
            check(box.run(COMMAND))
        finally:
            box.close()
    return columns


def sandbox_one_scale_out() -> float:
    def one() -> None:
        box = agent.Sandbox.create(image=IMAGE)
        try:
            check(box.run(COMMAND))
        finally:
            box.close()

    with ThreadPoolExecutor(max_workers=PARALLEL) as pool:
        started = time.perf_counter()
        for future in [pool.submit(one) for _ in range(PARALLEL)]:
            future.result()
        return time.perf_counter() - started


# ----------------------------------------------------------------------------- session


def session_open_first_second() -> list[list[float]]:
    columns: list[list[float]] = [[], [], []]
    for _ in range(SAMPLES):
        started = time.perf_counter()
        session = agent.open_session(image=IMAGE)
        opened = time.perf_counter() - started
        try:
            first = timed_run(session.run_code, COMMAND)
            second = timed_run(session.run_code, COMMAND)
        finally:
            session.close()
        for column, value in zip(columns, (opened, first, second)):
            column.append(value)
    return columns


def session_steady() -> list[float]:
    samples: list[float] = []
    session = agent.open_session(image=IMAGE)
    try:
        check(session.run_code(COMMAND))
        for _ in range(STEADY_COMMANDS):
            samples.append(timed_run(session.run_code, COMMAND))
    finally:
        session.close()
    return samples


def session_one_scale_out() -> float:
    def one() -> None:
        session = agent.open_session(image=IMAGE)
        try:
            check(session.run_code(COMMAND))
        finally:
            session.close()

    with ThreadPoolExecutor(max_workers=PARALLEL) as pool:
        started = time.perf_counter()
        for future in [pool.submit(one) for _ in range(PARALLEL)]:
            future.result()
        return time.perf_counter() - started


# ----------------------------------------------------------------------------- report


def row(name: str, samples: list[float], note: str = "") -> str:
    ordered = sorted(samples)
    middle = len(ordered) // 2
    values = "/".join(f"{value * 1000:.2f}" for value in (ordered[0], ordered[middle], ordered[-1]))
    return f"| {name} | {len(samples)} | {values} | {note} |"


def main() -> None:
    rows: list[str] = []

    created, first, second = sandbox_create_first_second()
    rows.append(row("Sandbox.create", created, "creates the session; the sandbox starts on the first command"))
    rows.append(row("Sandbox first command", first, "executor bind and sandbox start"))
    rows.append(row("Sandbox second command", second, "warm"))
    rows.append(row("Sandbox steady command", sandbox_steady(), f"{STEADY_COMMANDS} commands on one sandbox"))
    if SUSPEND:
        suspend, resume = sandbox_suspend_resume()
        rows.append(row("Sandbox.suspend", suspend, "session suspend; the sandbox is not preserved"))
        rows.append(row("Sandbox.resume", resume, "back to open"))
    rows.append(row(f"Sandbox scale-out x{PARALLEL}", [sandbox_one_scale_out() for _ in range(SAMPLES)], "create, one command and close, in parallel"))

    opened, first, second = session_open_first_second()
    rows.append(row("agent.open_session", opened, "creates the session"))
    rows.append(row("Session first command", first, "executor bind and sandbox start"))
    rows.append(row("Session second command", second, "warm"))
    rows.append(row("Session steady command", session_steady(), f"{STEADY_COMMANDS} commands on one session"))
    rows.append(row(f"Session scale-out x{PARALLEL}", [session_one_scale_out() for _ in range(SAMPLES)], "open, one command and close, in parallel"))

    print("## flamepy.agent benchmark", flush=True)
    print(f"Image: `{IMAGE or 'flmbox default'}`; command: `{COMMAND}`; each sample creates and closes its own sandbox or session.\n", flush=True)
    print("| Case | Samples | Wall time ms (min/p50/max) | Note |", flush=True)
    print("| --- | ---: | ---: | --- |", flush=True)
    for line in rows:
        print(line, flush=True)


if __name__ == "__main__":
    main()

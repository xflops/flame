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

import os
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor

import flamepy
import pytest
from flamepy import SessionState, TaskState
from flamepy.proto.types_pb2 import ExecutorState, NodeState

from e2e.api import TestRequest
from e2e.helpers import deserialize_response, serialize_request
from tests.utils import deploy_e2e_application, random_string


def policy(name):
    """Gate a case on the scheduling policy the cluster runs (FLAME_E2E_POLICY, default priority)."""
    return pytest.mark.skipif(
        os.environ.get("FLAME_E2E_POLICY", "priority") != name,
        reason=f"requires a {name} cluster (FLAME_E2E_POLICY={name})",
    )


POLL_SECONDS = 0.2
STABLE_SECONDS = 1.5  # Three 500ms scheduler intervals (ci/docker/flame-cluster-drf.yaml).
MEMORY_BYTES = 64 * 1024 * 1024
NODE_EXECUTOR_LIMIT = 10  # ci/docker/flame-cluster-drf.yaml
BOUND_STATES = {ExecutorState.ExecutorBinding, ExecutorState.ExecutorBound}


def _wait_until(predicate, description, timeout=60):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        matched, last = predicate()
        if matched:
            return
        time.sleep(POLL_SECONDS)
    raise AssertionError(f"Timed out waiting for {description}; last observation: {last}")


def _capacity():
    """Choose CPU-dominant requests with exactly four or six allocatable bundles.

    Dividing every node's CPU capacity avoids fractional per-node leftovers
    changing the expected fair split. Memory and node instance limits must not
    reduce that count. Explicitly enabled tests fail on unsupported clusters.
    """
    nodes = [node for node in flamepy.list_nodes() if node.status.state == NodeState.Ready]
    assert nodes, "DRF E2E needs Ready executor-manager nodes"
    cpus = [node.status.allocatable.cpu for node in nodes]
    total_memory = sum(node.status.allocatable.memory for node in nodes)
    for cpu in range(1, min(cpus) + 1):
        if any(capacity % cpu for capacity in cpus):
            continue
        counts = [capacity // cpu for capacity in cpus]
        capacity = sum(counts)
        if capacity not in (4, 6):
            continue
        if any(count > NODE_EXECUTOR_LIMIT or count * MEMORY_BYTES > node.status.allocatable.memory for count, node in zip(counts, nodes)):
            continue
        if MEMORY_BYTES / total_memory > cpu / sum(cpus):
            continue
        return flamepy.ResourceRequirement(cpu=cpu, memory=MEMORY_BYTES, gpu=0), capacity, nodes
    raise AssertionError(f"DRF E2E needs CPU capacities divisible into four or six slots; Ready node CPUs: {cpus}")


class DrfCase:
    def __init__(self, application, resreq, capacity, nodes):
        self.application = application
        self.resreq = resreq
        self.capacity = capacity
        self.nodes = {node.metadata.name: node.status.allocatable for node in nodes}
        self.sessions = []
        self.tasks = []

    def session(self, **options):
        session = flamepy.create_session(application=self.application, session_id=f"drf-{random_string(10)}", resreq=self.resreq, **options)
        self.sessions.append(session)
        return session

    def submit(self, session, count, sleep_ms=750):
        tasks = []
        for index in range(count):
            expected = f"{session.id}-{index}"
            task = session.create_task(serialize_request(TestRequest(input=expected, sleep_ms=sleep_ms)))
            tasks.append((session, task.id, expected))
        self.tasks.extend(tasks)

    def submit_backlogs(self, *sessions):
        # Queue both sides together; any first-wave admission skew can drain
        # before asserting a sustained fair split with a remaining backlog.
        with ThreadPoolExecutor(max_workers=len(sessions)) as pool:
            futures = [pool.submit(self.submit, session, self.capacity * 8) for session in sessions]
            for future in futures:
                future.result(timeout=30)

    def distribution(self, targets, working=(), running_only=()):
        stable_since = None
        history = []

        def observe():
            nonlocal stable_since
            executors = [executor for executor in flamepy.list_executors() if executor.spec.application == self.application and executor.status.state != ExecutorState.ExecutorReleased]
            assert len(executors) <= self.capacity, f"Physical executor reservations exceed {self.capacity}: {executors}"
            per_node = Counter(executor.spec.node for executor in executors)
            for node, count in per_node.items():
                assert count <= NODE_EXECUTOR_LIMIT
                assert count * self.resreq.cpu <= self.nodes[node].cpu
                assert count * self.resreq.memory <= self.nodes[node].memory
            counts = Counter(executor.status.session_id for executor in executors if executor.status.state in BOUND_STATES)
            live = {session.id: flamepy.get_session(session.id) for session, _ in targets}
            assert all(session.failed == 0 for session in live.values()), f"Tasks failed: {live}"
            matched = all(live[session.id].state == SessionState.OPEN and counts[session.id] == count for session, count in targets)
            matched = matched and all(live[session.id].pending > 0 and live[session.id].running > 0 for session in working)
            matched = matched and all(live[session.id].pending == 0 and live[session.id].running > 0 for session in running_only)
            if not matched:
                stable_since = None
            elif stable_since is None:
                stable_since = time.monotonic()
            state = {key: (session.state.name, session.pending, session.running, session.succeed, counts[key]) for key, session in live.items()}
            nodes = sorted(Counter((executor.spec.node, ExecutorState.Name(executor.status.state)) for executor in executors).items())
            if not history or history[-1][1:] != (state, nodes):
                history.append((round(time.monotonic() - started, 1), state, nodes))
            return matched and time.monotonic() - stable_since >= STABLE_SECONDS, state

        started = time.monotonic()
        try:
            _wait_until(observe, f"stable DRF bindings {[(session.id, count) for session, count in targets]}")
        except AssertionError as error:
            timeline = "\n".join(str(entry) for entry in history[-25:])
            raise AssertionError(f"{error}\nlast observations (seconds, session state, executors by node/state):\n{timeline}") from None

    def finish(self, timeout=90):
        def drained():
            state = [(session.id, flamepy.get_session(session.id)) for session in self.sessions]
            return all(item.pending == 0 and item.running == 0 for _, item in state), [(key, item.pending, item.running) for key, item in state]

        _wait_until(drained, "all submitted tasks to finish", timeout)
        for session, task_id, expected in self.tasks:
            task = session.get_task(task_id)
            assert task.state == TaskState.SUCCEED, f"Task {task_id} ended as {task.state}; events: {task.events}"
            assert deserialize_response(task.output).output == expected

    def cleanup(self):
        errors = []
        try:
            self.finish()
        except Exception as error:
            errors.append(f"task completion: {error}")
        for session in self.sessions:
            try:
                flamepy.close_session(session.id)
            except Exception as error:
                errors.append(f"close {session.id}: {error}")
        try:
            if flamepy.get_application(self.application) is not None:
                flamepy.unregister_application(self.application)
            _wait_until(
                lambda: (not any(executor.spec.application == self.application for executor in flamepy.list_executors()), self.application),
                "owned executors to release",
            )
            _wait_until(lambda: (flamepy.get_application(self.application) is None, self.application), "owned application to delete")
        except Exception as error:
            errors.append(f"application cleanup: {error}")
        assert not errors, "; ".join(errors)


@pytest.fixture
def drf_case():
    def idle_cluster():
        live = [executor for executor in flamepy.list_executors() if executor.status.state != ExecutorState.ExecutorReleased]
        return not live, [(executor.spec.application, ExecutorState.Name(executor.status.state)) for executor in live]

    _wait_until(idle_cluster, "a cluster without executors", timeout=180)
    resreq, capacity, nodes = _capacity()
    case = DrfCase(f"flme2e-drf-{random_string(8)}", resreq, capacity, nodes)
    try:
        deploy_e2e_application(case.application)
        yield case
    finally:
        case.cleanup()


@policy("drf")
def test_zero_minimum_idle_session_does_not_reduce_busy_capacity(drf_case):
    idle = drf_case.session()
    busy = drf_case.session()
    drf_case.submit(busy, drf_case.capacity * 8)
    drf_case.distribution([(idle, 0), (busy, drf_case.capacity)], working=[busy])
    assert flamepy.get_session(idle.id).state == SessionState.OPEN


@policy("drf")
def test_two_backlogged_sessions_receive_equal_dominant_shares(drf_case):
    first, second = drf_case.session(), drf_case.session()
    drf_case.submit_backlogs(first, second)
    half = drf_case.capacity // 2
    drf_case.distribution([(first, half), (second, half)], working=[first, second])


@policy("drf")
def test_small_session_maximum_redistributes_unused_share(drf_case):
    small, busy = drf_case.session(max_instances=1), drf_case.session()
    drf_case.submit_backlogs(small, busy)
    drf_case.distribution([(small, 1), (busy, drf_case.capacity - 1)], working=[small, busy])


@policy("drf")
def test_explicit_idle_minimum_is_preserved(drf_case):
    minimum = drf_case.capacity - 1
    warm = drf_case.session(min_instances=minimum)
    drf_case.distribution([(warm, minimum)])
    busy = drf_case.session()
    drf_case.submit(busy, drf_case.capacity * 2)
    drf_case.distribution([(warm, minimum), (busy, 1)], working=[busy])


@policy("drf")
def test_running_demand_then_open_idle_session_hands_capacity_to_backlog(drf_case):
    first = drf_case.session()
    half = drf_case.capacity // 2
    drf_case.submit(first, half, sleep_ms=10000)
    drf_case.distribution([(first, half)], running_only=[first])
    busy = drf_case.session()
    drf_case.submit(busy, drf_case.capacity * 16)
    drf_case.distribution([(first, half), (busy, half)], working=[busy], running_only=[first])
    drf_case.distribution([(first, 0), (busy, drf_case.capacity)], working=[busy])
    drained = flamepy.get_session(first.id)
    assert drained.state == SessionState.OPEN and drained.pending == 0 and drained.running == 0


@policy("drf")
def test_late_session_rebalances_to_an_equal_share(drf_case):
    greedy = drf_case.session()
    drf_case.submit(greedy, drf_case.capacity * 16)
    drf_case.distribution([(greedy, drf_case.capacity)], working=[greedy])
    late = drf_case.session()
    drf_case.submit(late, drf_case.capacity * 16)
    half = drf_case.capacity // 2
    drf_case.distribution([(greedy, half), (late, half)], working=[greedy, late])

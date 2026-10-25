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

The per-executor cgroup of the host shim (#604): the service of an executor runs in
/sys/fs/cgroup/flame/<executor-id> with the limits of the executor's resource
requirement, and the group is removed when the executor is released.

These tests need the executor manager on this machine, as root, with cgroup v2.
They run only when FLAME_E2E_CGROUP=1 (the host-shim CI job sets it).
"""

import os
import time
from pathlib import Path

import flamepy
import pytest

from e2e.api import TestRequest
from e2e.helpers import invoke_task
from tests.utils import deploy_e2e_application, random_string

FLM_CGROUP_APP = f"flme2e-cgroup-{random_string(6)}"
GROUPS = Path("/sys/fs/cgroup/flame")
TIMEOUT = 120.0
# An idle executor is released after the application's delay_release (about two
# minutes by default), and its group is removed then.
RELEASE_TIMEOUT = 300.0

pytestmark = pytest.mark.skipif(
    os.environ.get("FLAME_E2E_CGROUP", "").strip().lower() not in ("1", "true", "yes"),
    reason="set FLAME_E2E_CGROUP=1 on a node with cgroup v2 and the executor manager",
)


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    deploy_e2e_application(FLM_CGROUP_APP)
    yield
    for sess in flamepy.list_sessions():
        if sess.application == FLM_CGROUP_APP:
            try:
                flamepy.close_session(sess.id)
            except Exception:
                pass
    flamepy.unregister_application(FLM_CGROUP_APP)


def _groups():
    return {p.name for p in GROUPS.iterdir() if p.is_dir()} if GROUPS.is_dir() else set()


def _wait(predicate, what, timeout=TIMEOUT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.5)
    pytest.fail(f"timed out waiting for {what}")


def test_executor_runs_in_a_limited_group_and_the_group_goes_with_it():
    before = _groups()
    session = flamepy.create_session(application=FLM_CGROUP_APP, session_id=f"cgroup-{random_string(8)}")
    try:
        assert invoke_task(session, TestRequest(input="hello")).output == "hello"

        created = _wait(lambda: _groups() - before, "the executor's cgroup")
        group = GROUPS / sorted(created)[0]

        # The limits come from the executor's resource requirement, swap is off.
        memory = (group / "memory.max").read_text().strip()
        assert memory == "max" or int(memory) > 0
        assert (group / "memory.swap.max").read_text().strip() == "0"
        assert (group / "cpu.max").read_text().split()[1] == "100000"
        # The service runs in the group.
        assert (group / "cgroup.procs").read_text().split(), "the group has no process"
    finally:
        flamepy.close_session(session.id)

    # Releasing the executor kills and removes its group.
    _wait(lambda: not group.exists(), f"the removal of {group}", RELEASE_TIMEOUT)

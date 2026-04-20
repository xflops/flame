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

from concurrent.futures import wait

import flamepy
import pytest

from e2e.api import TestRequest
from e2e.helpers import invoke_task, serialize_request

FLM_TEST_SVC_APP = "flme2e-svc"
FLM_TEST_SVC_APP_URL = "file:///opt/e2e"


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    flamepy.register_application(
        FLM_TEST_SVC_APP,
        flamepy.ApplicationAttributes(
            command="${FLAME_HOME}/bin/uv",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG"},
            arguments=["run", "src/e2e/basic_svc.py", "src/e2e/api.py"],
            url=FLM_TEST_SVC_APP_URL,
        ),
    )

    yield

    sessions = flamepy.list_sessions()
    for sess in sessions:
        try:
            flamepy.close_session(sess.id)
        except:
            pass

    flamepy.unregister_application(FLM_TEST_SVC_APP)


def test_batch_session_basic():
    session = flamepy.create_session(
        application=FLM_TEST_SVC_APP,
        batch_size=2,
        min_instances=2,
    )

    task_num = 4
    for i in range(task_num):
        request = TestRequest(input=f"batch_task_{i}")
        response = invoke_task(session, request)
        assert response.output == f"batch_task_{i}"

    session.close()


def test_batch_session_parallel_tasks():
    session = flamepy.create_session(
        application=FLM_TEST_SVC_APP,
        batch_size=2,
        min_instances=2,
    )

    task_num = 4
    futures = []
    for i in range(task_num):
        request = TestRequest(input=f"parallel_batch_task_{i}")
        future = session.run(serialize_request(request))
        futures.append(future)

    wait(futures)
    results = [f.result() for f in futures]

    assert len(results) == task_num

    session.close()


def test_batch_session_no_partial_start():
    """Test that tasks don't start partially with batch_size > 1.

    With batch_size=2 and min_instances=0, a single task should remain
    Pending until a second task is created to complete the batch.
    """
    import time

    from flamepy.core.types import TaskState

    session = flamepy.create_session(
        application=FLM_TEST_SVC_APP,
        batch_size=2,
        min_instances=0,
    )

    request1 = TestRequest(input="partial_start_test_1")
    task1 = session.create_task(serialize_request(request1))
    task1_id = task1.id

    time.sleep(3)

    task1_status = session.get_task(task1_id)
    assert task1_status.state == TaskState.PENDING, f"Task should remain Pending with batch_size=2 and only 1 task. Got: {task1_status.state}"

    request2 = TestRequest(input="partial_start_test_2")
    task2 = session.create_task(serialize_request(request2))
    task2_id = task2.id

    timeout = 120
    poll_interval = 0.5
    deadline = time.time() + timeout

    while time.time() < deadline:
        t1 = session.get_task(task1_id)
        t2 = session.get_task(task2_id)

        t1_done = t1.state in (TaskState.SUCCEED, TaskState.FAILED)
        t2_done = t2.state in (TaskState.SUCCEED, TaskState.FAILED)

        if t1_done and t2_done:
            assert t1.state == TaskState.SUCCEED, f"Task 1 should succeed, got {t1.state}"
            assert t2.state == TaskState.SUCCEED, f"Task 2 should succeed, got {t2.state}"
            break

        time.sleep(poll_interval)
    else:
        t1 = session.get_task(task1_id)
        t2 = session.get_task(task2_id)
        pytest.fail(f"Timeout waiting for tasks to complete. Task1: {t1.state}, Task2: {t2.state}")

    session.close()

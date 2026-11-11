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

import base64
import time

import flamepy
import pytest
from flamepy import SessionState
from flamepy.core import ObjectRef, get_object, put_object
from flamepy.core.cache import ObjectKey
from flamepy.serving import open_session

from e2e.api import TestContext, TestRequest
from tests.utils import deploy_e2e_application, random_string

FLM_TEST_APP = "flme2e"


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    runtime = flamepy.get_application("flmrun")
    deploy_e2e_application(
        FLM_TEST_APP,
        command=runtime.command if runtime is not None else None,
        arguments=[*runtime.arguments[:-1], "e2e.instance_svc"] if runtime is not None else ["-m", "e2e.instance_svc"],
        shim=runtime.shim.name.lower() if runtime is not None and runtime.shim is not None else None,
        image=runtime.image if runtime is not None else None,
        environments=runtime.environments if runtime is not None else None,
        delay_release=5,
        max_instances=1,
    )

    yield

    # Clean up this application's sessions before unregistering.
    sessions = flamepy.list_sessions(application=FLM_TEST_APP)
    for sess in sessions:
        try:
            flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(FLM_TEST_APP)


# ============================================================================
# Serving Session API Tests
# ============================================================================


def test_create_service_session():
    """Test creating a service Session with application name."""
    session = open_session(FLM_TEST_APP, ctx=TestContext())

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    session_id = session.id()
    assert ssn_list[0].id == session_id
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    session.close()

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == session_id
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.CLOSED


def test_invoke_task_without_context():
    """Test invoking a task without common data using the service Session API."""
    session = open_session(name=FLM_TEST_APP, ctx=None)

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == session.id()
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    input = random_string()

    output = session.run(TestRequest(input=input))
    assert output.output == input
    assert output.common_data is None

    session.close()


def test_invoke_task_with_context():
    """Test invoking a task with context using the service Session API."""
    sys_context = random_string()
    input = random_string()

    session = open_session(FLM_TEST_APP, ctx=TestContext(common_data=sys_context))

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == session.id()
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    output = session.run(TestRequest(input=input))
    assert output.output == input
    assert output.common_data == sys_context

    session.close()


def test_patch_context():
    """Test changing context by appending to it using the service Session API."""
    sys_context = random_string()

    session = open_session(FLM_TEST_APP, ctx=TestContext(common_data=sys_context))

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == session.id()
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    previous_context = sys_context
    for _ in range(5):
        new_input_data = random_string()
        output = session.run(TestRequest(input=new_input_data, update_common_data=True))
        assert output.output == new_input_data
        assert output.common_data == previous_context

        ctx = session.context()
        assert ctx.common_data == new_input_data

        previous_context = new_input_data

    session.close()


def test_service_session_context():
    """Test getting service Session context."""
    sys_context = random_string()

    session = open_session(FLM_TEST_APP, ctx=TestContext(common_data=sys_context))

    ctx = session.context()
    assert ctx is not None
    assert isinstance(ctx, TestContext)
    assert ctx.common_data == sys_context

    session.close()


def test_service_session_context_manager():
    """Test using service Session as a context manager."""
    input = random_string()

    with open_session(FLM_TEST_APP, ctx=None) as session:
        ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
        assert len(ssn_list) == 1
        session_id = session.id()
        assert ssn_list[0].id == session_id

        output = session.run(TestRequest(input=input))
        assert output.output == input

    # After context exit, session should be closed
    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.id == session_id]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == session_id
    assert ssn_list[0].state == SessionState.CLOSED


def test_service_session_open_existing_session():
    """Test opening an existing session with service Session."""
    # First create a session and keep it open
    session1 = open_session(FLM_TEST_APP, ctx=TestContext(common_data=random_string()))
    session_id = session1.id()

    # Now open the same open session with service Session
    session2 = open_session(session_id=session_id)

    assert session2.id() == session_id
    assert session2.id() == session1.id()

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.id == session_id]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == session_id
    assert ssn_list[0].state == SessionState.OPEN

    session1.close()
    session2.close()


def test_service_session_validation():
    """Test that service Session raises ValueError when both name and session_id are None."""
    with pytest.raises(ValueError, match="Either 'name' or 'session_id' must be provided"):
        open_session(name=None, session_id=None)


def test_service_session_validation_both_provided():
    """Test that service Session raises ValueError when both name and session_id are provided."""
    with pytest.raises(ValueError, match="Cannot provide both 'name' and 'session_id'"):
        open_session(name=FLM_TEST_APP, session_id="some-session-id")


def _events(ref):
    # A fresh read observes writes from the executor and does not rely on a
    # local version surviving service shutdown or application removal.
    fresh = ObjectRef(endpoint=ref.endpoint, key=ref.key, version=0)
    return get_object(fresh, deserializer=lambda parts: parts)[1:]


def _wait_for_events(ref, predicate, timeout=90):
    deadline = time.monotonic() + timeout
    events = []
    while time.monotonic() < deadline:
        events = _events(ref)
        bad_enter = [event for event in events if event["phase"] == "session_enter" and not event["setup_complete"]]
        assert not bad_enter, f"FEM entered a session before instance setup: {bad_enter}"
        if predicate(events):
            return events
        time.sleep(0.1)
    pytest.fail(f"Timed out waiting for lifecycle events; observed: {events}")


def _wait_for_executor_removed(executor_id, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not any(executor.metadata.id == executor_id for executor in flamepy.list_executors()):
            return
        time.sleep(0.1)
    pytest.fail(f"Executor {executor_id} still exists after lifecycle teardown")


@pytest.mark.parametrize("setup_delay", [0, 31], ids=["ordinary-setup", "setup-longer-than-connection-timeout"])
def test_setup_retained_across_sessions_and_teardown_on_executor_release(setup_delay):
    """A deployed process initializes before enter, stays warm, and tears down once."""
    suffix = random_string(8).lower()
    application = f"flme2e-lifecycle-{suffix}"
    # The observer application owns the shared event log through teardown assertions.
    observer_application = f"flme2e-lifecycle-observer-{suffix}"
    flamepy.register_application(observer_application, flamepy.ApplicationAttributes())
    ref = None
    sessions = []
    deployed = False
    try:
        observer_prefix = ObjectKey.for_shared(observer_application).to_prefix()
        ref = put_object(observer_prefix, {"observer": application})
        # Reuse the cluster's configured runtime launch profile in CRI and
        # Kubernetes jobs; a standalone Host cluster needs no template.
        runtime = flamepy.get_application("flmrun")
        deploy_e2e_application(
            application,
            command=runtime.command if runtime is not None else None,
            arguments=[*runtime.arguments[:-1], "e2e.instance_svc"] if runtime is not None else ["-m", "e2e.instance_svc"],
            shim=runtime.shim.name.lower() if runtime is not None and runtime.shim is not None else None,
            image=runtime.image if runtime is not None else None,
            max_instances=1,
            delay_release=5,
            environments={
                **(runtime.environments or {} if runtime is not None else {}),
                "FLAME_E2E_LIFECYCLE_EVENTS": base64.b64encode(ref.encode()).decode(),
                "FLAME_E2E_SETUP_DELAY": str(setup_delay),
            },
        )
        deployed = True
        first = flamepy.create_session(application, min_instances=1, max_instances=1)
        sessions.append(first)

        # Warm-up binding happens with no queued tasks. An older FEM's SDK lazy
        # fallback cannot satisfy this assertion (or the service's enter guard).
        events = _wait_for_events(ref, lambda current: any(event["phase"] == "session_enter" for event in current))
        assert [event["phase"] for event in events] == ["setup_started", "setup_complete", "session_enter"]
        assert events[1]["at"] - events[0]["at"] >= setup_delay
        identity = (events[0]["executor_id"], events[0]["pid"])
        first_output = open_session(session_id=first.id).run(TestRequest(input="first"))
        assert first_output.output == "first"

        # The second session exists before releasing the first binding; one
        # application instance and the retention window allow immediate reuse.
        second = flamepy.create_session(application, max_instances=1)
        sessions.append(second)
        first.close()
        _wait_for_events(ref, lambda current: any(event["phase"] == "session_leave" for event in current))
        second_output = open_session(session_id=second.id).run(TestRequest(input="second"))
        assert second_output.output == "second"
        events = _events(ref)
        assert sum(event["phase"] == "setup_complete" for event in events) == 1
        assert not any(event["phase"] == "teardown" for event in events)

        second.close()
        _wait_for_events(ref, lambda current: any(event["phase"] == "teardown" for event in current))
        _wait_for_executor_removed(identity[0])
        events = _events(ref)
        assert [event["phase"] for event in events] == ["setup_started", "setup_complete", "session_enter", "task", "session_leave", "session_enter", "task", "session_leave", "teardown"]
        assert {(event["executor_id"], event["pid"]) for event in events} == {identity}
        assert [event["session_id"] for event in events if event["phase"] == "session_enter"] == [first.id, second.id]
    finally:
        for session in sessions:
            session.close()
        if deployed:
            flamepy.unregister_application(application)
        flamepy.unregister_application(observer_application)

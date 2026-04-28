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

import flamepy
import pytest
from flamepy import SessionState
from flamepy.agent import Agent

from e2e.api import TestContext, TestRequest
from tests.utils import random_string

FLM_TEST_APP = "flme2e"


@pytest.fixture(scope="module", autouse=True)
def setup_test_env():
    flamepy.register_application(
        FLM_TEST_APP,
        flamepy.ApplicationAttributes(
            command="python3",
            working_directory="/opt/e2e",
            environments={"FLAME_LOG_LEVEL": "DEBUG", "PYTHONPATH": "/opt/e2e/src"},
            arguments=["src/e2e/instance_svc.py", "src/e2e/api.py"],
            installer="python",
        ),
    )

    yield

    # Clean up all sessions before unregistering
    sessions = flamepy.list_sessions()
    for sess in sessions:
        try:
            flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application(FLM_TEST_APP)


# ============================================================================
# Agent API Tests
# ============================================================================


def test_create_agent():
    """Test creating an Agent with application name."""
    agent = Agent(FLM_TEST_APP, ctx=TestContext())

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    agent_id = agent.id()
    assert ssn_list[0].id == agent_id
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    agent.close()

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == agent_id
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.CLOSED


def test_invoke_task_without_context():
    """Test invoking a task without common data using Agent API."""
    agent = Agent(name=FLM_TEST_APP, ctx=None)

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == agent.id()
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    input = random_string()

    output = agent.invoke(TestRequest(input=input))
    assert output.output == input
    assert output.common_data is None

    agent.close()


def test_invoke_task_with_context():
    """Test invoking a task with context using Agent API."""
    sys_context = random_string()
    input = random_string()

    agent = Agent(FLM_TEST_APP, ctx=TestContext(common_data=sys_context))

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == agent.id()
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    output = agent.invoke(TestRequest(input=input))
    assert output.output == input
    assert output.common_data == sys_context

    agent.close()


def test_update_context():
    """Test updating context using Agent API."""
    sys_context = random_string()

    agent = Agent(FLM_TEST_APP, ctx=TestContext(common_data=sys_context))

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == agent.id()
    assert ssn_list[0].application == FLM_TEST_APP
    assert ssn_list[0].state == SessionState.OPEN

    previous_context = sys_context
    for _ in range(5):
        new_input_data = random_string()
        output = agent.invoke(TestRequest(input=new_input_data, update_common_data=True))
        assert output.output == new_input_data
        assert output.common_data == previous_context

        ctx = agent.context()
        assert ctx.common_data == new_input_data

        previous_context = new_input_data

    agent.close()


def test_agent_context():
    """Test getting Agent context."""
    sys_context = random_string()

    agent = Agent(FLM_TEST_APP, ctx=TestContext(common_data=sys_context))

    ctx = agent.context()
    assert ctx is not None
    assert isinstance(ctx, TestContext)
    assert ctx.common_data == sys_context

    agent.close()


def test_agent_context_manager():
    """Test using Agent as a context manager."""
    input = random_string()

    with Agent(FLM_TEST_APP, ctx=None) as agent:
        ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.state == SessionState.OPEN]
        assert len(ssn_list) == 1
        agent_id = agent.id()
        assert ssn_list[0].id == agent_id

        output = agent.invoke(TestRequest(input=input))
        assert output.output == input

    # After context exit, session should be closed
    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.id == agent_id]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == agent_id
    assert ssn_list[0].state == SessionState.CLOSED


def test_agent_open_existing_session():
    """Test opening an existing session with Agent."""
    # First create a session and keep it open
    agent1 = Agent(FLM_TEST_APP, ctx=TestContext(common_data=random_string()))
    agent_id = agent1.id()

    # Now open the same open session with Agent
    agent2 = Agent(session_id=agent_id)

    assert agent2.id() == agent_id
    assert agent2.id() == agent1.id()

    ssn_list = [s for s in flamepy.list_sessions() if s.application == FLM_TEST_APP and s.id == agent_id]
    assert len(ssn_list) == 1
    assert ssn_list[0].id == agent_id
    assert ssn_list[0].state == SessionState.OPEN

    agent1.close()
    agent2.close()


def test_agent_validation():
    """Test that Agent raises ValueError when both name and session_id are None."""
    with pytest.raises(ValueError, match="Either 'name' or 'session_id' must be provided"):
        Agent(name=None, session_id=None)


def test_agent_validation_both_provided():
    """Test that Agent raises ValueError when both name and session_id are provided."""
    with pytest.raises(ValueError, match="Cannot provide both 'name' and 'session_id'"):
        Agent(name=FLM_TEST_APP, session_id="some-session-id")

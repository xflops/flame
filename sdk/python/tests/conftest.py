import time
from datetime import datetime, timezone

import pytest


class _FakeChannel:
    def __init__(self, location: str):
        self.location = location
        self.closed = False

    def close(self):
        self.closed = True


class _FakeFrontend:
    def __init__(self):
        pass


@pytest.fixture
def sample_event():
    from flamepy.core.types import Event

    return Event(code=0, message="sample", creation_time=int(time.time() * 1000))


@pytest.fixture
def sample_session_attrs():
    from flamepy.core.types import SessionAttributes

    return SessionAttributes(application="test-app")


@pytest.fixture
def sample_task():
    from flamepy.core.types import Task, TaskState

    return Task(id="task-1", session_id="sess-1", state=TaskState.PENDING, creation_time=datetime.now(timezone.utc))


@pytest.fixture
def sample_application():
    from flamepy.core.types import Application, ApplicationState

    return Application(id="app-1", name="test-app", state=ApplicationState.ENABLED, creation_time=datetime.now(timezone.utc))

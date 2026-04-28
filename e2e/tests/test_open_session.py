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

from tests.utils import random_string


@pytest.fixture(scope="module", autouse=True)
def setup_test_app():
    """Setup test application for open_session tests."""
    flamepy.register_application(
        "flmtest-open-session",
        flamepy.ApplicationAttributes(),
    )

    yield

    # Clean up all sessions before unregistering
    sessions = flamepy.list_sessions()
    for sess in sessions:
        try:
            if sess.application == "flmtest-open-session":
                flamepy.close_session(sess.id)
        except Exception:
            pass

    flamepy.unregister_application("flmtest-open-session")


def test_open_session_existing():
    """Test open_session returns existing session when no spec provided."""
    # First create a session
    session_id = f"test-open-existing-{random_string(8)}"
    created_session = flamepy.create_session(
        application="flmtest-open-session",
        session_id=session_id,
    )

    # Open the existing session without spec
    opened_session = flamepy.open_session(session_id)

    # Verify it's the same session
    assert opened_session.id == created_session.id
    assert opened_session.application == created_session.application
    assert opened_session.state == flamepy.SessionState.OPEN

    # Clean up
    flamepy.close_session(session_id)


def test_open_session_create_with_spec():
    """Test open_session creates new session when spec provided and session doesn't exist."""
    session_id = f"test-open-create-{random_string(8)}"

    # Open session with spec - should create it
    spec = flamepy.SessionAttributes(
        id=session_id,
        application="flmtest-open-session",
        slots=1,
        min_instances=0,
        max_instances=5,
    )
    session = flamepy.open_session(session_id, spec=spec)

    # Verify session was created
    assert session.id == session_id
    assert session.application == "flmtest-open-session"
    assert session.state == flamepy.SessionState.OPEN

    # Clean up
    flamepy.close_session(session_id)


def test_open_session_existing_with_matching_spec():
    """Test open_session returns existing session when spec matches."""
    session_id = f"test-open-match-{random_string(8)}"

    # Create session with specific spec
    spec = flamepy.SessionAttributes(
        id=session_id,
        application="flmtest-open-session",
        slots=1,
        min_instances=0,
        max_instances=10,
    )
    created_session = flamepy.create_session(
        application="flmtest-open-session",
        session_id=session_id,
        slots=1,
        min_instances=0,
        max_instances=10,
    )

    # Open with same spec - should succeed
    opened_session = flamepy.open_session(session_id, spec=spec)

    # Verify it's the same session
    assert opened_session.id == created_session.id
    assert opened_session.application == created_session.application

    # Clean up
    flamepy.close_session(session_id)


def test_open_session_existing_with_mismatched_spec():
    """Test open_session raises error when spec doesn't match existing session."""
    session_id = f"test-open-mismatch-{random_string(8)}"

    # Create session with specific spec
    flamepy.create_session(
        application="flmtest-open-session",
        session_id=session_id,
        slots=1,
        min_instances=0,
        max_instances=10,
    )

    # Try to open with different spec - should fail
    mismatched_spec = flamepy.SessionAttributes(
        id=session_id,
        application="flmtest-open-session",
        slots=2,  # Different slots
        min_instances=0,
        max_instances=10,
    )

    with pytest.raises(Exception) as exc_info:
        flamepy.open_session(session_id, spec=mismatched_spec)

    # Verify error message mentions spec mismatch
    assert "spec mismatch" in str(exc_info.value).lower() or "slots" in str(exc_info.value).lower()

    # Clean up
    flamepy.close_session(session_id)


def test_open_session_not_found_without_spec():
    """Test open_session raises error when session doesn't exist and no spec provided."""
    non_existent_id = f"non-existent-{random_string(8)}"

    with pytest.raises(Exception) as exc_info:
        flamepy.open_session(non_existent_id)

    # Verify error indicates session not found
    assert "not found" in str(exc_info.value).lower()


def test_open_session_idempotent():
    """Test open_session is idempotent - multiple calls return same session."""
    session_id = f"test-open-idempotent-{random_string(8)}"

    spec = flamepy.SessionAttributes(
        id=session_id,
        application="flmtest-open-session",
        slots=1,
        min_instances=0,
        max_instances=5,
    )

    # First call creates the session
    session1 = flamepy.open_session(session_id, spec=spec)

    # Second call should return the same session
    session2 = flamepy.open_session(session_id, spec=spec)

    # Third call without spec should also work
    session3 = flamepy.open_session(session_id)

    # All should be the same session
    assert session1.id == session2.id == session3.id
    assert session1.application == session2.application == session3.application

    # Clean up
    flamepy.close_session(session_id)


def test_open_session_closed_session():
    """Test open_session raises error when session exists but is closed."""
    session_id = f"test-open-closed-{random_string(8)}"

    # Create and close a session
    flamepy.create_session(
        application="flmtest-open-session",
        session_id=session_id,
    )
    flamepy.close_session(session_id)

    # Try to open the closed session - should fail
    with pytest.raises(Exception) as exc_info:
        flamepy.open_session(session_id)

    # Verify error indicates session is not open
    assert "not open" in str(exc_info.value).lower() or "closed" in str(exc_info.value).lower() or "invalid" in str(exc_info.value).lower()

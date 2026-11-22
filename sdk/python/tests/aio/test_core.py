"""AsyncIO frontend client integration tests."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from flamepy.core import aio
from flamepy.core import client as sync_client
from flamepy.core._bridge import LoopThread
from flamepy.core.types import ApplicationAttributes, Event, FlameError, SessionAttributes, SessionState, TaskState


def test_aio_frontend_api_parity(frontend_server):
    endpoint, service, server_loop = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            await connection.register_application("app", ApplicationAttributes())
            await connection.unregister_application("app")
            assert len(await connection.list_applications()) == 1
            assert (await connection.get_application("app")).name == "app"
            assert await connection.get_application("missing") is None
            assert not (await connection.list_executors())
            assert not (await connection.list_nodes())
            session = await connection.create_session(SessionAttributes(application="app"))
            assert session.common_data() == b""
            assert (await connection.open_session(session.id)).id == session.id
            assert (await connection.get_session(session.id)).id == session.id
            assert len(await connection.list_sessions()) == 1
            assert (await session.create_task(b"input")).id == "task-1"
            assert (await session.get_task("task-1")).output == b"done"
            updates = session.watch_task("task-1")
            assert (await updates.__anext__()).state == TaskState.SUCCEED
            updates.close()
            assert await session.run(b"input") == b"done"
            submitted = session.submit(b"input")
            assert isinstance(submitted, asyncio.Task)
            assert await submitted == b"done"
            assert await session.run(b"input") == b"done"
            suspended = await session.suspend()
            assert suspended.state == SessionState.SUSPENDING and session.state == SessionState.SUSPENDING
            assert (await connection.suspend_session(session.id)).id == session.id
            assert (await session.resume()).state == SessionState.OPEN and session.state == SessionState.OPEN
            assert (await connection.resume_session(session.id)).id == session.id
            names = [request.DESCRIPTOR.name for request in service.requests]
            assert names.count("SuspendSessionRequest") == 2 and names.count("ResumeSessionRequest") == 2
            service.reject_suspend = True
            with pytest.raises(FlameError, match="suspend rejected"):
                await session.suspend()
            await session.close()

    asyncio.run(exercise())
    assert service.watches == 1


def test_aio_watchers_share_session_stream_and_close_reports_error(frontend_server):
    endpoint, service, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            session = await connection.create_session(SessionAttributes(application="app"))
            first = session.watch_task("task-1")
            second = session.watch_task("task-1")
            assert (await first.__anext__()).state == TaskState.SUCCEED
            assert (await second.__anext__()).state == TaskState.SUCCEED
            with pytest.raises(StopAsyncIteration):
                await first.__anext__()

            held = session.watch_task("hold")
            assert (await held.__anext__()).state == TaskState.PENDING
            second_held = session.watch_task("hold")
            assert (await second_held.__anext__()).state == TaskState.PENDING
            assert service.watch_requests.count("hold") == 2
            await session.close()
            with pytest.raises(FlameError, match="session closed"):
                await held.__anext__()
            with pytest.raises(FlameError, match="session closed"):
                await second_held.__anext__()

    asyncio.run(exercise())
    assert service.watches == 1


def test_aio_watch_failure_and_cancellation(frontend_server):
    endpoint, service, server_loop = frontend_server

    async def exercise():
        connection = await aio.connect(endpoint)
        try:
            session = await connection.create_session(SessionAttributes(application="app"))
            watcher = session.watch_task("error")
            assert (await watcher.__anext__()).state == TaskState.PENDING
            with pytest.raises(FlameError, match="watch failed"):
                await watcher.__anext__()
            result = session.submit(b"hold")
            result.cancel()
            with pytest.raises(asyncio.CancelledError):
                await result
            await asyncio.sleep(0)
        finally:
            await connection.close()

    asyncio.run(exercise())


def test_aio_watch_timeout_and_connection_close_error(frontend_server):
    endpoint, _, _ = frontend_server

    async def exercise():
        connection = await aio.connect(endpoint)
        session = await connection.create_session(SessionAttributes(application="app"))
        watcher = session.watch_task("hold", timeout=0.01)
        assert (await watcher.__anext__()).state == TaskState.PENDING
        with pytest.raises(TimeoutError, match="watch_task timed out"):
            await watcher.__anext__()

        result = session.submit(b"hold")
        await connection.close()
        with pytest.raises(FlameError, match="connection closed"):
            await result

    asyncio.run(exercise())


def test_aio_submit_then_close_settles_submitted_task(frontend_server):
    endpoint, _, _ = frontend_server

    async def exercise():
        connection = await aio.connect(endpoint)
        session = await connection.create_session(SessionAttributes(application="app"))
        submitted = session.submit(b"input")
        await connection.close()
        assert submitted.done()
        with pytest.raises(FlameError, match="connection closed"):
            await submitted

    asyncio.run(exercise())


def test_aio_submit_reports_create_task_error_on_task(frontend_server):
    endpoint, service, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            session = await connection.create_session(SessionAttributes(application="app"))
            service.reject_create_task = True
            submitted = session.submit(b"input")
            with pytest.raises(FlameError, match="task rejected"):
                await submitted

    asyncio.run(exercise())


def test_aio_submit_close_during_create_task_settles_submitted_task(frontend_server):
    endpoint, service, server_loop = frontend_server
    service.create_task_gate = asyncio.Event()

    async def exercise():
        connection = await aio.connect(endpoint)
        session = await connection.create_session(SessionAttributes(application="app"))
        submitted = session.submit(b"input")
        assert await asyncio.to_thread(service.create_task_started.wait, 3)
        await connection.close()
        assert submitted.done()
        with pytest.raises(FlameError, match="connection closed"):
            await submitted

    try:
        asyncio.run(exercise())
    finally:
        server_loop.loop.call_soon_threadsafe(service.create_task_gate.set)


def test_aio_connection_rejects_other_loop(frontend_server):
    endpoint, _, _ = frontend_server
    owner = LoopThread("test-aio-owner")

    async def make_connection():
        return await aio.connect(endpoint)

    connection = owner.call(make_connection())

    async def misuse():
        with pytest.raises(RuntimeError, match="another event loop"):
            await connection.list_nodes()

    try:
        asyncio.run(misuse())
    finally:
        owner.call(connection.close())
        owner.close()


def test_asyncio_wrap_future_on_sync_core_result(frontend_server):
    endpoint, _, _ = frontend_server
    connection = sync_client.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="app"))

        async def wait_for_result():
            future = session.submit(b"input")
            return await asyncio.wrap_future(future)

        assert asyncio.run(wait_for_result()) == b"done"
    finally:
        connection.close()


def test_close_wakes_pending_async_watch_read(frontend_server):
    endpoint, _, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            session = await connection.create_session(SessionAttributes(application="app"))
            watcher = session.watch_task("hold")
            assert (await watcher.__anext__()).state == TaskState.PENDING
            waiting = asyncio.create_task(watcher.__anext__())
            await asyncio.sleep(0)
            watcher.close()
            with pytest.raises(StopAsyncIteration):
                await asyncio.wait_for(waiting, 1)

    asyncio.run(exercise())


@pytest.mark.parametrize("submit", [False, True])
def test_remote_cancelled_task_raises_instead_of_returning_output(frontend_server, monkeypatch, submit):
    endpoint, frontend, _ = frontend_server
    original = frontend._task

    def cancelled_task(task_id="task-1", state=TaskState.SUCCEED):
        task = original(task_id, TaskState.CANCELLED)
        task.spec.ClearField("output")
        return task

    monkeypatch.setattr(frontend, "_task", cancelled_task)

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            session = await connection.create_session(SessionAttributes(application="app"))
            with pytest.raises(FlameError, match="Task was cancelled"):
                await (session.submit(b"input") if submit else session.run(b"input"))

    asyncio.run(exercise())


@pytest.mark.parametrize(
    ("events", "details"),
    [
        (
            [Event(42, "Waiting for resources"), Event(73, "Executor unavailable"), Event(9999)],
            "[42] Waiting for resources; [73] Executor unavailable",
        ),
        (
            [Event(1001, "Image pull TLS failure"), Event(1002, "Session retry limit reached"), Event(74, "Releasing executor")],
            "[1001] Image pull TLS failure; [1002] Session retry limit reached; [74] Releasing executor",
        ),
    ],
)
def test_aio_watch_timeout_includes_session_events(frontend_server, monkeypatch, events, details):
    endpoint, _, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            session = await connection.create_session(SessionAttributes(application="app"))
            diagnostics = AsyncMock(return_value=SimpleNamespace(id=session.id, events=events))
            monkeypatch.setattr(connection, "get_session", diagnostics)
            watcher = session.watch_task("hold", timeout=0.03)
            assert (await watcher.__anext__()).state == TaskState.PENDING
            diagnostics.assert_not_called()
            with pytest.raises(TimeoutError) as caught:
                await watcher.__anext__()
            assert str(caught.value) == f"watch_task timed out after 0.03 seconds; session {session.id!r} events: {details}"
            diagnostics.assert_awaited_once_with(session.id)
            # A diagnostic timeout does not close the shared watch or settle
            # queued tasks: another subscriber can still receive its result.
            completed = session.watch_task("task-1")
            assert (await completed.__anext__()).state == TaskState.SUCCEED

    asyncio.run(exercise())


@pytest.mark.parametrize("diagnostic", ["failure", "hang", "no_events", "empty_messages"])
def test_aio_watch_timeout_preserves_error_when_diagnostics_unavailable(frontend_server, monkeypatch, diagnostic):
    from flamepy.core.aio import client as aio_client

    endpoint, _, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            session = await connection.create_session(SessionAttributes(application="app"))

            async def diagnostics(_):
                if diagnostic == "failure":
                    raise RuntimeError("manager unavailable")
                if diagnostic == "hang":
                    await asyncio.Event().wait()
                return SimpleNamespace(id=session.id, events=[Event(42), Event(73, "")] if diagnostic == "empty_messages" else [])

            monkeypatch.setattr(connection, "get_session", diagnostics)
            monkeypatch.setattr(aio_client, "_SESSION_DIAGNOSTIC_TIMEOUT", 0.01)
            watcher = session.watch_task("hold", timeout=0.03)
            assert (await watcher.__anext__()).state == TaskState.PENDING
            with pytest.raises(TimeoutError, match="^watch_task timed out after 0.03 seconds$"):
                await asyncio.wait_for(watcher.__anext__(), 1)

    asyncio.run(exercise())

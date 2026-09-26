"""AsyncIO frontend client integration tests."""

import asyncio

import pytest

from flamepy.core import aio
from flamepy.core import client as sync_client
from flamepy.core._bridge import LoopThread
from flamepy.core.types import ApplicationAttributes, FlameError, SessionAttributes, TaskState


def test_aio_frontend_api_parity(frontend_server):
    endpoint, service, server_loop = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            await connection.register_application("app", ApplicationAttributes())
            await connection.unregister_application("app")
            assert len(await connection.list_applications()) == 1
            assert (await connection.get_application("app")).name == "app"
            assert await connection.get_application("missing") is None
            assert await connection.list_executors() == []
            assert await connection.list_nodes() == []
            session = await connection.create_session(SessionAttributes(application="app"))
            assert session.common_data() == b""
            assert (await connection.open_session(session.id)).id == session.id
            assert (await connection.get_session(session.id)).id == session.id
            assert len(await connection.list_sessions()) == 1
            assert (await session.create_task(b"input")).id == "task-1"
            assert (await session.get_task("task-1")).output == b"done"
            assert [task.id async for task in session.list_tasks()] == ["task-1"]
            updates = session.watch_task("task-1")
            assert (await updates.__anext__()).state == TaskState.SUCCEED
            updates.close()
            result = await session.run(b"input")
            assert isinstance(result, asyncio.Future)
            assert await result == b"done"
            assert await session.invoke(b"input") == b"done"
            await session.close()

    asyncio.run(exercise())
    assert service.watches == 3


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
            result = await session.run(b"hold")
            result.cancel()
            with pytest.raises(asyncio.CancelledError):
                await result
            await asyncio.sleep(0)
        finally:
            await connection.close()

    asyncio.run(exercise())


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
            future = await asyncio.to_thread(session.run, b"input")
            return await asyncio.wrap_future(future)

        assert asyncio.run(wait_for_result()) == b"done"
    finally:
        connection.close()

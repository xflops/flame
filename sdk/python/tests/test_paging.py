"""Paging envelopes, exact filters and explicit page selection."""

import asyncio
from dataclasses import asdict
from unittest.mock import AsyncMock

import pytest

from flamepy.core import ApplicationFilter, ExecutorFilter, NodeFilter, Page, Paging, SessionFilter, TaskFilter, aio, connect
from flamepy.core.types import ApplicationState, FlameError, SessionState, TaskState
from flamepy.proto import types_pb2 as pb


def test_sync_pages_preserve_metadata_and_forward_filters(frontend_server):
    endpoint, service, _ = frontend_server
    connection = connect(endpoint)
    try:
        apps = connection.list_applications(ApplicationFilter(names=["app"], state=ApplicationState.ENABLED, paging=Paging(limit=7, number=3)))
        assert isinstance(apps, Page)
        assert apps.paging == Paging(limit=7, number=3, total=1)
        assert apps[0].name == "app"
        assert asdict(apps)["paging"]["total"] == 1
        request = service.requests[-1]
        assert list(request.names) == ["app"]
        assert request.HasField("state") and request.state == 0
        executors = connection.list_executors(ExecutorFilter(ids=["e"], application="app", node="node", session_id="s", state=2, paging=Paging(limit=5)))
        assert executors.paging.total == 0
        request = service.requests[-1]
        assert (list(request.ids), request.application, request.node, request.session_id, request.state) == (["e"], "app", "node", "s", 2)
        connection.list_nodes(NodeFilter(names=["node"], state=1))
        assert list(service.requests[-1].names) == ["node"]
        sessions = connection.list_sessions(SessionFilter(ids=["s"], state=SessionState.CLOSED, application="app", paging=Paging(number=2)))
        assert sessions.paging.number == 2
        assert sessions[0].connection is connection
        tasks = connection.list_tasks("s", TaskFilter(ids=[1], states=[TaskState.SUCCEED], paging=Paging(limit=1)))
        assert tasks.paging == Paging(limit=1, number=1, total=1)
        assert tasks[0].input is None and tasks[0].output is None
        request = service.requests[-1]
        assert request.session_id == "s"
        assert list(request.ids) == [1] and list(request.states) == [2]

    finally:
        connection.close()


def test_async_callers_select_pages_through_filter(frontend_server):
    endpoint, _, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            requests = []

            async def reply(method, request, _operation):
                assert method == "ListNodes"
                requests.append(request)
                return pb.NodeList(paging=pb.Paging(limit=1, number=request.paging.number, total=2))

            connection._rpc = reply
            filter = NodeFilter(names=["node"], paging=Paging(limit=1, number=1))
            first = await connection.list_nodes(filter)
            assert first.paging == Paging(limit=1, number=1, total=2)
            assert len(requests) == 1
            filter.paging = Paging(limit=1, number=2)
            second = await connection.list_nodes(filter)
            assert second.paging == Paging(limit=1, number=2, total=2)
            assert [request.paging.number for request in requests] == [1, 2]
            assert all(list(request.names) == ["node"] for request in requests)

    asyncio.run(exercise())


def test_missing_totals_are_rejected_even_for_empty_pages(frontend_server):
    endpoint, _, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            connection._rpc = AsyncMock(return_value=pb.NodeList(paging=pb.Paging(limit=100, number=1)))
            with pytest.raises(FlameError, match="paging"):
                await connection.list_nodes()
            connection._rpc = AsyncMock(return_value=pb.NodeList())
            with pytest.raises(FlameError, match="paging"):
                await connection.list_nodes()

    asyncio.run(exercise())


@pytest.mark.parametrize("limit,number", [(0, 1), (1001, 1), (1, 0), (1, -1)])
def test_invalid_response_bounds_are_rejected(frontend_server, limit, number):
    endpoint, _, _ = frontend_server

    async def exercise():
        async with await aio.connect(endpoint) as connection:
            connection._rpc = AsyncMock(return_value=pb.NodeList(paging=pb.Paging(limit=limit, number=number, total=0)))
            with pytest.raises(FlameError, match="paging"):
                await connection.list_nodes()

    asyncio.run(exercise())

"""Paged list contracts, including the existing task stream's binary header."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock

import grpc
import pytest

from flamepy import Page, PageInfo, PageRequest
from flamepy.core.aio.client import Connection
from flamepy.core.client import Connection as SyncConnection
from flamepy.core.types import ApplicationState, FlameError, FlameErrorCode, SessionState, TaskState
from flamepy.proto import types_pb2 as rpc


class TaskStream:
    def __init__(self, items=(), headers=None, code=grpc.StatusCode.OK, completed=False):
        self.items = list(items)
        self.headers = [("flame-page-bin", b"")] if headers is None else headers
        self.status = code
        self.completed = completed
        self.cancelled = False
        self.consumed = 0

    async def initial_metadata(self):
        return self.headers

    def done(self):
        return self.completed

    async def code(self):
        return self.status

    async def details(self):
        return "invalid page number"

    def cancel(self):
        self.cancelled = True

    def __aiter__(self):
        return self

    async def __anext__(self):
        self.consumed += 1
        if not self.items:
            raise StopAsyncIteration
        return self.items.pop(0)


def connection(stub):
    return Connection("fake", None, stub)


@pytest.mark.parametrize("size", [-1, 1001, True, 1.5])
def test_page_request_size_validation(size):
    with pytest.raises(FlameError, match="page_size"):
        PageRequest(page_size=size)


def test_unary_presence_filters_legacy_and_lazy_iteration():
    async def exercise():
        stub = SimpleNamespace(
            ListApplications=AsyncMock(return_value=rpc.ApplicationList(paging=rpc.PageInfo(total_count=8, resource_count=9, state_counts={"Enabled": 8}))),
            ListSessions=AsyncMock(return_value=rpc.SessionList(paging=rpc.PageInfo())),
            ListNodes=AsyncMock(return_value=rpc.NodeList(paging=rpc.PageInfo())),
            ListExecutors=AsyncMock(return_value=rpc.ExecutorList(paging=rpc.PageInfo())),
        )
        client = connection(stub)
        assert await client.list_applications() == []
        assert not stub.ListApplications.call_args.args[0].HasField("paging")
        page = await client.list_applications_page(state=ApplicationState.ENABLED, names=["app"])
        request = stub.ListApplications.call_args.args[0]
        assert request.HasField("paging") and request.paging.page_size == 0
        assert request.HasField("state") and request.state == 0 and list(request.names) == ["app"]
        assert page.paging == PageInfo(total_count=8, resource_count=9, state_counts={"Enabled": 8})
        await client.list_sessions_page(state=SessionState.CLOSED, ids=["s"], application="app")
        request = stub.ListSessions.call_args.args[0]
        assert request.state == 1 and list(request.ids) == ["s"] and request.application == "app"
        await client.list_nodes_page(state=0, names=["node"])
        assert stub.ListNodes.call_args.args[0].HasField("state")
        await client.list_executors_page(state=0, ids=["e"], application="a", node="n", session_id="s")
        request = stub.ListExecutors.call_args.args[0]
        assert request.application == "a" and request.node == "n" and request.session_id == "s" and list(request.ids) == ["e"]
        stub.ListNodes.reset_mock()
        stub.ListNodes.side_effect = [rpc.NodeList(paging=rpc.PageInfo(page_number=1, page_size=2, total_pages=2)), rpc.NodeList(paging=rpc.PageInfo())]
        pages = client.iter_nodes_pages(PageRequest(page_size=2, order_by="name desc"), names=iter(["node"]))
        assert stub.ListNodes.call_count == 0
        assert len([page async for page in pages]) == 2
        requests = [call.args[0] for call in stub.ListNodes.call_args_list]
        assert [req.paging.page_number for req in requests] == [0, 2]
        assert all(list(req.names) == ["node"] and req.paging.order_by == "name desc" for req in requests)
        stub.ListNodes.side_effect = None
        stub.ListNodes.return_value = rpc.NodeList()
        with pytest.raises(FlameError, match="does not support"):
            await client.list_nodes_page()

    asyncio.run(exercise())


def test_task_page_binary_metadata_and_filters():
    async def exercise():
        stream = TaskStream([rpc.Task(metadata=rpc.Metadata(id="42"))], headers=[("flame-page-bin", rpc.PageInfo(total_count=3, page_number=1, page_size=1, total_pages=3).SerializeToString())])
        requests = []

        def list_tasks(request):
            requests.append(request)
            return stream

        client = connection(SimpleNamespace(ListTasks=list_tasks))
        page = await client.list_tasks_page("session", PageRequest(page_size=1), state=TaskState.SUCCEED, ids=["42"], include_payloads=True)
        assert isinstance(page, Page) and page.items[0].id == "42" and page.paging.total_count == 3
        assert page.items[0].session_id == "session" and not stream.cancelled
        assert requests[0].HasField("paging") and list(requests[0].ids) == [42] and requests[0].state == 2 and requests[0].include_payloads
        empty = TaskStream()
        client._frontend.ListTasks = lambda _: empty
        assert (await client.list_tasks_page("session")).paging == PageInfo()

    asyncio.run(exercise())


@pytest.mark.parametrize("headers,code,completed,error", [([], grpc.StatusCode.OK, False, "does not support"), ([("flame-page-bin", b"\xff")], grpc.StatusCode.OK, False, "invalid task page metadata"), ([], grpc.StatusCode.INVALID_ARGUMENT, True, "invalid page number")])
def test_task_header_failure_cancels_without_consuming(headers, code, completed, error):
    async def exercise():
        stream = TaskStream(headers=headers, code=code, completed=completed)
        with pytest.raises(FlameError, match=error) as failure:
            await connection(SimpleNamespace(ListTasks=lambda _: stream)).list_tasks_page("s")
        assert stream.cancelled and stream.consumed == 0
        if code == grpc.StatusCode.INVALID_ARGUMENT:
            assert failure.value.code == FlameErrorCode.INVALID_ARGUMENT

    asyncio.run(exercise())


def test_task_cancellation_and_excess_results():
    async def exercise():
        stream = TaskStream([rpc.Task(), rpc.Task()])
        with pytest.raises(FlameError, match="more tasks"):
            await connection(SimpleNamespace(ListTasks=lambda _: stream)).list_tasks_page("s", PageRequest(page_size=1))
        assert stream.cancelled

        class HeldStream(TaskStream):
            async def initial_metadata(self):
                raise asyncio.CancelledError

        held = HeldStream()
        with pytest.raises(asyncio.CancelledError):
            await connection(SimpleNamespace(ListTasks=lambda _: held)).list_tasks_page("s")
        assert held.cancelled

    asyncio.run(exercise())


def test_sync_iteration_is_lazy_and_uses_bounded_pages():
    client = object.__new__(SyncConnection)
    calls = []

    def fetch(paging, **filters):
        calls.append((paging, filters))
        return Page([], PageInfo(page_number=len(calls), total_pages=2))

    client.list_nodes_page = fetch
    pages = client.iter_nodes_pages(PageRequest(order_by="name desc"), names=iter(["node"]))
    assert not calls
    assert len(list(pages)) == 2
    assert calls[1][0].page_number == 2 and calls[1][0].order_by == "name desc"
    assert all(filters["names"] == ("node",) for _, filters in calls)


def test_real_grpc_initial_binary_metadata_and_rejected_page():
    """Use actual gRPC framing for empty pages, errors, and legacy streams."""
    from flamepy.proto import frontend_pb2_grpc

    class Service(frontend_pb2_grpc.FrontendServicer):
        async def ListTasks(self, request, context):  # noqa: N802 - gRPC method name
            if request.paging.page_number == 99:
                await context.abort(grpc.StatusCode.INVALID_ARGUMENT, "invalid page number")
            if request.paging.page_number == 98:
                yield rpc.Task()
                return
            await context.send_initial_metadata((("flame-page-bin", rpc.PageInfo(total_count=7).SerializeToString()),))
            if request.session_id == "empty":
                return
            yield rpc.Task(metadata=rpc.Metadata(id="1"))

    async def exercise():
        server = grpc.aio.server()
        frontend_pb2_grpc.add_FrontendServicer_to_server(Service(), server)
        port = server.add_insecure_port("127.0.0.1:0")
        await server.start()
        try:
            async with await Connection.connect(f"http://127.0.0.1:{port}") as client:
                assert (await client.list_tasks_page("empty")).items == []
                page = await client.list_tasks_page("s")
                assert page.items[0].id == "1" and page.paging.total_count == 7
                with pytest.raises(FlameError, match="invalid page number") as failure:
                    await client.list_tasks_page("s", PageRequest(page_number=99))
                assert failure.value.code == FlameErrorCode.INVALID_ARGUMENT
                with pytest.raises(FlameError, match="does not support"):
                    await client.list_tasks_page("s", PageRequest(page_number=98))
        finally:
            await server.stop(None)

    asyncio.run(exercise())


def test_sync_page_bridge_and_session_wrapping():
    from flamepy.core.aio.client import _session_from_proto
    from flamepy.core.client import Session

    client = object.__new__(SyncConnection)
    aio_session = _session_from_proto(None, rpc.Session(metadata=rpc.Metadata(id="session")))
    client._aio = SimpleNamespace(
        list_sessions_page=AsyncMock(return_value=Page([aio_session], PageInfo(total_count=1))),
        list_tasks_page=AsyncMock(return_value=Page([], PageInfo())),
    )
    client._call = asyncio.run
    page = client.list_sessions_page(PageRequest(page_size=1), application="app")
    assert isinstance(page.items[0], Session) and page.items[0].id == "session"
    assert page.paging.total_count == 1
    assert client._aio.list_sessions_page.call_args.kwargs["application"] == "app"
    assert page.items[0].list_tasks_page().items == []
    assert client._aio.list_tasks_page.call_args.args[0] == "session"


@pytest.mark.parametrize("number", [-1, True, 1.5, 2**63])
def test_page_number_validation(number):
    with pytest.raises(FlameError, match="page_number"):
        PageRequest(page_number=number)


def test_page_iteration_starts_at_requested_number_and_stops_when_empty():
    async def exercise():
        stub = SimpleNamespace(ListNodes=AsyncMock(return_value=rpc.NodeList(paging=rpc.PageInfo(total_pages=4))))
        pages = [page async for page in connection(stub).iter_nodes_pages(PageRequest(page_number=3))]
        assert len(pages) == 2
        assert [call.args[0].paging.page_number for call in stub.ListNodes.call_args_list] == [3, 4]
        stub.ListNodes.reset_mock()
        stub.ListNodes.return_value = rpc.NodeList(paging=rpc.PageInfo(total_pages=0))
        assert len([page async for page in connection(stub).iter_nodes_pages()]) == 1
        assert stub.ListNodes.call_count == 1

    asyncio.run(exercise())

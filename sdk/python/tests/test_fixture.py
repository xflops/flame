"""In-process frontend gRPC fixture shared by sync and aio core tests."""

# Generated gRPC method names are capitalized.
# ruff: noqa: N802

import asyncio
import threading

import grpc
import pytest

from flamepy.core._bridge import LoopThread
from flamepy.core.types import SessionState, TaskState
from flamepy.proto import types_pb2 as pb
from flamepy.proto.frontend_pb2_grpc import FrontendServicer, add_FrontendServicer_to_server


class FrontendFixture(FrontendServicer):
    def __init__(self):
        self.watches = 0
        self.watch_requests = []
        self.release = asyncio.Event()
        self.requests = []
        self.reject_close = False
        self.reject_create_task = False
        self.create_task_gate = None
        self.create_task_started = threading.Event()

    def _session(self, session_id="sess-1"):
        return pb.Session(
            metadata=pb.Metadata(id=session_id),
            spec=pb.SessionSpec(application="app", common_data=b""),
            status=pb.SessionStatus(state=SessionState.OPEN, creation_time=1, events=[pb.Event(code=1001, message="test", creation_time=1)]),
        )

    def _task(self, task_id="task-1", state=TaskState.SUCCEED):
        return pb.Task(
            metadata=pb.Metadata(id=task_id),
            spec=pb.TaskSpec(session_id="sess-1", input=b"", output=b"done"),
            status=pb.TaskStatus(state=state, creation_time=1),
        )

    async def RegisterApplication(self, request, context):
        self.requests.append(request)
        return pb.Result()

    async def UnregisterApplication(self, request, context):
        return pb.Result()

    async def ListApplications(self, request, context):
        return pb.ApplicationList(applications=[await self.GetApplication(type("Request", (), {"name": "app"})(), context)])

    async def GetApplication(self, request, context):
        if request.name == "missing":
            await context.abort(grpc.StatusCode.NOT_FOUND, "missing")
        app = pb.Application(
            metadata=pb.Metadata(id="app-1", name=request.name),
            status=pb.ApplicationStatus(state=0, creation_time=1),
        )
        app.spec.image = ""
        app.spec.schema.CopyFrom(pb.ApplicationSchema(input=""))
        return app

    async def ListExecutors(self, request, context):
        return pb.ExecutorList()

    async def ListNodes(self, request, context):
        return pb.NodeList()

    async def CreateSession(self, request, context):
        self.requests.append(request)
        return self._session(request.session_id)

    async def OpenSession(self, request, context):
        return self._session(request.session_id)

    async def GetSession(self, request, context):
        return self._session(request.session_id)

    async def ListSessions(self, request, context):
        return pb.SessionList(sessions=[self._session()])

    async def CloseSession(self, request, context):
        if self.reject_close:
            await context.abort(grpc.StatusCode.FAILED_PRECONDITION, "close rejected")
        return self._session(request.session_id)

    async def CreateTask(self, request, context):
        if self.create_task_gate is not None:
            self.create_task_started.set()
            await self.create_task_gate.wait()
        if self.reject_create_task:
            await context.abort(grpc.StatusCode.INVALID_ARGUMENT, "task rejected")
        self.requests.append(request)
        task_id = "hold" if request.task.input == b"hold" else "task-1"
        return self._task(task_id, TaskState.PENDING)

    async def GetTask(self, request, context):
        return self._task(request.task_id)

    async def ListTasks(self, request, context):
        yield self._task()

    async def WatchTasks(self, requests, context):
        self.watches += 1
        updates = asyncio.Queue()
        release_tasks = []

        async def after_release(task_id):
            await self.release.wait()
            await updates.put(self._task(task_id))

        async def receive():
            async for request in requests:
                self.watch_requests.append(request.task_id)
                if request.task_id == "hold":
                    await updates.put(self._task("hold", TaskState.PENDING))
                    release_tasks.append(asyncio.create_task(after_release("hold")))
                elif request.task_id == "error":
                    await updates.put(self._task("error", TaskState.PENDING))
                    await updates.put(None)
                else:
                    await updates.put(self._task(request.task_id))

        reader = asyncio.create_task(receive())
        try:
            while True:
                update = await updates.get()
                if update is None:
                    await context.abort(grpc.StatusCode.INTERNAL, "watch failed")
                yield update
        finally:
            reader.cancel()
            for task in release_tasks:
                task.cancel()
            await asyncio.gather(reader, *release_tasks, return_exceptions=True)


@pytest.fixture
def frontend_server():
    bridge = LoopThread("test-frontend-server")
    fixture = FrontendFixture()

    async def start():
        server = grpc.aio.server()
        add_FrontendServicer_to_server(fixture, server)
        port = server.add_insecure_port("127.0.0.1:0")
        await server.start()
        return server, port

    server, port = bridge.call(start())
    try:
        yield f"http://127.0.0.1:{port}", fixture, bridge
    finally:
        bridge.call(server.stop(0))
        bridge.close()

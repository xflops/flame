"""AsyncIO frontend client and task watches."""

import asyncio
from datetime import datetime, timezone
from typing import Any, AsyncIterator, Dict, List, Optional, Union
from urllib.parse import urlparse

import grpc

from flamepy.core.types import (
    Application,
    ApplicationAttributes,
    ApplicationSchema,
    ApplicationState,
    Event,
    FlameClientTls,
    FlameError,
    FlameErrorCode,
    SessionAttributes,
    SessionID,
    SessionState,
    Shim,
    Task,
    TaskID,
    TaskOptions,
    TaskState,
    short_name,
)
from flamepy.proto.frontend_pb2 import (
    CloseSessionRequest,
    CreateSessionRequest,
    CreateTaskRequest,
    GetApplicationRequest,
    GetSessionRequest,
    GetTaskRequest,
    ListApplicationsRequest,
    ListExecutorsRequest,
    ListNodesRequest,
    ListSessionsRequest,
    ListTasksRequest,
    OpenSessionRequest,
    RegisterApplicationRequest,
    UnregisterApplicationRequest,
    WatchTaskRequest,
)
from flamepy.proto.frontend_pb2_grpc import FrontendStub
from flamepy.proto.types_pb2 import (
    ApplicationSchema as ApplicationSchemaProto,
)
from flamepy.proto.types_pb2 import (
    ApplicationSpec,
    Environment,
    SessionSpec,
    TaskSpec,
)
from flamepy.proto.types_pb2 import (
    ResourceRequirement as ResourceRequirementProto,
)


def _optional_field(message: Any, field: str) -> Any:
    return getattr(message, field) if message.HasField(field) else None


def _events_from_proto(events) -> List[Event]:
    return [Event(event.code, event.message, datetime.fromtimestamp(event.creation_time / 1000, tz=timezone.utc)) for event in events]


def _application_from_proto(app) -> Application:
    spec = app.spec
    schema = None
    if spec.HasField("schema"):
        schema = ApplicationSchema(
            input=_optional_field(spec.schema, "input"),
            output=_optional_field(spec.schema, "output"),
            common_data=_optional_field(spec.schema, "common_data"),
        )
    return Application(
        id=app.metadata.id,
        name=app.metadata.name,
        state=ApplicationState(app.status.state),
        creation_time=datetime.fromtimestamp(app.status.creation_time / 1000, tz=timezone.utc),
        shim=Shim(spec.shim),
        image=_optional_field(spec, "image"),
        description=_optional_field(spec, "description"),
        labels=list(spec.labels),
        command=_optional_field(spec, "command"),
        arguments=list(spec.arguments),
        environments={env.name: env.value for env in spec.environments},
        working_directory=_optional_field(spec, "working_directory"),
        max_instances=_optional_field(spec, "max_instances"),
        delay_release=_optional_field(spec, "delay_release"),
        schema=schema,
        url=_optional_field(spec, "url"),
        installer=_optional_field(spec, "installer"),
    )


def _session_from_proto(connection: "Connection", response) -> "Session":
    return Session(
        connection=connection,
        id=response.metadata.id,
        application=response.spec.application,
        state=SessionState(response.status.state),
        creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
        pending=response.status.pending,
        running=response.status.running,
        succeed=response.status.succeed,
        failed=response.status.failed,
        completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
        common_data=_optional_field(response.spec, "common_data"),
        events=_events_from_proto(response.status.events),
    )


def _task_from_proto(response, session_id: str) -> Task:
    return Task(
        id=response.metadata.id,
        session_id=session_id,
        state=TaskState(response.status.state),
        creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
        input=_optional_field(response.spec, "input"),
        output=_optional_field(response.spec, "output"),
        affinity=set(response.spec.affinity),
        completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
        events=_events_from_proto(response.status.events),
    )


def _raise_for_result(response, operation: str) -> None:
    if response.return_code != 0:
        raise FlameError(FlameErrorCode.INTERNAL, response.message or f"{operation} failed with return_code {response.return_code}")


def _grpc_error(error: grpc.RpcError, operation: str) -> FlameError:
    code = error.code()
    mapping = {
        grpc.StatusCode.NOT_FOUND: FlameErrorCode.NOT_FOUND,
        grpc.StatusCode.ALREADY_EXISTS: FlameErrorCode.ALREADY_EXISTS,
        grpc.StatusCode.INVALID_ARGUMENT: FlameErrorCode.INVALID_ARGUMENT,
        grpc.StatusCode.FAILED_PRECONDITION: FlameErrorCode.INVALID_STATE,
    }
    return FlameError(mapping.get(code, FlameErrorCode.INTERNAL), f"{operation}: {error.details() or ''}")


def _session_spec(attrs: SessionAttributes) -> SessionSpec:
    if attrs.common_data is not None and not isinstance(attrs.common_data, bytes):
        raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "common_data must be bytes in core API")
    spec = SessionSpec(
        application=attrs.application,
        common_data=attrs.common_data,
        min_instances=attrs.min_instances,
        max_instances=attrs.max_instances,
        batch_size=1,
    )
    if attrs.resreq is not None:
        spec.resreq.CopyFrom(ResourceRequirementProto(cpu=attrs.resreq.cpu, memory=attrs.resreq.memory, gpu=attrs.resreq.gpu))
    return spec


async def connect(addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
    return await Connection.connect(addr, tls_config)


class Connection:
    """One frontend gRPC channel owned by the event loop that created it."""

    def __init__(self, addr: str, channel: grpc.aio.Channel, frontend: FrontendStub):
        self.addr = addr
        self._channel = channel
        self._frontend = frontend
        self._loop = asyncio.get_running_loop()
        self._closed = False
        self._watches: set[asyncio.Task] = set()
        self._results: set[asyncio.Future] = set()

    @classmethod
    async def connect(cls, addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
        if not addr:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "address cannot be empty")
        channel = None
        try:
            parsed = urlparse(addr)
            host = parsed.hostname or parsed.path
            port = parsed.port or 8080
            target = f"{host}:{port}"
            if parsed.scheme == "https":
                roots = None
                if tls_config is not None and tls_config.ca_file:
                    with open(tls_config.ca_file, "rb") as file:
                        roots = file.read()
                channel = grpc.aio.secure_channel(target, grpc.ssl_channel_credentials(root_certificates=roots))
            else:
                channel = grpc.aio.insecure_channel(target)
            await asyncio.wait_for(channel.channel_ready(), timeout=10)
            return cls(addr, channel, FrontendStub(channel))
        except BaseException as error:
            if channel is not None:
                await channel.close()
            if isinstance(error, asyncio.CancelledError):
                raise
            if isinstance(error, FlameError):
                raise
            if isinstance(error, asyncio.TimeoutError):
                raise FlameError(FlameErrorCode.INVALID_CONFIG, f"timeout connecting to {addr}") from error
            raise FlameError(FlameErrorCode.INVALID_CONFIG, f"failed to connect to {addr}: {error}") from error

    def _check_loop(self) -> None:
        if asyncio.get_running_loop() is not self._loop:
            raise RuntimeError("aio Flame connection used from another event loop")
        if self._closed:
            raise RuntimeError("Flame connection is closed")

    async def __aenter__(self) -> "Connection":
        self._check_loop()
        return self

    async def __aexit__(self, *_exc) -> None:
        await self.close()

    async def close(self) -> None:
        if asyncio.get_running_loop() is not self._loop:
            raise RuntimeError("aio Flame connection used from another event loop")
        if self._closed:
            return
        self._closed = True
        watches = list(self._watches)
        for watch in watches:
            watch.cancel()
        if watches:
            await asyncio.gather(*watches, return_exceptions=True)
        for result in tuple(self._results):
            if not result.done():
                result.set_exception(FlameError(FlameErrorCode.INTERNAL, "connection closed during task watch"))
        await self._channel.close()

    async def _rpc(self, method: str, request, operation: str):
        self._check_loop()
        try:
            return await getattr(self._frontend, method)(request)
        except grpc.RpcError as error:
            raise _grpc_error(error, operation) from error

    async def register_application(self, name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]) -> None:
        if isinstance(app_attrs, dict):
            app_attrs = ApplicationAttributes(**app_attrs)
        schema = None
        if app_attrs.schema is not None:
            values = {field: getattr(app_attrs.schema, field) for field in ("input", "output", "common_data") if getattr(app_attrs.schema, field) and getattr(app_attrs.schema, field).strip()}
            if values:
                schema = ApplicationSchemaProto(**values)
        spec = ApplicationSpec(
            shim=(app_attrs.shim or Shim.HOST).value,
            image=app_attrs.image,
            command=app_attrs.command,
            description=app_attrs.description,
            labels=app_attrs.labels or [],
            arguments=app_attrs.arguments or [],
            environments=[Environment(name=k, value=v) for k, v in (app_attrs.environments or {}).items()],
            working_directory=app_attrs.working_directory,
            max_instances=app_attrs.max_instances,
            delay_release=app_attrs.delay_release,
            schema=schema,
            url=app_attrs.url,
            installer=app_attrs.installer,
        )
        response = await self._rpc("RegisterApplication", RegisterApplicationRequest(name=name, application=spec), "failed to register application")
        _raise_for_result(response, "register application")

    async def unregister_application(self, name: str) -> None:
        response = await self._rpc("UnregisterApplication", UnregisterApplicationRequest(name=name), "failed to unregister application")
        _raise_for_result(response, "unregister application")

    async def list_applications(self) -> List[Application]:
        response = await self._rpc("ListApplications", ListApplicationsRequest(), "failed to list applications")
        return [_application_from_proto(app) for app in response.applications]

    async def get_application(self, name: str) -> Optional[Application]:
        try:
            response = await self._rpc("GetApplication", GetApplicationRequest(name=name), "failed to get application")
        except FlameError as error:
            if error.code == FlameErrorCode.NOT_FOUND:
                return None
            raise
        return _application_from_proto(response)

    async def list_executors(self) -> List[Any]:
        response = await self._rpc("ListExecutors", ListExecutorsRequest(), "failed to list executors")
        return list(response.executors)

    async def list_nodes(self) -> List[Any]:
        response = await self._rpc("ListNodes", ListNodesRequest(), "failed to list nodes")
        return list(response.nodes)

    async def create_session(self, attrs: SessionAttributes) -> "Session":
        response = await self._rpc("CreateSession", CreateSessionRequest(session_id=attrs.id or short_name(attrs.application), session=_session_spec(attrs)), "failed to create session")
        return _session_from_proto(self, response)

    async def list_sessions(self) -> List["Session"]:
        response = await self._rpc("ListSessions", ListSessionsRequest(), "failed to list sessions")
        return [_session_from_proto(self, item) for item in response.sessions]

    async def open_session(self, session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
        response = await self._rpc("OpenSession", OpenSessionRequest(session_id=session_id, session=_session_spec(spec) if spec is not None else None), "failed to open session")
        return _session_from_proto(self, response)

    async def get_session(self, session_id: SessionID) -> "Session":
        response = await self._rpc("GetSession", GetSessionRequest(session_id=session_id), "failed to get session")
        return _session_from_proto(self, response)

    async def close_session(self, session_id: SessionID) -> "Session":
        response = await self._rpc("CloseSession", CloseSessionRequest(session_id=session_id), "failed to close session")
        return _session_from_proto(self, response)


class Session:
    """A session handle using its connection's aio frontend channel."""

    def __init__(self, connection: Connection, id: SessionID, application: str, state: SessionState, creation_time: datetime, pending: int, running: int, succeed: int, failed: int, completion_time: Optional[datetime], common_data: Optional[bytes] = None, events: Optional[List[Event]] = None):
        self.connection = connection
        self.id = id
        self.application = application
        self.state = state
        self.creation_time = creation_time
        self.pending = pending
        self.running = running
        self.succeed = succeed
        self.failed = failed
        self.completion_time = completion_time
        self._common_data = common_data
        self.events = events or []

    def common_data(self) -> Optional[bytes]:
        return self._common_data

    async def _create_task_response(self, input_data: bytes, option: Optional[TaskOptions] = None):
        if not isinstance(input_data, bytes):
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "input_data must be bytes in core API")
        option = option or TaskOptions()
        response = await self.connection._rpc("CreateTask", CreateTaskRequest(task=TaskSpec(session_id=self.id, input=input_data, affinity=list(option.affinity))), "failed to create task")
        return response, option

    async def create_task(self, input_data: bytes, option: Optional[TaskOptions] = None) -> Task:
        response, option = await self._create_task_response(input_data, option)
        task = _task_from_proto(response, self.id)
        task.input = input_data
        if not task.affinity:
            task.affinity = set(option.affinity)
        return task

    async def get_task(self, task_id: TaskID) -> Task:
        response = await self.connection._rpc("GetTask", GetTaskRequest(task_id=task_id, session_id=self.id), "failed to get task")
        return _task_from_proto(response, self.id)

    def list_tasks(self) -> "TaskIterator":
        self.connection._check_loop()
        stream = self.connection._frontend.ListTasks(ListTasksRequest(session_id=self.id))
        return TaskIterator(stream, self.id)

    def watch_task(self, task_id: TaskID, timeout: Optional[float] = None) -> "TaskWatcher":
        self.connection._check_loop()
        stream = self.connection._frontend.WatchTask(WatchTaskRequest(task_id=task_id, session_id=self.id))
        return TaskWatcher(stream, self.id, timeout)

    async def invoke(self, input_data: bytes, option: Optional[TaskOptions] = None) -> bytes:
        return await (await self.run(input_data, option))

    async def run(self, input_data: bytes, option: Optional[TaskOptions] = None, *, _defer_watch: bool = False, _before_result=None, _discard_result_slot=None) -> asyncio.Future:
        response, _ = await self._create_task_response(input_data, option)
        task_id = response.metadata.id
        watcher = None if _defer_watch else self.watch_task(task_id)
        result = asyncio.get_running_loop().create_future()
        self.connection._results.add(result)
        result.add_done_callback(self.connection._results.discard)

        async def finish(*, output: Optional[bytes] = None, error: Optional[Exception] = None) -> None:
            if result.done():
                return
            slot_owned = False
            if _before_result is not None:
                slot_owned = await _before_result()
            if result.done():
                if slot_owned and _discard_result_slot is not None:
                    _discard_result_slot()
                return
            if error is None:
                result.set_result(output)
            else:
                result.set_exception(error)

        async def consume() -> None:
            active_watcher = watcher
            try:
                if active_watcher is None:
                    # The synchronous facade can return as soon as CreateTask
                    # succeeds. WatchTask sends a current-status snapshot, so
                    # this one-turn delay cannot miss task completion.
                    await asyncio.sleep(0)
                    active_watcher = self.watch_task(task_id)
                async for update in active_watcher:
                    if update.is_failed():
                        message = next((event.message for event in update.events or [] if event.code == TaskState.FAILED), "Task failed without error message")
                        await finish(error=FlameError(FlameErrorCode.INTERNAL, message))
                        return
                    if update.is_completed():
                        await finish(output=update.output)
                        return
                await finish(error=FlameError(FlameErrorCode.INTERNAL, "task watch closed before completion"))
            except asyncio.CancelledError:
                if not result.done():
                    result.set_exception(FlameError(FlameErrorCode.INTERNAL, "connection closed during task watch"))
                raise
            except Exception as error:
                await finish(error=error if isinstance(error, FlameError) else FlameError(FlameErrorCode.INTERNAL, f"Watch failed: {error}"))
            finally:
                if active_watcher is not None:
                    active_watcher.close()

        watch = asyncio.create_task(consume())
        self.connection._watches.add(watch)
        watch.add_done_callback(self.connection._watches.discard)

        def cancel_watch(future: asyncio.Future) -> None:
            if future.cancelled():
                watch.cancel()

        result.add_done_callback(cancel_watch)
        return result

    async def close(self) -> None:
        await self.connection.close_session(self.id)


class TaskWatcher(AsyncIterator[Task]):
    """A single-task async status stream with an optional overall timeout."""

    def __init__(self, stream, session_id: str, timeout: Optional[float] = None):
        self._stream = stream
        self._responses = stream.__aiter__()
        self._session_id = session_id
        self._loop = asyncio.get_running_loop()
        self._timeout = timeout
        self._deadline = asyncio.get_running_loop().time() + timeout if timeout is not None else None

    def __aiter__(self) -> "TaskWatcher":
        return self

    async def __anext__(self) -> Task:
        if asyncio.get_running_loop() is not self._loop:
            raise RuntimeError("aio task watch used from another event loop")
        try:
            if self._deadline is not None:
                remaining = self._deadline - asyncio.get_running_loop().time()
                if remaining <= 0:
                    raise asyncio.TimeoutError
                response = await asyncio.wait_for(self._responses.__anext__(), remaining)
            else:
                response = await self._responses.__anext__()
            return _task_from_proto(response, self._session_id)
        except asyncio.TimeoutError as error:
            self.close()
            raise TimeoutError(f"watch_task timed out after {self._timeout} seconds") from error
        except grpc.RpcError as error:
            raise _grpc_error(error, "failed to watch task") from error

    def close(self) -> None:
        if asyncio.get_running_loop() is not self._loop:
            raise RuntimeError("aio task watch used from another event loop")
        self._stream.cancel()


class TaskIterator(AsyncIterator[Task]):
    """Lazy async iterator for a session's tasks."""

    def __init__(self, stream, session_id: str):
        self._stream = stream
        self._responses = stream.__aiter__()
        self._session_id = session_id
        self._loop = asyncio.get_running_loop()

    def __aiter__(self) -> "TaskIterator":
        return self

    async def __anext__(self) -> Task:
        if asyncio.get_running_loop() is not self._loop:
            raise RuntimeError("aio task iterator used from another event loop")
        try:
            return _task_from_proto(await self._responses.__anext__(), self._session_id)
        except grpc.RpcError as error:
            raise _grpc_error(error, "failed to list tasks") from error

    def close(self) -> None:
        if asyncio.get_running_loop() is not self._loop:
            raise RuntimeError("aio task iterator used from another event loop")
        self._stream.cancel()

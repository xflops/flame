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

import threading
from typing import Optional, List, Dict, Any, Union
from urllib.parse import urlparse
import grpc
import grpc.aio
import asyncio
from datetime import datetime, timezone

from .cache import put_object
from .types import (
    Task,
    Application,
    SessionAttributes,
    ApplicationAttributes,
    Event,
    SessionID,
    TaskID,
    ApplicationID,
    TaskInput,
    TaskOutput,
    CommonData,
    SessionState,
    TaskState,
    ApplicationState,
    Shim,
    FlameError,
    FlameErrorCode,
    TaskInformer,
    Request as FlameRequest,
    Response as FlameResponse,
    FlameContext,
    ApplicationSchema,
    short_name,
)

from .types_pb2 import ApplicationSpec, SessionSpec, TaskSpec, Environment
from .frontend_pb2 import (
    RegisterApplicationRequest,
    UnregisterApplicationRequest,
    ListApplicationRequest,
    CreateSessionRequest,
    ListSessionRequest,
    GetSessionRequest,
    CloseSessionRequest,
    CreateTaskRequest,
    WatchTaskRequest,
    GetTaskRequest,
    GetApplicationRequest,
    OpenSessionRequest,
)
from .frontend_pb2_grpc import FrontendStub


async def connect(addr: str) -> "Connection":
    """Connect to the Flame service."""
    return await Connection.connect(addr)


async def create_session(application: str,
                         common_data: Dict[str, Any] = None,
                         session_id: str = None,
                         slots: int = 1) -> "Session":

    conn = await ConnectionInstance.instance()

    if common_data is None:
        pass
    elif isinstance(common_data, FlameRequest):
        common_data = common_data.to_json()
    elif not isinstance(common_data, CommonData):
        raise ValueError(
            "Invalid common data type, must be a Request or CommonData")

    session_id = short_name(application) if session_id is None else session_id

    data_expr = await put_object(session_id, common_data)
    common_data = CommonData(data_expr.to_json())

    session = await conn.create_session(
        SessionAttributes(id=session_id,
                          application=application,
                          common_data=common_data,
                          slots=slots))
    return session


async def open_session(session_id: SessionID) -> "Session":
    conn = await ConnectionInstance.instance()
    return await conn.open_session(session_id)


async def register_application(
        name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]) -> None:
    conn = await ConnectionInstance.instance()
    await conn.register_application(name, app_attrs)


async def unregister_application(name: str) -> None:
    conn = await ConnectionInstance.instance()
    await conn.unregister_application(name)


async def list_applications() -> List[Application]:
    conn = await ConnectionInstance.instance()
    return await conn.list_applications()


async def get_application(name: str) -> Application:
    conn = await ConnectionInstance.instance()
    return await conn.get_application(name)


async def list_sessions() -> List["Session"]:
    conn = await ConnectionInstance.instance()
    return await conn.list_sessions()


async def get_session(session_id: SessionID) -> "Session":
    conn = await ConnectionInstance.instance()
    return await conn.get_session(session_id)


async def close_session(session_id: SessionID) -> "Session":
    conn = await ConnectionInstance.instance()
    return await conn.close_session(session_id)


class ConnectionInstance:
    """Connection instance."""

    _lock = threading.Lock()
    _connection = None
    _context = None

    @classmethod
    async def instance(cls) -> "Connection":
        """Get the connection instance."""

        with cls._lock:
            if cls._connection is None:
                cls._context = FlameContext()
                cls._connection = await connect(cls._context._endpoint)
            return cls._connection


class Connection:
    """Connection to the Flame service."""

    def __init__(self, addr: str, channel: grpc.aio.Channel, frontend: FrontendStub):
        self.addr = addr
        self._channel = channel
        self._frontend = frontend

    @classmethod
    async def connect(cls, addr: str) -> "Connection":
        """Establish a connection to the Flame service."""
        if not addr:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "address cannot be empty")

        try:
            parsed_addr = urlparse(addr)
            host = parsed_addr.hostname or parsed_addr.path
            port = parsed_addr.port or 8080

            # Create insecure channel
            channel = grpc.aio.insecure_channel(f"{host}:{port}")

            # Wait for channel to be ready
            await channel.channel_ready()

            # Create frontend stub
            frontend = FrontendStub(channel)

            return cls(addr, channel, frontend)

        except Exception as e:
            raise FlameError(
                FlameErrorCode.INVALID_CONFIG, f"failed to connect to {addr}: {str(e)}"
            )

    async def close(self) -> None:
        """Close the connection."""
        await self._channel.close()

    async def register_application(
        self, name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]
    ) -> None:
        """Register a new application."""
        if isinstance(app_attrs, dict):
            app_attrs = ApplicationAttributes(**app_attrs)

        schema = None
        if app_attrs.schema is not None:
            schema = ApplicationSchema(
                input=app_attrs.schema.input,
                output=app_attrs.schema.output,
                common_data=app_attrs.schema.common_data,
            )

        environments = []
        if app_attrs.environments is not None:
            for k, v in app_attrs.environments.items():
                environments.append(Environment(name=k, value=v))

        app_spec = ApplicationSpec(
            shim=app_attrs.shim,
            image=app_attrs.image,
            command=app_attrs.command,
            description=app_attrs.description,
            labels=app_attrs.labels or [],
            arguments=app_attrs.arguments or [],
            environments=environments,
            working_directory=app_attrs.working_directory,
            max_instances=app_attrs.max_instances,
            delay_release=app_attrs.delay_release,
            schema=schema,
        )

        request = RegisterApplicationRequest(name=name, application=app_spec)

        try:
            await self._frontend.RegisterApplication(request)
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"failed to register application: {e.details()}",
            )

    async def unregister_application(self, name: str) -> None:
        """Unregister an application."""
        request = UnregisterApplicationRequest(name=name)

        try:
            await self._frontend.UnregisterApplication(request)
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"failed to unregister application: {e.details()}",
            )

    async def list_applications(self) -> List[Application]:
        """List all applications."""
        request = ListApplicationRequest()

        try:
            response = await self._frontend.ListApplication(request)

            applications = []
            for app in response.applications:
                schema = None
                if app.spec.schema is not None:
                    schema = ApplicationSchema(
                        input=app.spec.schema.input,
                        output=app.spec.schema.output,
                        common_data=app.spec.schema.common_data,
                    )
                environments = {}
                if app.spec.environments is not None:
                    for env in app.spec.environments:
                        environments[env.name] = env.value

                applications.append(
                    Application(
                        id=app.metadata.id,
                        name=app.metadata.name,
                        shim=Shim(app.spec.shim),
                        state=ApplicationState(app.status.state),
                        creation_time=datetime.fromtimestamp(
                            app.status.creation_time / 1000, tz=timezone.utc
                        ),
                        image=app.spec.image,
                        command=app.spec.command,
                        arguments=list(app.spec.arguments),
                        environments=environments,
                        working_directory=app.spec.working_directory,
                        max_instances=app.spec.max_instances,
                        delay_release=app.spec.delay_release,
                        schema=schema,
                    )
                )

            return applications

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to list applications: {e.details()}"
            )

    async def get_application(self, name: str) -> Application:
        """Get an application by name."""
        request = GetApplicationRequest(name=name)

        try:
            response = await self._frontend.GetApplication(request)
            schema = None
            if response.spec.schema is not None:
                schema = ApplicationSchema(
                    input=response.spec.schema.input,
                    output=response.spec.schema.output,
                    common_data=response.spec.schema.common_data,
                )

            environments = {}
            if response.spec.environments is not None:
                for env in response.spec.environments:
                    environments[env.name] = env.value

            return Application(
                id=response.metadata.id,
                name=response.metadata.name,
                shim=Shim(response.spec.shim),
                state=ApplicationState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                image=response.spec.image,
                command=response.spec.command,
                arguments=list(response.spec.arguments),
                environments=environments,
                working_directory=response.spec.working_directory,
                max_instances=response.spec.max_instances,
                delay_release=response.spec.delay_release,
                schema=schema,
            )

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to get application: {e.details()}"
            )

    async def create_session(self, attrs: SessionAttributes) -> "Session":
        """Create a new session."""
        session_spec = SessionSpec(
            application=attrs.application,
            slots=attrs.slots,
            common_data=attrs.common_data,
        )

        request = CreateSessionRequest(session_id=attrs.id, session=session_spec)

        try:
            response = await self._frontend.CreateSession(request)

            session = Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
            )
            return session
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to create session: {e.details()}"
            )

    async def list_sessions(self) -> List["Session"]:
        """List all sessions."""
        request = ListSessionRequest()

        try:
            response = await self._frontend.ListSession(request)

            sessions = []
            for session in response.sessions:
                sessions.append(
                    Session(
                        connection=self,
                        id=session.metadata.id,
                        application=session.spec.application,
                        slots=session.spec.slots,
                        state=SessionState(session.status.state),
                        creation_time=datetime.fromtimestamp(
                            session.status.creation_time / 1000, tz=timezone.utc
                        ),
                        pending=session.status.pending,
                        running=session.status.running,
                        succeed=session.status.succeed,
                        failed=session.status.failed,
                        completion_time=(
                            datetime.fromtimestamp(
                                session.status.completion_time / 1000, tz=timezone.utc
                            )
                            if session.status.HasField("completion_time")
                            else None
                        ),
                    )
                )

            return sessions

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to list sessions: {e.details()}"
            )

    async def open_session(self, session_id: SessionID) -> "Session":
        """Open a session."""
        request = OpenSessionRequest(session_id=session_id)

        try:
            response = await self._frontend.OpenSession(request)
            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
            )

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to open session: {e.details()}"
            )

    async def get_session(self, session_id: SessionID) -> "Session":
        """Get a session by ID."""
        request = GetSessionRequest(session_id=session_id)

        try:
            response = await self._frontend.GetSession(request)

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
            )

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to get session: {e.details()}"
            )

    async def close_session(self, session_id: SessionID) -> "Session":
        """Close a session."""
        request = CloseSessionRequest(session_id=session_id)

        try:
            response = await self._frontend.CloseSession(request)

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
            )

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to close session: {e.details()}"
            )


class Session:
    connection: Connection

    """Represents a computing session."""
    id: SessionID
    application: str
    slots: int
    state: SessionState
    creation_time: datetime
    pending: int = 0
    running: int = 0
    succeed: int = 0
    failed: int = 0
    completion_time: Optional[datetime] = None

    """Client for session-specific operations."""

    def __init__(
        self,
        connection: Connection,
        id: SessionID,
        application: str,
        slots: int,
        state: SessionState,
        creation_time: datetime,
        pending: int,
        running: int,
        succeed: int,
        failed: int,
        completion_time: Optional[datetime],
    ):
        self.connection = connection
        self.id = id
        self.application = application
        self.slots = slots
        self.state = state
        self.creation_time = creation_time
        self.pending = pending
        self.running = running
        self.succeed = succeed
        self.failed = failed
        self.completion_time = completion_time
        self.mutex = threading.Lock()

    async def create_task(self, input_data: TaskInput) -> Task:
        """Create a new task in the session."""
        task_spec = TaskSpec(session_id=self.id, input=input_data)

        request = CreateTaskRequest(task=task_spec)

        try:
            response = await self.connection._frontend.CreateTask(request)

            return Task(
                id=response.metadata.id,
                session_id=self.id,
                state=TaskState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                input=input_data,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
                events=[
                    Event(
                        code=event.code,
                        message=event.message,
                        creation_time=datetime.fromtimestamp(
                            event.creation_time / 1000, tz=timezone.utc
                        ),
                    )
                    for event in response.status.events
                ],
            )

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to create task: {e.details()}"
            )

    async def get_task(self, task_id: TaskID) -> Task:
        """Get a task by ID."""
        request = GetTaskRequest(task_id=task_id, session_id=self.id)

        try:
            response = await self.connection._frontend.GetTask(request)

            return Task(
                id=response.metadata.id,
                session_id=self.id,
                state=TaskState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                input=response.spec.input,
                output=response.spec.output,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
                events=[
                    Event(
                        code=event.code,
                        message=event.message,
                        creation_time=datetime.fromtimestamp(
                            event.creation_time / 1000, tz=timezone.utc
                        ),
                    )
                    for event in response.status.events
                ],
            )

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to get task: {e.details()}"
            )

    async def watch_task(self, task_id: TaskID) -> "TaskWatcher":
        """Watch a task for updates."""
        request = WatchTaskRequest(task_id=task_id, session_id=self.id)

        try:
            stream = self.connection._frontend.WatchTask(request)
            return TaskWatcher(stream)

        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL, f"failed to watch task: {e.details()}"
            )

    async def invoke(
        self, input_data, informer: Optional[TaskInformer] = None
    ) -> TaskOutput:
        """Invoke a task with the given input and optional informer."""
        if input_data is None:
            pass
        if isinstance(input_data, FlameRequest):
            input_data = input_data.to_json()
        elif not isinstance(input_data, TaskInput):
            raise ValueError("Invalid input data type, must be a Request or TaskInput")

        task = await self.create_task(input_data)
        watcher = await self.watch_task(task.id)

        async for task in watcher:
            # If informer is provided, use it to update the task and
            # return None to indicate that the task is handled by the informer.
            if informer is not None:
                with self.mutex:
                    informer.on_update(task)
                if task.is_completed():
                    return None

            # If the task is failed, raise an error.
            if task.is_failed():
                for event in task.events:
                    if event.code == TaskState.FAILED:
                        raise FlameError(FlameErrorCode.INTERNAL, f"{event.message}")
            # If the task is completed, return the output.
            elif task.is_completed():
                return task.output

    async def close(self) -> None:
        """Close the session."""
        await self.connection.close_session(self.id)


class TaskWatcher:
    """Async iterator for watching task updates."""

    def __init__(self, stream):
        self._stream = stream

    def __aiter__(self):
        return self

    async def __anext__(self) -> Task:
        try:
            response = await self._stream.read()

            return Task(
                id=response.metadata.id,
                session_id=response.spec.session_id,
                state=TaskState(response.status.state),
                creation_time=datetime.fromtimestamp(
                    response.status.creation_time / 1000, tz=timezone.utc
                ),
                input=response.spec.input,
                output=response.spec.output,
                completion_time=(
                    datetime.fromtimestamp(
                        response.status.completion_time / 1000, tz=timezone.utc
                    )
                    if response.status.HasField("completion_time")
                    else None
                ),
                events=[
                    Event(
                        code=event.code,
                        message=event.message,
                        creation_time=datetime.fromtimestamp(
                            event.creation_time / 1000, tz=timezone.utc
                        ),
                    )
                    for event in response.status.events
                ],
            )

        except StopAsyncIteration:
            raise
        except Exception as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to watch task: {str(e)}")

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

import logging
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Union
from urllib.parse import urlparse

import grpc

from typing import Callable

from flamepy.core.types import (
    Application,
    ApplicationAttributes,
    ApplicationSchema,
    ApplicationState,
    Event,
    FlameClientTls,
    FlameContext,
    FlameError,
    FlameErrorCode,
    SessionAttributes,
    SessionID,
    SessionState,
    Shim,
    Task,
    TaskID,
    TaskInformer,
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
    ListApplicationRequest,
    ListSessionRequest,
    ListTaskRequest,
    OpenSessionRequest,
    RegisterApplicationRequest,
    UnregisterApplicationRequest,
    WatchTaskRequest,
)
from flamepy.proto.frontend_pb2_grpc import FrontendStub
from flamepy.proto.types_pb2 import ApplicationSchema as ApplicationSchemaProto
from flamepy.proto.types_pb2 import ApplicationSpec, Environment, SessionSpec, TaskSpec

logger = logging.getLogger(__name__)


def connect(addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
    """Connect to the Flame service.

    Args:
        addr: The endpoint URL (use https:// for TLS, http:// for plaintext)
        tls_config: Optional TLS configuration for secure connections
    """
    return Connection.connect(addr, tls_config)


def create_session(application: str, common_data: Optional[bytes] = None, session_id: Optional[str] = None, slots: int = 1, min_instances: int = 0, max_instances: Optional[int] = None, batch_size: int = 1) -> "Session":
    """Create a new session.

    Args:
        application: Application name
        common_data: Common data as bytes (core API works with bytes)
        session_id: Optional session ID
        slots: Number of slots
        min_instances: Minimum number of instances (default: 0)
        max_instances: Maximum number of instances (None = unlimited)
        batch_size: Number of executors per batch for gang scheduling (default: 1)
    """
    conn = ConnectionInstance.instance()
    return conn.create_session(SessionAttributes(id=session_id, application=application, common_data=common_data, slots=slots, min_instances=min_instances, max_instances=max_instances, batch_size=batch_size))


def open_session(session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
    """Open an existing session or create a new one if spec is provided.

    Args:
        session_id: The session ID to open or create.
        spec: Optional session specification. If provided and session doesn't
              exist, a new session will be created with this spec. If session
              exists, the spec will be validated against the existing session.

    Returns:
        The opened or newly created Session object.

    Raises:
        FlameError(NOT_FOUND): If session doesn't exist and no spec provided.
        FlameError(INVALID_STATE): If session exists but is not in Open state.
        FlameError(INVALID_ARGUMENT): If session exists but spec doesn't match.
    """
    conn = ConnectionInstance.instance()
    return conn.open_session(session_id, spec)


def register_application(name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]) -> None:
    conn = ConnectionInstance.instance()
    conn.register_application(name, app_attrs)


def unregister_application(name: str) -> None:
    conn = ConnectionInstance.instance()
    conn.unregister_application(name)


def list_applications() -> List[Application]:
    conn = ConnectionInstance.instance()
    return conn.list_applications()


def get_application(name: str) -> Optional[Application]:
    conn = ConnectionInstance.instance()
    return conn.get_application(name)


def list_sessions() -> List["Session"]:
    conn = ConnectionInstance.instance()
    return conn.list_sessions()


def get_session(session_id: SessionID) -> "Session":
    conn = ConnectionInstance.instance()
    return conn.get_session(session_id)


def close_session(session_id: SessionID) -> "Session":
    conn = ConnectionInstance.instance()
    return conn.close_session(session_id)


class ConnectionInstance:
    """Connection instance."""

    _lock = threading.Lock()
    _connection = None
    _context = None

    @classmethod
    def instance(cls) -> "Connection":
        """Get the connection instance."""
        with cls._lock:
            if cls._connection is None:
                cls._context = FlameContext()
                cls._connection = connect(cls._context._endpoint, cls._context.tls)
            return cls._connection


class Connection:
    """Connection to the Flame service."""

    def __init__(self, addr: str, channel: grpc.Channel, frontend: FrontendStub):
        self.addr = addr
        self._channel = channel
        self._frontend = frontend
        self._executor = ThreadPoolExecutor(max_workers=10)

    @classmethod
    def connect(cls, addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
        """Establish a connection to the Flame service.

        Args:
            addr: The endpoint URL (use https:// for TLS, http:// for plaintext)
            tls_config: Optional TLS configuration for secure connections

        TLS Behavior:
            - If addr starts with https:// and tls_config is provided, use provided TLS config
            - If addr starts with https:// and tls_config is None, use default TLS config (system CA)
            - If addr starts with http://, TLS is not used regardless of tls_config
        """
        if not addr:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, "address cannot be empty")

        try:
            parsed_addr = urlparse(addr)
            scheme = parsed_addr.scheme or "http"
            host = parsed_addr.hostname or parsed_addr.path
            port = parsed_addr.port or 8080

            # Determine if TLS should be used
            use_tls = scheme == "https"

            if use_tls:
                # Create secure channel with TLS
                if tls_config is not None and tls_config.ca_file:
                    # Use custom CA certificate
                    with open(tls_config.ca_file, "rb") as f:
                        root_certs = f.read()
                    credentials = grpc.ssl_channel_credentials(root_certificates=root_certs)
                    logger.debug("TLS enabled with custom CA certificate: %s", tls_config.ca_file)
                else:
                    # Use system CA bundle (default)
                    credentials = grpc.ssl_channel_credentials()
                    logger.debug("TLS enabled with system CA bundle")

                channel = grpc.secure_channel(f"{host}:{port}", credentials)
            else:
                # Create insecure channel
                channel = grpc.insecure_channel(f"{host}:{port}")

            # Wait for channel to be ready (with timeout)
            try:
                grpc.channel_ready_future(channel).result(timeout=10)
            except grpc.FutureTimeoutError:
                raise FlameError(FlameErrorCode.INVALID_CONFIG, f"timeout connecting to {addr}")

            # Create frontend stub
            frontend = FrontendStub(channel)

            return cls(addr, channel, frontend)

        except FlameError:
            raise
        except Exception as e:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, f"failed to connect to {addr}: {str(e)}")

    def close(self) -> None:
        """Close the connection."""
        self._executor.shutdown(wait=True)
        self._channel.close()

    def register_application(self, name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]) -> None:
        """Register a new application."""
        if isinstance(app_attrs, dict):
            app_attrs = ApplicationAttributes(**app_attrs)

        schema = None
        if app_attrs.schema is not None:
            has_input = app_attrs.schema.input and app_attrs.schema.input.strip()
            has_output = app_attrs.schema.output and app_attrs.schema.output.strip()
            has_common_data = app_attrs.schema.common_data and app_attrs.schema.common_data.strip()

            if has_input or has_output or has_common_data:
                schema = ApplicationSchemaProto(
                    input=app_attrs.schema.input if has_input else None,
                    output=app_attrs.schema.output if has_output else None,
                    common_data=app_attrs.schema.common_data if has_common_data else None,
                )

        environments = []
        if app_attrs.environments is not None:
            for k, v in app_attrs.environments.items():
                environments.append(Environment(name=k, value=v))

        shim_value = app_attrs.shim.value if app_attrs.shim is not None else Shim.HOST.value

        app_spec = ApplicationSpec(
            shim=shim_value,
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
            url=app_attrs.url,
        )

        request = RegisterApplicationRequest(name=name, application=app_spec)

        try:
            self._frontend.RegisterApplication(request)
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"failed to register application: {e.details()}",
            )

    def unregister_application(self, name: str) -> None:
        """Unregister an application."""
        request = UnregisterApplicationRequest(name=name)

        try:
            self._frontend.UnregisterApplication(request)
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"failed to unregister application: {e.details()}",
            )

    def list_applications(self) -> List[Application]:
        """List all applications."""
        request = ListApplicationRequest()

        try:
            response = self._frontend.ListApplication(request)

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
                        state=ApplicationState(app.status.state),
                        creation_time=datetime.fromtimestamp(app.status.creation_time / 1000, tz=timezone.utc),
                        shim=Shim(app.spec.shim),
                        image=app.spec.image,
                        command=app.spec.command,
                        arguments=list(app.spec.arguments),
                        environments=environments,
                        working_directory=app.spec.working_directory,
                        max_instances=app.spec.max_instances,
                        delay_release=app.spec.delay_release,
                        schema=schema,
                        url=app.spec.url if app.spec.HasField("url") else None,
                    )
                )

            return applications

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list applications: {e.details()}")

    def get_application(self, name: str) -> Optional[Application]:
        """Get an application by name. Returns None if not found."""
        request = GetApplicationRequest(name=name)

        try:
            response = self._frontend.GetApplication(request)
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
                state=ApplicationState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                shim=Shim(response.spec.shim),
                image=response.spec.image,
                command=response.spec.command,
                arguments=list(response.spec.arguments),
                environments=environments,
                working_directory=response.spec.working_directory,
                max_instances=response.spec.max_instances,
                delay_release=response.spec.delay_release,
                schema=schema,
                url=response.spec.url if response.spec.HasField("url") else None,
            )

        except grpc.RpcError as e:
            if e.code() == grpc.StatusCode.NOT_FOUND:
                return None
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to get application: {e.details()}")

    def create_session(self, attrs: SessionAttributes) -> "Session":
        """Create a new session."""

        session_id = short_name(attrs.application) if attrs.id is None else attrs.id

        # Common data should be bytes in core API
        common_data_bytes = attrs.common_data if isinstance(attrs.common_data, bytes) else None
        if common_data_bytes is None and attrs.common_data is not None:
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "common_data must be bytes in core API")

        session_spec = SessionSpec(
            application=attrs.application,
            slots=attrs.slots,
            common_data=common_data_bytes,
            min_instances=attrs.min_instances,
            max_instances=attrs.max_instances if attrs.max_instances is not None else None,
            batch_size=attrs.batch_size,
        )

        request = CreateSessionRequest(session_id=session_id, session=session_spec)

        try:
            response = self._frontend.CreateSession(request)
            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") and response.spec.common_data else None

            session = Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
            )
            return session
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to create session: {e.details()}")

    def list_sessions(self) -> List["Session"]:
        """List all sessions."""
        request = ListSessionRequest()

        try:
            response = self._frontend.ListSession(request)

            sessions = []
            for session in response.sessions:
                # Common data is bytes in core API
                common_data_bytes = session.spec.common_data if session.spec.HasField("common_data") and session.spec.common_data else None

                sessions.append(
                    Session(
                        connection=self,
                        id=session.metadata.id,
                        application=session.spec.application,
                        slots=session.spec.slots,
                        state=SessionState(session.status.state),
                        creation_time=datetime.fromtimestamp(session.status.creation_time / 1000, tz=timezone.utc),
                        pending=session.status.pending,
                        running=session.status.running,
                        succeed=session.status.succeed,
                        failed=session.status.failed,
                        completion_time=(datetime.fromtimestamp(session.status.completion_time / 1000, tz=timezone.utc) if session.status.HasField("completion_time") else None),
                        common_data=common_data_bytes,
                    )
                )

            return sessions

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list sessions: {e.details()}")

    def open_session(self, session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
        """Open an existing session or create a new one if spec is provided.

        Args:
            session_id: The session ID to open or create.
            spec: Optional session specification for creation/validation.

        Returns:
            The opened or newly created Session object.
        """
        # Build SessionSpec protobuf if spec is provided
        session_spec = None
        if spec is not None:
            session_spec = SessionSpec(
                application=spec.application,
                slots=spec.slots,
                common_data=spec.common_data,
                min_instances=spec.min_instances,
                max_instances=spec.max_instances,
                batch_size=spec.batch_size,
            )

        request = OpenSessionRequest(session_id=session_id, session=session_spec)

        try:
            response = self._frontend.OpenSession(request)
            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") and response.spec.common_data else None

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to open session: {e.details()}")

    def get_session(self, session_id: SessionID) -> "Session":
        """Get a session by ID."""
        request = GetSessionRequest(session_id=session_id)

        try:
            response = self._frontend.GetSession(request)

            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") and response.spec.common_data else None

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to get session: {e.details()}")

    def close_session(self, session_id: SessionID) -> "Session":
        """Close a session."""
        request = CloseSessionRequest(session_id=session_id)

        try:
            response = self._frontend.CloseSession(request)

            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") and response.spec.common_data else None

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                slots=response.spec.slots,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to close session: {e.details()}")


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
    _common_data: Optional[bytes] = None
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
        common_data: Optional[bytes] = None,
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
        self._common_data = common_data

    def common_data(self) -> Optional[bytes]:
        """Get the common data of Session as bytes."""
        return self._common_data

    def create_task(self, input_data: bytes) -> Task:
        """Create a new task in the session.

        Args:
            input_data: Task input as bytes (core API works with bytes)
        """
        # Input data should be bytes in core API
        if not isinstance(input_data, bytes):
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "input_data must be bytes in core API")

        task_spec = TaskSpec(session_id=self.id, input=input_data)

        request = CreateTaskRequest(task=task_spec)

        try:
            response = self.connection._frontend.CreateTask(request)

            return Task(
                id=response.metadata.id,
                session_id=self.id,
                state=TaskState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                input=input_data,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                events=[
                    Event(
                        code=event.code,
                        message=event.message,
                        creation_time=datetime.fromtimestamp(event.creation_time / 1000, tz=timezone.utc),
                    )
                    for event in response.status.events
                ],
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to create task: {e.details()}")

    def get_task(self, task_id: TaskID) -> Task:
        """Get a task by ID."""
        request = GetTaskRequest(task_id=task_id, session_id=self.id)

        try:
            response = self.connection._frontend.GetTask(request)

            return Task(
                id=response.metadata.id,
                session_id=self.id,
                state=TaskState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                input=response.spec.input if response.spec.HasField("input") and response.spec.input else None,
                output=response.spec.output if response.spec.HasField("output") and response.spec.output else None,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                events=[
                    Event(
                        code=event.code,
                        message=event.message,
                        creation_time=datetime.fromtimestamp(event.creation_time / 1000, tz=timezone.utc),
                    )
                    for event in response.status.events
                ],
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to get task: {e.details()}")

    def list_tasks(self) -> "TaskIterator":
        """List all tasks in the session.

        Returns:
            An iterator of Task objects in this session. Tasks are streamed
            lazily to save memory when dealing with large numbers of tasks.

        Example:
            >>> for task in session.list_tasks():
            ...     print(f"Task {task.id}: {task.state}")
        """
        request = ListTaskRequest(session_id=self.id)

        try:
            task_stream = self.connection._frontend.ListTask(request)
            return TaskIterator(task_stream, self.id)

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list tasks: {e.details()}")

    def watch_task(self, task_id: TaskID, timeout: Optional[float] = None) -> "TaskWatcher":
        """Watch a task for updates.

        Args:
            task_id: The ID of the task to watch
            timeout: Optional timeout in seconds. If specified, iteration will raise
                     TimeoutError if no update is received within the timeout period.

        Returns:
            A TaskWatcher iterator that yields Task updates
        """
        request = WatchTaskRequest(task_id=task_id, session_id=self.id)

        try:
            stream = self.connection._frontend.WatchTask(request)
            return TaskWatcher(stream, timeout=timeout)

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to watch task: {e.details()}")

    def invoke(self, input_data: Any) -> Any:
        """Invoke a task with the given input (synchronous).

        This method blocks until the task completes or fails.

        Args:
            input_data: The input data for the task

        Returns:
            The task output

        Example:
            >>> result = session.invoke(b"input data")
            >>> print(result)
        """
        return self.run(input_data).result()

    def run(self, input_data: Any) -> Future:
        """Run a task asynchronously and return a Future.

        This method returns immediately after task creation.

        Args:
            input_data: The input data for the task

        Returns:
            A Future object that will contain the result when the task completes

        Example (single task):
            >>> future = session.run(b"input data")
            >>> result = future.result()  # Wait for completion

        Example (parallel execution):
            >>> from concurrent.futures import wait
            >>> futures = [session.run(f"input {i}".encode()) for i in range(10)]
            >>> wait(futures)
            >>> results = [f.result() for f in futures]
        """
        task = self.create_task(input_data)
        future = _LazyTaskFuture(self, task.id)
        future_informer = _FutureTaskInformer(future)

        def _watch():
            try:
                watcher = self.watch_task(task.id)
                for t in watcher:
                    future_informer.on_update(t)
                    if t.is_completed() or t.is_failed():
                        return
            except Exception as e:
                if isinstance(e, FlameError):
                    future_informer.on_error(e)
                else:
                    future_informer.on_error(FlameError(FlameErrorCode.INTERNAL, f"Watch failed: {str(e)}"))

        self.connection._executor.submit(_watch)
        return future

    def close(self) -> None:
        """Close the session."""
        self.connection.close_session(self.id)


def _task_from_proto(response, session_id: str) -> Task:
    """Convert a protobuf Task response to a Task object."""
    return Task(
        id=response.metadata.id,
        session_id=session_id,
        state=TaskState(response.status.state),
        creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
        input=response.spec.input if response.spec.HasField("input") and response.spec.input else None,
        output=response.spec.output if response.spec.HasField("output") and response.spec.output else None,
        completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
        events=[
            Event(
                code=event.code,
                message=event.message,
                creation_time=datetime.fromtimestamp(event.creation_time / 1000, tz=timezone.utc),
            )
            for event in response.status.events
        ],
    )


class TaskWatcher:
    """Iterator for watching task updates."""

    def __init__(self, stream, timeout: Optional[float] = None):
        self._stream = stream
        self._timeout = timeout
        self._deadline = None
        if timeout is not None:
            self._deadline = time.monotonic() + timeout

    def __iter__(self):
        return self

    def __next__(self) -> Task:
        if self._deadline is not None and time.monotonic() >= self._deadline:
            raise TimeoutError(f"watch_task timed out after {self._timeout} seconds")

        try:
            response = next(self._stream)
            return _task_from_proto(response, response.spec.session_id)

        except StopIteration:
            raise
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to watch task: {e.details()}")
        except Exception as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to watch task: {str(e)}")


class TaskIterator:
    """Iterator for listing tasks in a session."""

    def __init__(self, stream, session_id: str):
        self._stream = stream
        self._session_id = session_id

    def __iter__(self):
        return self

    def __next__(self) -> Task:
        try:
            response = next(self._stream)
            return _task_from_proto(response, self._session_id)

        except StopIteration:
            raise
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list tasks: {e.details()}")
        except Exception as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list tasks: {str(e)}")


class _LazyTaskFuture(Future):
    """A Future that tracks a Flame task, compatible with concurrent.futures.wait()/as_completed()."""

    def __init__(
        self,
        session: "Session",
        task_id: TaskID,
    ):
        super().__init__()
        self._session = session
        self._task_id = task_id


class _FutureTaskInformer(TaskInformer):
    """TaskInformer that updates a _LazyTaskFuture when task state changes."""

    def __init__(self, future: Future):
        self._future = future

    def on_update(self, task: Task) -> None:
        """Called when task status changes."""
        if self._future.done():
            return
        if task.is_failed():
            for event in task.events:
                if event.code == TaskState.FAILED:
                    self._future.set_exception(FlameError(FlameErrorCode.INTERNAL, f"{event.message}"))
                    return
            self._future.set_exception(FlameError(FlameErrorCode.INTERNAL, "Task failed without error message"))
        elif task.is_completed():
            self._future.set_result(task.output)

    def on_error(self, error: FlameError) -> None:
        """Called when watch stream encounters an error."""
        if not self._future.done():
            self._future.set_exception(error)

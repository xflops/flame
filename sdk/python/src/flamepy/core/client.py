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

import asyncio
import logging
import threading
import time
from collections import deque
from concurrent.futures import Future, ThreadPoolExecutor
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Union
from urllib.parse import urlparse

import grpc

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
    ResourceRequirement,
    SessionAttributes,
    SessionID,
    SessionState,
    Shim,
    Task,
    TaskID,
    TaskInformer,
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
from flamepy.proto.types_pb2 import ApplicationSchema as ApplicationSchemaProto
from flamepy.proto.types_pb2 import ApplicationSpec, Environment, SessionSpec, TaskSpec
from flamepy.proto.types_pb2 import ResourceRequirement as ResourceRequirementProto

logger = logging.getLogger(__name__)

# A watch completes its Future on the gRPC event loop. Future callbacks can run
# arbitrary user code, so dispatch them away from that loop. The pool is shared
# across connections so closing a connection never waits for a user callback and
# callbacks added to an already completed Future still have a live dispatcher.
_task_callback_executor = ThreadPoolExecutor(max_workers=8, thread_name_prefix="flamepy-task-callback")
_internal_callback_executor = ThreadPoolExecutor(max_workers=4, thread_name_prefix="flamepy-internal-callback")


def _optional_field(message: Any, field: str) -> Any:
    return getattr(message, field) if message.HasField(field) else None


def _schema_from_application_spec(spec: ApplicationSpec) -> Optional[ApplicationSchema]:
    if not spec.HasField("schema"):
        return None
    return ApplicationSchema(
        input=_optional_field(spec.schema, "input"),
        output=_optional_field(spec.schema, "output"),
        common_data=_optional_field(spec.schema, "common_data"),
    )


def _events_from_proto(events) -> List[Event]:
    return [
        Event(
            code=event.code,
            message=event.message,
            creation_time=datetime.fromtimestamp(event.creation_time / 1000, tz=timezone.utc),
        )
        for event in events
    ]


def _application_from_proto(app) -> Application:
    spec = app.spec
    environments = {env.name: env.value for env in spec.environments}
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
        environments=environments,
        working_directory=_optional_field(spec, "working_directory"),
        max_instances=_optional_field(spec, "max_instances"),
        delay_release=_optional_field(spec, "delay_release"),
        schema=_schema_from_application_spec(spec),
        url=_optional_field(spec, "url"),
        installer=_optional_field(spec, "installer"),
    )


def _raise_for_result(response, operation: str) -> None:
    if response.return_code != 0:
        message = response.message or f"{operation} failed with return_code {response.return_code}"
        raise FlameError(FlameErrorCode.INTERNAL, message)


def connect(addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
    """Connect to the Flame service.

    Args:
        addr: The endpoint URL (use https:// for TLS, http:// for plaintext)
        tls_config: Optional TLS configuration for secure connections
    """
    return Connection.connect(addr, tls_config)


def create_session(
    application: str,
    common_data: Optional[bytes] = None,
    session_id: Optional[str] = None,
    min_instances: int = 0,
    max_instances: Optional[int] = None,
    batch_size: int = 1,
    resreq: Optional[ResourceRequirement] = None,
) -> "Session":
    """Create a new session.

    Args:
        application: Application name
        common_data: Common data as bytes (core API works with bytes)
        session_id: Optional session ID
        min_instances: Minimum number of instances (default: 0)
        max_instances: Maximum number of instances (None = unlimited)
        batch_size: Reserved for future implementation; sessions currently use 1.
        resreq: Optional explicit resource requirements. When omitted, the
                server applies cluster.resource_requirement (or a hardcoded
                fallback when that is unset).
    """
    conn = ConnectionInstance.instance()
    return conn.create_session(
        SessionAttributes(
            id=session_id,
            application=application,
            common_data=common_data,
            min_instances=min_instances,
            max_instances=max_instances,
            batch_size=1,
            resreq=resreq,
        )
    )


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


def list_executors() -> List[Any]:
    conn = ConnectionInstance.instance()
    return conn.list_executors()


def list_nodes() -> List[Any]:
    conn = ConnectionInstance.instance()
    return conn.list_nodes()


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


class _SessionTaskWatch:
    """One stream and its outstanding task futures for a session."""

    def __init__(self, session_id: SessionID):
        self.session_id = session_id
        self.pending: Dict[TaskID, "_FutureTaskInformer"] = {}
        self.requests: Optional[asyncio.Queue] = None
        self.call: Optional[Future] = None
        self.closing = False


class Connection:
    """Connection to the Flame service."""

    def __init__(self, addr: str, channel: grpc.Channel, frontend: FrontendStub, credentials: Optional[grpc.ChannelCredentials] = None):
        self.addr = addr
        self._channel = channel
        self._frontend = frontend
        self._watch_credentials = credentials
        self._watch_target = None
        self._watch_lock = threading.Lock()
        self._watch_loop = None
        self._watch_thread = None
        self._watch_channel = None
        self._watch_frontend = None
        self._session_watches: Dict[SessionID, _SessionTaskWatch] = {}
        self._task_submissions: Dict[SessionID, int] = {}
        self._closed = False

    @staticmethod
    def _grpc_error_to_flame_error(e: grpc.RpcError, operation: str) -> FlameError:
        code = e.code()
        details = e.details() or ""
        if code == grpc.StatusCode.NOT_FOUND:
            return FlameError(FlameErrorCode.NOT_FOUND, f"{operation}: {details}")
        elif code == grpc.StatusCode.ALREADY_EXISTS:
            return FlameError(FlameErrorCode.ALREADY_EXISTS, f"{operation}: {details}")
        elif code == grpc.StatusCode.INVALID_ARGUMENT:
            return FlameError(FlameErrorCode.INVALID_ARGUMENT, f"{operation}: {details}")
        elif code == grpc.StatusCode.FAILED_PRECONDITION:
            return FlameError(FlameErrorCode.INVALID_STATE, f"{operation}: {details}")
        else:
            return FlameError(FlameErrorCode.INTERNAL, f"{operation}: {details}")

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

            connection = cls(addr, channel, frontend, credentials if use_tls else None)
            connection._watch_target = f"{host}:{port}"
            return connection

        except FlameError:
            raise
        except Exception as e:
            raise FlameError(FlameErrorCode.INVALID_CONFIG, f"failed to connect to {addr}: {str(e)}")

    def close(self) -> None:
        """Close the connection."""
        with self._watch_lock:
            if self._closed:
                return
            self._closed = True
            loop = self._watch_loop
            thread = self._watch_thread
            watches = tuple((watch, tuple(watch.pending.values())) for watch in self._session_watches.values())
        for watch, pending in watches:
            for informer in pending:
                informer.on_error(FlameError(FlameErrorCode.INTERNAL, "connection closed before task completed"))
            if watch.call is not None:
                watch.call.cancel()
        if loop is not None:

            async def shutdown():
                pending = [task for task in asyncio.all_tasks() if task is not asyncio.current_task()]
                for task in pending:
                    task.cancel()
                if pending:
                    await asyncio.gather(*pending, return_exceptions=True)
                if self._watch_channel is not None:
                    await self._watch_channel.close()

            shutdown_call = asyncio.run_coroutine_threadsafe(shutdown(), loop)
            if threading.current_thread() is not thread:
                shutdown_call.result()
                loop.call_soon_threadsafe(loop.stop)
                thread.join()
            else:
                shutdown_call.add_done_callback(lambda _: loop.call_soon_threadsafe(loop.stop))
        self._channel.close()

    def _begin_task_submission(self, session_id: SessionID) -> None:
        with self._watch_lock:
            if self._closed:
                raise FlameError(FlameErrorCode.INVALID_STATE, "connection is closed")
            self._task_submissions[session_id] = self._task_submissions.get(session_id, 0) + 1

    def _finish_task_submission(self, session_id: SessionID) -> None:
        with self._watch_lock:
            remaining = self._task_submissions[session_id] - 1
            if remaining:
                self._task_submissions[session_id] = remaining
            else:
                self._task_submissions.pop(session_id)
            watch = self._session_watches.get(session_id)
            stop_watch = watch is not None and watch.closing and not watch.pending and not remaining
            if stop_watch and watch is not None and watch.call is None:
                self._session_watches.pop(session_id)
        if stop_watch and watch is not None and watch.call is not None:
            watch.call.cancel()

    def _schedule_task_watch(self, session_id: SessionID, task_id: TaskID, informer: "_FutureTaskInformer") -> None:
        with self._watch_lock:
            if self._closed:
                raise FlameError(FlameErrorCode.INVALID_STATE, "connection is closed")
            if self._watch_loop is None:
                self._watch_loop = asyncio.new_event_loop()
                self._watch_thread = threading.Thread(target=self._run_watch_loop, name="flamepy-task-watches", daemon=True)
                self._watch_thread.start()
            watch = self._session_watches.get(session_id)
            if watch is None:
                watch = _SessionTaskWatch(session_id)
                self._session_watches[session_id] = watch
            if watch.call is None:
                watch.call = asyncio.run_coroutine_threadsafe(self._watch_session_tasks(watch), self._watch_loop)
            watch.pending[task_id] = informer
            if watch.requests is not None:
                self._watch_loop.call_soon_threadsafe(watch.requests.put_nowait, WatchTaskRequest(task_id=task_id, session_id=session_id))

    def _close_task_watch(self, session_id: SessionID) -> None:
        with self._watch_lock:
            watch = self._session_watches.get(session_id)
            if watch is None and self._task_submissions.get(session_id):
                watch = _SessionTaskWatch(session_id)
                self._session_watches[session_id] = watch
            if watch is not None:
                watch.closing = True
                cancel = not watch.pending and not self._task_submissions.get(session_id)
                if cancel and watch.call is None:
                    self._session_watches.pop(session_id)
            else:
                cancel = False
        if watch is not None and cancel and watch.call is not None:
            watch.call.cancel()

    def _run_watch_loop(self) -> None:
        loop = self._watch_loop
        asyncio.set_event_loop(loop)
        loop.run_forever()
        loop.close()

    async def _watch_session_tasks(self, watch: _SessionTaskWatch) -> None:
        session_id = watch.session_id
        error = FlameError(FlameErrorCode.INTERNAL, f"watch stream ended before session {session_id} tasks completed")
        try:
            if self._watch_frontend is None:
                target = self._watch_target or urlparse(self.addr).netloc
                if self._watch_credentials is None:
                    self._watch_channel = grpc.aio.insecure_channel(target)
                else:
                    self._watch_channel = grpc.aio.secure_channel(target, self._watch_credentials)
                self._watch_frontend = FrontendStub(self._watch_channel)

            requests = asyncio.Queue()
            with self._watch_lock:
                watch.requests = requests
                initial_ids = tuple(watch.pending)
            for task_id in initial_ids:
                requests.put_nowait(WatchTaskRequest(task_id=task_id, session_id=session_id))

            async def registrations():
                while True:
                    yield await requests.get()

            stream = self._watch_frontend.WatchTasks(registrations())
            async for response in stream:
                task = _task_from_proto(response, session_id)
                if task.is_completed() or task.is_failed():
                    with self._watch_lock:
                        informer = watch.pending.pop(task.id, None)
                        close_watch = watch.closing and not watch.pending and not self._task_submissions.get(session_id)
                    if informer is not None:
                        informer.on_update(task)
                    if close_watch:
                        if hasattr(stream, "cancel"):
                            stream.cancel()
                        return
        except asyncio.CancelledError:
            error = FlameError(FlameErrorCode.INTERNAL, f"watch for session {session_id} was cancelled")
            raise
        except Exception as e:
            if isinstance(e, FlameError):
                error = e
            elif isinstance(e, grpc.RpcError):
                error = FlameError(FlameErrorCode.INTERNAL, f"failed to watch session tasks: {e.details()}")
            else:
                error = FlameError(FlameErrorCode.INTERNAL, f"Watch failed: {e}")
        finally:
            with self._watch_lock:
                if self._session_watches.get(session_id) is watch:
                    self._session_watches.pop(session_id)
                pending = tuple(watch.pending.values())
                watch.pending.clear()
            for informer in pending:
                informer.on_error(error)

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
            installer=app_attrs.installer,
        )

        request = RegisterApplicationRequest(name=name, application=app_spec)

        try:
            response = self._frontend.RegisterApplication(request)
            _raise_for_result(response, "register application")
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"failed to register application: {e.details()}",
            )

    def unregister_application(self, name: str) -> None:
        """Unregister an application."""
        request = UnregisterApplicationRequest(name=name)

        try:
            response = self._frontend.UnregisterApplication(request)
            _raise_for_result(response, "unregister application")
        except grpc.RpcError as e:
            raise FlameError(
                FlameErrorCode.INTERNAL,
                f"failed to unregister application: {e.details()}",
            )

    def list_applications(self) -> List[Application]:
        """List all applications."""
        request = ListApplicationsRequest()

        try:
            response = self._frontend.ListApplications(request)

            applications = [_application_from_proto(app) for app in response.applications]

            return applications

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list applications: {e.details()}")

    def get_application(self, name: str) -> Optional[Application]:
        """Get an application by name. Returns None if not found."""
        request = GetApplicationRequest(name=name)

        try:
            response = self._frontend.GetApplication(request)
            return _application_from_proto(response)

        except grpc.RpcError as e:
            if e.code() == grpc.StatusCode.NOT_FOUND:
                return None
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to get application: {e.details()}")

    def list_executors(self) -> List[Any]:
        """List all executors."""
        request = ListExecutorsRequest()

        try:
            response = self._frontend.ListExecutors(request)
            return list(response.executors)
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list executors: {e.details()}")

    def list_nodes(self) -> List[Any]:
        """List all nodes."""
        request = ListNodesRequest()

        try:
            response = self._frontend.ListNodes(request)
            return list(response.nodes)
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list nodes: {e.details()}")

    def create_session(self, attrs: SessionAttributes) -> "Session":
        """Create a new session."""

        session_id = short_name(attrs.application) if attrs.id is None else attrs.id

        # Common data should be bytes in core API
        common_data_bytes = attrs.common_data if isinstance(attrs.common_data, bytes) else None
        if common_data_bytes is None and attrs.common_data is not None:
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "common_data must be bytes in core API")

        session_spec = SessionSpec(
            application=attrs.application,
            common_data=common_data_bytes,
            min_instances=attrs.min_instances,
            max_instances=attrs.max_instances if attrs.max_instances is not None else None,
            batch_size=1,
        )
        if attrs.resreq is not None:
            session_spec.resreq.CopyFrom(
                ResourceRequirementProto(
                    cpu=attrs.resreq.cpu,
                    memory=attrs.resreq.memory,
                    gpu=attrs.resreq.gpu,
                )
            )

        request = CreateSessionRequest(session_id=session_id, session=session_spec)

        try:
            response = self._frontend.CreateSession(request)
            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") else None

            session = Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
                events=_events_from_proto(response.status.events),
            )
            return session
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to create session: {e.details()}")

    def list_sessions(self) -> List["Session"]:
        """List all sessions."""
        request = ListSessionsRequest()

        try:
            response = self._frontend.ListSessions(request)

            sessions = []
            for session in response.sessions:
                # Common data is bytes in core API
                common_data_bytes = session.spec.common_data if session.spec.HasField("common_data") else None

                sessions.append(
                    Session(
                        connection=self,
                        id=session.metadata.id,
                        application=session.spec.application,
                        state=SessionState(session.status.state),
                        creation_time=datetime.fromtimestamp(session.status.creation_time / 1000, tz=timezone.utc),
                        pending=session.status.pending,
                        running=session.status.running,
                        succeed=session.status.succeed,
                        failed=session.status.failed,
                        completion_time=(datetime.fromtimestamp(session.status.completion_time / 1000, tz=timezone.utc) if session.status.HasField("completion_time") else None),
                        common_data=common_data_bytes,
                        events=_events_from_proto(session.status.events),
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
                common_data=spec.common_data,
                min_instances=spec.min_instances,
                max_instances=spec.max_instances,
                batch_size=1,
            )
            if spec.resreq is not None:
                session_spec.resreq.CopyFrom(
                    ResourceRequirementProto(
                        cpu=spec.resreq.cpu,
                        memory=spec.resreq.memory,
                        gpu=spec.resreq.gpu,
                    )
                )

        request = OpenSessionRequest(session_id=session_id, session=session_spec)

        try:
            response = self._frontend.OpenSession(request)
            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") else None

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
                events=_events_from_proto(response.status.events),
            )

        except grpc.RpcError as e:
            raise self._grpc_error_to_flame_error(e, "failed to open session")

    def get_session(self, session_id: SessionID) -> "Session":
        """Get a session by ID."""
        request = GetSessionRequest(session_id=session_id)

        try:
            response = self._frontend.GetSession(request)

            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") else None

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
                events=_events_from_proto(response.status.events),
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to get session: {e.details()}")

    def close_session(self, session_id: SessionID) -> "Session":
        """Close a session."""
        request = CloseSessionRequest(session_id=session_id)

        try:
            response = self._frontend.CloseSession(request)
            self._close_task_watch(session_id)

            # Common data is bytes in core API
            common_data_bytes = response.spec.common_data if response.spec.HasField("common_data") else None

            return Session(
                connection=self,
                id=response.metadata.id,
                application=response.spec.application,
                state=SessionState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                pending=response.status.pending,
                running=response.status.running,
                succeed=response.status.succeed,
                failed=response.status.failed,
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                common_data=common_data_bytes,
                events=_events_from_proto(response.status.events),
            )

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to close session: {e.details()}")


class Session:
    connection: Connection
    """Represents a computing session."""
    id: SessionID
    application: str
    state: SessionState
    creation_time: datetime
    pending: int = 0
    running: int = 0
    succeed: int = 0
    failed: int = 0
    completion_time: Optional[datetime] = None
    events: Optional[List[Event]] = None
    _common_data: Optional[bytes] = None
    """Client for session-specific operations."""

    def __init__(
        self,
        connection: Connection,
        id: SessionID,
        application: str,
        state: SessionState,
        creation_time: datetime,
        pending: int,
        running: int,
        succeed: int,
        failed: int,
        completion_time: Optional[datetime],
        common_data: Optional[bytes] = None,
        events: Optional[List[Event]] = None,
    ):
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
        self.mutex = threading.Lock()
        self._common_data = common_data
        self.events = events or []

    def common_data(self) -> Optional[bytes]:
        """Get the common data of Session as bytes."""
        return self._common_data

    def create_task(self, input_data: bytes, option: Optional[TaskOptions] = None) -> Task:
        """Create a new task in the session.

        Args:
            input_data: Task input as bytes (core API works with bytes)
        """
        # Input data should be bytes in core API
        if not isinstance(input_data, bytes):
            raise FlameError(FlameErrorCode.INVALID_ARGUMENT, "input_data must be bytes in core API")

        option = option or TaskOptions()
        task_spec = TaskSpec(session_id=self.id, input=input_data, affinity=list(option.affinity))

        request = CreateTaskRequest(task=task_spec)

        try:
            response = self.connection._frontend.CreateTask(request)

            return Task(
                id=response.metadata.id,
                session_id=self.id,
                state=TaskState(response.status.state),
                creation_time=datetime.fromtimestamp(response.status.creation_time / 1000, tz=timezone.utc),
                input=input_data,
                affinity=set(getattr(getattr(response, "spec", None), "affinity", option.affinity)),
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                events=_events_from_proto(response.status.events),
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
                input=response.spec.input if response.spec.HasField("input") else None,
                output=response.spec.output if response.spec.HasField("output") else None,
                affinity=set(response.spec.affinity),
                completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
                events=_events_from_proto(response.status.events),
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
        request = ListTasksRequest(session_id=self.id)

        try:
            task_stream = self.connection._frontend.ListTasks(request)
            return TaskIterator(task_stream, self.id)

        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to list tasks: {e.details()}")

    def watch_task(self, task_id: TaskID, timeout: Optional[float] = None) -> "TaskWatcher":
        """Yield the current task status, then later updates until terminal.

        Args:
            task_id: The ID of the task to watch
            timeout: Optional timeout in seconds. If specified, iteration will raise
                     TimeoutError if no update is received within the timeout period.

        Returns:
            A TaskWatcher iterator that yields Task updates
        """
        request = WatchTaskRequest(task_id=task_id, session_id=self.id)

        try:
            stream = self.connection._frontend.WatchTasks(iter((request,)))
            return TaskWatcher(stream, timeout=timeout)
        except grpc.RpcError as e:
            raise FlameError(FlameErrorCode.INTERNAL, f"failed to watch task: {e.details()}")

    def invoke(self, input_data: Any, option: Optional[TaskOptions] = None) -> Any:
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
        return self.run(input_data, option=option).result()

    def run(self, input_data: Any, option: Optional[TaskOptions] = None) -> Future:
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
        self.connection._begin_task_submission(self.id)
        try:
            task = self.create_task(input_data, option=option)
            future = _LazyTaskFuture(self, task.id)
            future_informer = _FutureTaskInformer(future)

            self.connection._schedule_task_watch(self.id, task.id, future_informer)
            return future
        finally:
            self.connection._finish_task_submission(self.id)

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
        input=response.spec.input if response.spec.HasField("input") else None,
        output=response.spec.output if response.spec.HasField("output") else None,
        affinity=set(response.spec.affinity),
        completion_time=(datetime.fromtimestamp(response.status.completion_time / 1000, tz=timezone.utc) if response.status.HasField("completion_time") else None),
        events=_events_from_proto(response.status.events),
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
        self._callback_lock = threading.Lock()
        self._callback_queue = deque()
        self._callback_running = False

    def add_done_callback(self, fn) -> None:
        """Run callbacks off the watch loop, in registration order per Future."""
        super().add_done_callback(lambda future: self._queue_callback(fn, future))

    def _add_internal_done_callback(self, fn) -> None:
        """Dispatch SDK completion work independently of user callbacks."""
        super().add_done_callback(lambda future: _internal_callback_executor.submit(fn, future))

    def _queue_callback(self, fn, future) -> None:
        with self._callback_lock:
            self._callback_queue.append((fn, future))
            if not self._callback_running:
                self._callback_running = True
                _task_callback_executor.submit(self._run_callbacks)

    def _run_callbacks(self) -> None:
        while True:
            with self._callback_lock:
                if not self._callback_queue:
                    self._callback_running = False
                    return
                fn, future = self._callback_queue.popleft()
            try:
                fn(future)
            except Exception:
                logger.exception("exception calling callback for %r", self)


class _FutureTaskInformer(TaskInformer):
    """TaskInformer that updates a _LazyTaskFuture when task state changes."""

    def __init__(self, future: Future):
        self._future = future
        self._lock = threading.RLock()

    def on_update(self, task: Task) -> None:
        """Called when task status changes."""
        with self._lock:
            if self._future.done():
                return
            if task.is_cancelled():
                self._future.set_exception(FlameError(FlameErrorCode.INVALID_STATE, "Task was cancelled"))
            elif task.is_failed():
                for event in task.events:
                    if event.code == TaskState.FAILED:
                        self._future.set_exception(FlameError(FlameErrorCode.INTERNAL, f"{event.message}"))
                        return
                self._future.set_exception(FlameError(FlameErrorCode.INTERNAL, "Task failed without error message"))
            elif task.is_completed():
                self._future.set_result(task.output)

    def on_error(self, error: FlameError) -> None:
        """Called when watch stream encounters an error."""
        with self._lock:
            if not self._future.done():
                self._future.set_exception(error)

"""Synchronous facade over the AsyncIO Flame frontend client."""

import asyncio
import logging
import os
import threading
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Any, Dict, List, Optional, Union

from flamepy.core._bridge import LoopThread
from flamepy.core.aio import client as aio_client
from flamepy.core.types import (
    Application,
    ApplicationAttributes,
    FlameClientTls,
    FlameContext,
    FlameError,
    FlameErrorCode,
    ResourceRequirement,
    SessionAttributes,
    SessionID,
    Task,
    TaskID,
    TaskOptions,
)

logger = logging.getLogger(__name__)
_MAX_CALLBACK_JOBS = 256
_CALLBACK_WORKERS = 4


def connect(addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
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
    return ConnectionInstance.instance().create_session(SessionAttributes(id=session_id, application=application, common_data=common_data, min_instances=min_instances, max_instances=max_instances, batch_size=1, resreq=resreq))


def open_session(session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
    return ConnectionInstance.instance().open_session(session_id, spec)


def register_application(name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]) -> None:
    ConnectionInstance.instance().register_application(name, app_attrs)


def unregister_application(name: str) -> None:
    ConnectionInstance.instance().unregister_application(name)


def list_applications() -> List[Application]:
    return ConnectionInstance.instance().list_applications()


def get_application(name: str) -> Optional[Application]:
    return ConnectionInstance.instance().get_application(name)


def list_executors() -> List[Any]:
    return ConnectionInstance.instance().list_executors()


def list_nodes() -> List[Any]:
    return ConnectionInstance.instance().list_nodes()


def list_sessions() -> List["Session"]:
    return ConnectionInstance.instance().list_sessions()


def get_session(session_id: SessionID) -> "Session":
    return ConnectionInstance.instance().get_session(session_id)


def close_session(session_id: SessionID) -> "Session":
    return ConnectionInstance.instance().close_session(session_id)


class ConnectionInstance:
    """Lazily initialized process-wide connection for module-level helpers."""

    _lock = threading.Lock()
    _connection: Optional["Connection"] = None
    _context: Optional[FlameContext] = None

    @classmethod
    def instance(cls) -> "Connection":
        with cls._lock:
            if cls._connection is None or cls._connection._closed:
                cls._context = FlameContext()
                cls._connection = connect(cls._context.endpoint, cls._context.tls)
            return cls._connection

    @classmethod
    def _reset_after_fork(cls) -> None:
        # Parent loop threads and their locks do not survive fork().
        cls._lock = threading.Lock()
        cls._connection = None
        cls._context = None


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=ConnectionInstance._reset_after_fork)


class Connection:
    """Blocking connection backed by one aio connection and event-loop thread."""

    def __init__(self, aio_connection: aio_client.Connection, bridge: LoopThread):
        self.addr = aio_connection.addr
        self._aio = aio_connection
        self._bridge = bridge
        self._callback_executor = ThreadPoolExecutor(max_workers=_CALLBACK_WORKERS, thread_name_prefix="flamepy-callback")
        # Completion waits for a slot on the aio loop, so slow user callbacks
        # cannot create an unbounded executor queue or block that loop.
        self._callback_slots = asyncio.Semaphore(_MAX_CALLBACK_JOBS)
        self._close_lock = threading.Lock()
        self._callback_lock = threading.Lock()
        self._callback_closed = False
        self._pending: set[Future] = set()
        self._pending_lock = threading.Lock()
        self._closed = False

    @classmethod
    def connect(cls, addr: str, tls_config: Optional[FlameClientTls] = None) -> "Connection":
        bridge = LoopThread()
        try:
            return cls(bridge.call(aio_client.connect(addr, tls_config)), bridge)
        except BaseException:
            bridge.close()
            raise

    def _call(self, coroutine):
        return self._bridge.call(coroutine)

    def _release_callback_slot(self) -> None:
        try:
            self._bridge.loop.call_soon_threadsafe(self._callback_slots.release)
        except RuntimeError:
            # Connection shutdown may have stopped the loop already.
            pass

    def _dispatch_callbacks(self, callbacks, future: Future, *, has_slot: bool = False) -> None:
        def invoke() -> None:
            try:
                for callback in callbacks:
                    try:
                        callback(future)
                    except Exception:
                        logger.exception("Flame task completion callback failed")
            finally:
                if has_slot:
                    self._release_callback_slot()

        with self._callback_lock:
            if not self._callback_closed:
                self._callback_executor.submit(invoke)
                return
        # A callback registered after close still runs. Do not run it on a
        # closed connection's aio loop, even in a late-completion race.
        if self._bridge.is_loop_thread():
            threading.Thread(target=invoke, name="flamepy-late-callback", daemon=True).start()
        else:
            invoke()

    def close(self) -> None:
        with self._close_lock:
            if self._closed:
                return
            self._closed = True
        try:
            with self._pending_lock:
                remaining = list(self._pending)
            for future in remaining:
                future._defer_close_callbacks()
            self._call(self._aio.close())
        finally:
            # Aio watch completion schedules Future callbacks on the next
            # loop turn. Drain them before stopping callback dispatch.
            async def drain():
                await asyncio.sleep(0)

            try:
                self._call(drain())
            except RuntimeError:
                pass
            # Resolve every pending Future before executing any of its user
            # callbacks. Four fixed worker jobs drain the close callbacks.
            for future in remaining:
                if not future.done():
                    future.set_exception(FlameError(FlameErrorCode.INTERNAL, "connection closed during task watch"))
            if remaining:
                for worker in range(_CALLBACK_WORKERS):

                    def drain(worker=worker):
                        for index in range(worker, len(remaining), _CALLBACK_WORKERS):
                            remaining[index]._run_deferred_callbacks()

                    self._callback_executor.submit(drain)
            with self._callback_lock:
                self._callback_closed = True
            # User callbacks may wait on unrelated work; connection close must
            # not wait for those user functions to return.
            self._callback_executor.shutdown(wait=False)
            self._bridge.close()

    def register_application(self, name: str, app_attrs: Union[ApplicationAttributes, Dict[str, Any]]) -> None:
        return self._call(self._aio.register_application(name, app_attrs))

    def unregister_application(self, name: str) -> None:
        return self._call(self._aio.unregister_application(name))

    def list_applications(self) -> List[Application]:
        return self._call(self._aio.list_applications())

    def get_application(self, name: str) -> Optional[Application]:
        return self._call(self._aio.get_application(name))

    def list_executors(self) -> List[Any]:
        return self._call(self._aio.list_executors())

    def list_nodes(self) -> List[Any]:
        return self._call(self._aio.list_nodes())

    def _session(self, aio_session: aio_client.Session) -> "Session":
        return Session(self, aio_session)

    def create_session(self, attrs: SessionAttributes) -> "Session":
        return self._session(self._call(self._aio.create_session(attrs)))

    def list_sessions(self) -> List["Session"]:
        return [self._session(session) for session in self._call(self._aio.list_sessions())]

    def open_session(self, session_id: SessionID, spec: Optional[SessionAttributes] = None) -> "Session":
        return self._session(self._call(self._aio.open_session(session_id, spec)))

    def get_session(self, session_id: SessionID) -> "Session":
        return self._session(self._call(self._aio.get_session(session_id)))

    def close_session(self, session_id: SessionID) -> "Session":
        return self._session(self._call(self._aio.close_session(session_id)))


class Session:
    """Blocking facade for one aio session handle."""

    def __init__(self, connection: Connection, aio_session: aio_client.Session):
        self.connection = connection
        self._aio = aio_session
        for name in ("id", "application", "state", "creation_time", "pending", "running", "succeed", "failed", "completion_time", "events"):
            setattr(self, name, getattr(aio_session, name))
        self.mutex = threading.Lock()

    def common_data(self) -> Optional[bytes]:
        return self._aio.common_data()

    def create_task(self, input_data: bytes, option: Optional[TaskOptions] = None) -> Task:
        return self.connection._call(self._aio.create_task(input_data, option))

    def get_task(self, task_id: TaskID) -> Task:
        return self.connection._call(self._aio.get_task(task_id))

    def list_tasks(self) -> "TaskIterator":
        async def start():
            return self._aio.list_tasks()

        return TaskIterator(self.connection._bridge, self.connection._call(start()))

    def watch_task(self, task_id: TaskID, timeout: Optional[float] = None) -> "TaskWatcher":
        async def start():
            return self._aio.watch_task(task_id, timeout)

        return TaskWatcher(self.connection._bridge, self.connection._call(start()))

    def invoke(self, input_data: bytes, option: Optional[TaskOptions] = None) -> bytes:
        return self.run(input_data, option).result()

    def run(self, input_data: bytes, option: Optional[TaskOptions] = None) -> Future:
        result = _LazyTaskFuture(self)
        with self.connection._pending_lock:
            self.connection._pending.add(result)

        def remove_pending(done: Future) -> None:
            with self.connection._pending_lock:
                self.connection._pending.discard(done)

        result._add_internal_callback(remove_pending)

        async def start() -> None:
            async def before_result() -> bool:
                await self.connection._callback_slots.acquire()
                if result._hold_callback_slot():
                    return True
                self.connection._callback_slots.release()
                return False

            aio_future = await self._aio.run(
                input_data,
                option,
                _defer_watch=True,
                _before_result=before_result,
                _discard_result_slot=result._release_held_callback_slot,
            )

            def completed(done: asyncio.Future) -> None:
                if result.done():
                    result._release_held_callback_slot()
                    return
                try:
                    result.set_result(done.result())
                except asyncio.CancelledError:
                    result.set_exception(FlameError(FlameErrorCode.INTERNAL, "task watch cancelled"))
                except Exception as error:
                    result.set_exception(error)

            aio_future.add_done_callback(completed)

            def cancelled(future: Future) -> None:
                if future.cancelled() and not self.connection._bridge.loop.is_closed():
                    try:
                        self.connection._bridge.loop.call_soon_threadsafe(aio_future.cancel)
                    except RuntimeError:
                        pass

            result._add_internal_callback(cancelled)

        try:
            self.connection._call(start())
        except BaseException:
            with self.connection._pending_lock:
                self.connection._pending.discard(result)
            raise
        return result

    def close(self) -> None:
        self.connection.close_session(self.id)


class _AsyncIteratorFacade:
    def __init__(self, bridge: LoopThread, iterator):
        self._bridge = bridge
        self._iterator = iterator
        self._closed = False

    def __iter__(self):
        return self

    def __next__(self) -> Task:
        if self._closed:
            raise StopIteration
        try:
            return self._bridge.call(self._iterator.__anext__())
        except StopAsyncIteration:
            self.close()
            raise StopIteration from None
        except BaseException:
            self.close()
            raise

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self._bridge.loop.call_soon_threadsafe(self._iterator.close)
        except RuntimeError:
            # Closing the connection has already closed its channel and loop.
            pass


class TaskWatcher(_AsyncIteratorFacade):
    """Blocking iterator for task status updates."""


class TaskIterator(_AsyncIteratorFacade):
    """Blocking iterator for a session's tasks."""


class _LazyTaskFuture(Future):
    """Thread-safe completion handle for a remotely running task."""

    def __init__(self, session: Session):
        super().__init__()
        self._session = session
        self._user_callbacks = []
        self._user_callback_lock = threading.Lock()
        self._has_callback_slot = False
        self._close_callbacks_deferred = False
        super().add_done_callback(self._dispatch_registered_callbacks)

    def _dispatch_registered_callbacks(self, _future: Future) -> None:
        with self._user_callback_lock:
            if self._close_callbacks_deferred:
                return
            callbacks = self._user_callbacks
            self._user_callbacks = []
            has_slot = self._has_callback_slot
            self._has_callback_slot = False
        if callbacks:
            self._session.connection._dispatch_callbacks(callbacks, self, has_slot=has_slot)
        elif has_slot:
            self._session.connection._release_callback_slot()

    def _hold_callback_slot(self) -> bool:
        with self._user_callback_lock:
            if self._has_callback_slot:
                return False
            self._has_callback_slot = True
            return True

    def _release_held_callback_slot(self) -> None:
        with self._user_callback_lock:
            has_slot = self._has_callback_slot
            self._has_callback_slot = False
        if has_slot:
            self._session.connection._release_callback_slot()

    def _defer_close_callbacks(self) -> None:
        with self._user_callback_lock:
            self._close_callbacks_deferred = True

    def _run_deferred_callbacks(self) -> None:
        with self._user_callback_lock:
            callbacks = self._user_callbacks
            self._user_callbacks = []
            self._close_callbacks_deferred = False
            has_slot = self._has_callback_slot
            self._has_callback_slot = False
        try:
            for callback in callbacks:
                try:
                    callback(self)
                except Exception:
                    logger.exception("Flame task completion callback failed")
        finally:
            if has_slot:
                self._session.connection._release_callback_slot()

    def _add_internal_callback(self, callback) -> None:
        super().add_done_callback(callback)

    def cancel(self) -> bool:
        if self.done():
            return False
        if self._session.connection._bridge.is_loop_thread():
            raise RuntimeError("synchronous Future.cancel cannot run on its aio loop")
        # Future.cancel() normally invokes callbacks inline on its caller.
        # Preserve that behavior so cancellation never queues callbacks or
        # waits for capacity held by a blocked callback worker.
        self._defer_close_callbacks()
        cancelled = super().cancel()
        self._run_deferred_callbacks()
        return cancelled

    def add_done_callback(self, callback) -> None:
        # Dispatch callbacks registered before completion as one ordered job.
        # A late registration retains Future's normal immediate-call behavior.
        with self._user_callback_lock:
            if not self.done() or self._close_callbacks_deferred:
                self._user_callbacks.append(callback)
                return
        try:
            callback(self)
        except Exception:
            logger.exception("Flame task completion callback failed")

"""A private event-loop thread for the synchronous SDK facade."""

import asyncio
import threading
from concurrent.futures import Future
from typing import Any, Coroutine, TypeVar

_T = TypeVar("_T")


class LoopThread:
    """Run aio SDK operations on one owned loop without blocking that loop."""

    def __init__(self, name: str = "flamepy-aio"):
        self.loop = asyncio.new_event_loop()
        self._thread = threading.Thread(target=self._run, name=name, daemon=True)
        self._lock = threading.Lock()
        self._closed = False
        self._thread.start()

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        try:
            self.loop.run_forever()
        finally:
            pending = asyncio.all_tasks(self.loop)
            for task in pending:
                task.cancel()
            if pending:
                self.loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
            self.loop.close()

    def submit(self, coroutine: Coroutine[Any, Any, _T]) -> Future[_T]:
        if threading.current_thread() is self._thread:
            coroutine.close()
            raise RuntimeError("synchronous Flame API cannot be called from its aio loop")
        with self._lock:
            if self._closed:
                coroutine.close()
                raise RuntimeError("Flame connection is closed")
            return asyncio.run_coroutine_threadsafe(coroutine, self.loop)

    def call(self, coroutine: Coroutine[Any, Any, _T]) -> _T:
        return self.submit(coroutine).result()

    def is_loop_thread(self) -> bool:
        return threading.current_thread() is self._thread

    def close(self) -> None:
        if threading.current_thread() is self._thread:
            raise RuntimeError("synchronous Flame API cannot close its aio loop")
        with self._lock:
            if self._closed:
                return
            self._closed = True
            self.loop.call_soon_threadsafe(self.loop.stop)
        self._thread.join()

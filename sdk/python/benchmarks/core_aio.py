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

Compare blocking, pipelined, and asyncio core task throughput on an in-process server.

Run from sdk/python with: uv run python benchmarks/core_aio.py
"""

import asyncio
import statistics
import threading
import time
from concurrent.futures import wait

import grpc

import flamepy.core as core
from flamepy.core import SessionAttributes, SessionState, TaskState
from flamepy.proto import types_pb2 as pb
from flamepy.proto.frontend_pb2_grpc import FrontendServicer, add_FrontendServicer_to_server


class EchoFrontend(FrontendServicer):
    def __init__(self):
        self.task_id = 0

    async def CreateSession(self, request, context):  # noqa: N802
        return pb.Session(
            metadata=pb.Metadata(id="benchmark-session"),
            spec=pb.SessionSpec(application=request.session.application),
            status=pb.SessionStatus(state=SessionState.OPEN, creation_time=1),
        )

    async def CreateTask(self, request, context):  # noqa: N802
        self.task_id += 1
        return self._task(str(self.task_id), request.task.input)

    async def WatchTasks(self, requests, context):  # noqa: N802
        async for request in requests:
            yield self._task(request.task_id, b"ping")

    @staticmethod
    def _task(task_id: str, payload: bytes):
        return pb.Task(
            metadata=pb.Metadata(id=task_id),
            spec=pb.TaskSpec(session_id="benchmark-session", input=payload, output=payload),
            status=pb.TaskStatus(state=TaskState.SUCCEED, creation_time=1),
        )


class Server:
    def __init__(self):
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        self.server, self.port = asyncio.run_coroutine_threadsafe(self._start(), self.loop).result()

    def _run(self):
        asyncio.set_event_loop(self.loop)
        self.loop.run_forever()
        self.loop.close()

    async def _start(self):
        server = grpc.aio.server()
        add_FrontendServicer_to_server(EchoFrontend(), server)
        port = server.add_insecure_port("127.0.0.1:0")
        await server.start()
        return server, port

    def close(self):
        asyncio.run_coroutine_threadsafe(self.server.stop(0), self.loop).result()
        self.loop.call_soon_threadsafe(self.loop.stop)
        self.thread.join()


def bench_sync(endpoint: str, tasks: int, samples: int, method_name: str) -> tuple[list[float], int]:
    connection = core.connect(endpoint)
    try:
        session = connection.create_session(SessionAttributes(application="benchmark"))
        elapsed = []
        run_task = getattr(session, method_name)
        for _ in range(samples):
            start = time.perf_counter()
            futures = [run_task(b"ping") for _ in range(tasks)]
            wait(futures)
            assert all(future.result() == b"ping" for future in futures)
            elapsed.append(time.perf_counter() - start)
        return elapsed, threading.active_count()
    finally:
        connection.close()


async def bench_aio(endpoint: str, tasks: int, samples: int) -> tuple[list[float], int]:
    import flamepy.core.aio as aio

    async with await aio.connect(endpoint) as connection:
        session = await connection.create_session(SessionAttributes(application="benchmark"))
        elapsed = []
        for _ in range(samples):
            start = time.perf_counter()
            futures = [session.submit(b"ping") for _ in range(tasks)]
            results = await asyncio.gather(*futures)
            assert all(result == b"ping" for result in results)
            elapsed.append(time.perf_counter() - start)
        return elapsed, threading.active_count()


def main():
    server = Server()
    endpoint = f"http://127.0.0.1:{server.port}"
    try:
        print("Mode\tTasks\tSamples\tMedian ms\tTasks/s\tThreads")
        for tasks, samples in ((1, 30), (100, 5), (1000, 3)):
            modes = [
                ("sync-run", bench_sync(endpoint, tasks, samples, "run")),
                ("sync-submit", bench_sync(endpoint, tasks, samples, "submit")),
            ]
            try:
                modes.append(("aio", asyncio.run(bench_aio(endpoint, tasks, samples))))
            except ImportError:
                pass  # main has no aio core namespace
            for mode, (elapsed, threads) in modes:
                median = statistics.median(elapsed)
                print(f"{mode}\t{tasks}\t{samples}\t{median * 1000:.2f}\t{tasks / median:.1f}\t{threads}")
    finally:
        server.close()


if __name__ == "__main__":
    main()

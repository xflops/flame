"""Distributed service declarations for the replay-buffer example."""

import flamepy.app as app
from collector import Collector
from replay_buffer import ReplayBuffer

app.init("replay-buffer")


@app.service(warmup=2)
class BufferService:
    def __init__(self):
        self._buffers: dict[tuple[str, str], ReplayBuffer] = {}

    def _buffer(self, buffer: ReplayBuffer) -> ReplayBuffer:
        ref = buffer.buffer_ref
        key = (ref.endpoint, ref.key)
        retained = self._buffers.get(key)
        if retained is None:
            retained = buffer
            self._buffers[key] = retained
        return retained

    def merge(self, buffer: ReplayBuffer):
        return self._buffer(buffer).merge()

    def state(self, buffer: ReplayBuffer):
        return self._buffer(buffer).state()

    def sample(self, buffer: ReplayBuffer, size: int):
        return self._buffer(buffer).sample(size)


@app.service(autoscale=True)
class CollectorService:
    def __init__(self):
        self._collectors: dict[str, Collector] = {}

    def collect(
        self,
        service_env_name: str,
        buffer: ReplayBuffer,
        num_steps: int,
    ) -> dict:
        collector = self._collectors.get(service_env_name)
        if collector is None:
            collector = Collector(service_env_name)
            self._collectors[service_env_name] = collector
        return collector.collect(buffer, num_steps)

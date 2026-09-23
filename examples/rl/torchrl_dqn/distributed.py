"""Flame services for distributed TorchRL training.

Local mode never imports this module, so it does not initialize a Flame app.
"""

import flamepy.app as app
from collector import FlameTorchRLCollector

app.init("torchrl-dqn")


@app.service(autoscale=True, warmup=1)
class ReplaySamplerService:
    def __init__(self):
        self._buffers: dict[tuple[str, str], object] = {}

    def sample(self, replay_buffer, size: int):
        ref = replay_buffer.storage.object_ref
        key = (ref.endpoint, ref.key)
        retained = self._buffers.get(key)
        if retained is None:
            retained = replay_buffer
            self._buffers[key] = retained
        return retained.sample(size)


@app.service(autoscale=True)
class CollectorService:
    def __init__(self):
        self._collectors: dict[tuple, FlameTorchRLCollector] = {}

    def collect(
        self,
        replay_buffer,
        weights,
        num_steps: int,
        epsilon: float,
        env_name: str,
        obs_dim: int,
        action_dim: int,
        hidden_dim: int,
        seed: int | None,
    ) -> dict:
        key = (env_name, obs_dim, action_dim, hidden_dim, seed)
        collector = self._collectors.get(key)
        if collector is None:
            collector = FlameTorchRLCollector(
                env_name,
                obs_dim,
                action_dim,
                hidden_dim,
                seed=seed,
            )
            self._collectors[key] = collector
        return collector.collect(replay_buffer, weights, num_steps, epsilon)

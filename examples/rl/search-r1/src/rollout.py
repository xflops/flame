"""Rollout service: one vLLM engine per executor, running the Search-R1 loop."""

import time
from typing import Any, Optional

from runtime import NAME, ROLLOUTS, app


@app.service(resreq="cpu=8,mem=64g,gpu=1", warmup=ROLLOUTS)
class Rollout:
    def __init__(self, model: str, corpus: list[str], gpu_memory_utilization: float = 0.4):
        from engine import SearchEngine
        from index import Bm25Index

        # The index lives in the executor next to the engine, so a search is a local call and
        # a rollout never waits on another service.
        self._engine = SearchEngine(model, Bm25Index(corpus).search, gpu_memory_utilization=gpu_memory_utilization)

    def generate(
        self,
        items: list[dict],
        weights: Optional[dict] = None,
        version: int = 0,
        n: int = 1,
        max_turns: int = 4,
        temperature: float = 1.0,
    ) -> Any:
        import traceback

        try:
            started = time.time()
            load_timings = self._engine.load_weights(weights, version) if weights is not None else {}
            loaded = time.time()
            trajectories = self._engine.run(items, n=n, max_turns=max_turns, temperature=temperature)
            # Timings for the driver's step breakdown.
            for trajectory in trajectories:
                trajectory["weights_load_s"] = loaded - started
                trajectory["weights_get_s"] = load_timings.get("get_s", 0.0)
                trajectory["generate_s"] = time.time() - loaded
            # Task output is limited to 128 KB, so the trajectories go through the cache and
            # the call returns a reference; `.get()` on the future reads them back.
            from flamepy.core import ObjectKey, put_object

            return put_object(ObjectKey.for_shared(NAME).to_prefix(), trajectories)
        except Exception:
            # Flame reports a failed task without its cause; keep the traceback in the log.
            print(traceback.format_exc(), flush=True)
            raise

# vLLM Example

This example verifies that a Python environment can start a small vLLM model from Flame's example tree.

## Run

From the Flame console or a local development shell with the example dependencies installed:

```bash
cd /opt/examples/vllm
uv run main.py
```

The script loads `facebook/opt-125m` with vLLM. It is intended as a minimal smoke test for vLLM dependencies and GPU/runtime setup, not as a registered Flame service.

## Data-aware Runner service

`main.py` runs the model as a stateless autoscaled Runner service. Each
replica keeps its own in-memory KV cache, publishes a set of opaque cache keys
through `SessionContext.publish`, and later calls pass matching keys with
`TaskOptions(affinity={key})`. Flame then prefers a replica that published the
matching key; the cache object itself is never written back to object storage.

```bash
cd /opt/examples/vllm
uv run main.py
```

# vLLM Example

This example verifies that a Python environment can start a small vLLM model
from Flame's example tree.

## Run

From the Flame console or a local development shell with the example
dependencies installed:

```bash
cd /opt/examples/vllm
uv run main.py
```

The script loads `facebook/opt-125m` with vLLM. It is intended as a minimal
smoke test for vLLM dependencies and GPU/runtime setup, not as a registered
Flame service.

## Runner instance publication

`main.py` runs the model as a stateless autoscaled Runner service. Each
replica keeps its own in-memory KV cache and uses `RunnerService`'s
`publish_attributes(attrs)` API to publish its opaque cache keys
for each invocation. Its simple key index assumes no cache eviction; production
code should publish the engine's complete current cache index on every response.
The example uses one warmed executor, so it
demonstrates publication rather than cross-session DAS selection; the cache
object itself is never written back to object storage.

Subclassing `RunnerService` is only needed here for attribute publication;
ordinary Runner functions, classes, and instances do not require it.

```bash
cd /opt/examples/vllm
uv run main.py
```

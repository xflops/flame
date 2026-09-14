# vLLM DAS Reuse Example

This example uses data-aware scheduling (DAS) to reuse a retained Runner
executor and its in-memory vLLM engine after the executor unbinds from its
first session.

## Run

From the Flame console or a local development shell with the example
dependencies installed:

```bash
cd /opt/examples/vllm
uv run main.py
```

The example expects the default 60-second application `delay_release` and DAS
in the scheduler policy list:

```yaml
cluster:
  policies:
    - priority
    - drf
    - das
```

It performs this sequence:

1. Create an autoscaled class service with the default zero warmup.
2. Initialize `facebook/opt-125m` in `VllmEngine.__init__`, finish one task,
   and publish the prompt's affinity key.
3. Wait 90 seconds. The executor waits 60 seconds for another task, unbinds,
   and remains Idle within its 120-second retention period.
4. Create another zero-warmup service and submit the same prompt with its
   affinity key.
5. Assert that vLLM reports cached prompt tokens for the second invocation.

`VllmEngine` lives in the importable `engine.py` module. Runner retains class
execution objects by their fully qualified class name, so its model remains an
ordinary instance field. When the executor binds to the second session, Runner
reuses the same `VllmEngine` object. The executor's first-task attributes remain
available to DAS while it is Idle; Session Manager accumulates later published
keys in the same executor set.

The prompt is intentionally longer than a vLLM cache block. Published affinity
keys are scheduling hints and remain on the executor until it is removed; a
stale key after vLLM eviction can only cause a soft preference and vLLM still
handles the cache miss. The model and KV cache remain only in executor memory
and are not persisted to Flame storage.

The Runner uses `fail_if_exists=True` so an interrupted earlier run cannot cause
the example to execute a stale uploaded application package. Remove that stale
application before retrying if registration reports that it already exists.

This intentionally single-executor flow verifies retained cache reuse through
an affinity submission. The Runner DAS E2E test covers selection among multiple
Idle executors.

Subclassing `RunnerService` is only needed here for attribute publication;
ordinary Runner functions, classes, and instances do not require it.

# vLLM Offline DAS Reuse Example

This example uses data-aware scheduling (DAS) to reuse a retained App
executor and its in-memory vLLM engine after the executor unbinds from its
service session.

## Run

From the Flame console with the Flame Python SDK environment loaded:

```bash
source /usr/local/flame/sbin/flmenv.sh
cd "$FLAME_HOME/examples/vllm/offline"
python3 main.py
```

`flmadm install --client --with-examples` includes this example.

The console needs only the Flame SDK. vLLM is imported on the executor, where
Flame installs the pinned dependencies from `pyproject.toml` using Python 3.12.
The service requests `gpu=1`. It runs a single-GPU engine in eager
mode, with a 1024-token context and 10% GPU memory utilization for this small
model smoke example.

GPU FEMs must expose the NVIDIA devices and driver libraries and set
`LD_LIBRARY_PATH`, such as `/run/nvidia/driver/usr/lib/x86_64-linux-gnu` for
GPU Operator installations. Flame's Python installer combines dependency
library paths, application-configured paths, and the FEM's inherited library
paths in that order. Older FEM images that do not preserve inherited paths
require the driver path to be configured explicitly on the App template.

FEM assigns one available physical GPU to each `gpu=1` executor and sets
`CUDA_VISIBLE_DEVICES` when starting the service. Do not set GPU visibility on
the application template. The assignment remains reserved while the executor
is retained between sessions, and is released after its service stops.
Host visibility controls CUDA selection; it does not restrict access to device
files. For container device isolation, see [GPU runtime images](../../../docs/tutorials/gpu-runtime.md).

From a source checkout of `examples/vllm/offline`, check the example without loading a model:

```bash
PYTHONPATH=.:$PYTHONPATH python3 -m unittest discover -s tests -v
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

1. Construct a remote proxy for an autoscaled class service with the default zero
   warmup.
2. Initialize `facebook/opt-125m` in `VllmEngine.__init__`, finish one task,
   and publish the prompt's affinity key.
3. Wait 90 seconds. The executor waits 60 seconds for another task, unbinds,
   and remains Idle within its 120-second retention period.
4. Submit the same service again with the prompt's affinity key, causing the
   retained executor to bind again.
5. Assert that vLLM reports cached prompt tokens for the second invocation.

`VllmEngine` is decorated directly in the importable `engine.py` module after
that module initializes the App. Decoration only declares the service class;
calling `VllmEngine.remote(MODEL)` creates its service proxy and session. `flmrun`
runs `VllmEngine.__init__()` when an executor binds the session, so the client
never constructs or serializes an `LLM` object. Calls use the proxy directly,
for example `engine.generate(...)`.

App retains the constructed service object on its executor, so the model
remains an ordinary instance field. When the executor rebinds the service
session, App reuses the same `VllmEngine` object. The executor's first-task
attributes remain available to DAS while it is Idle; Session Manager
accumulates later published keys in the same executor set.

The prompt is intentionally longer than a vLLM cache block. Published affinity
keys are scheduling hints and remain on the executor until it is removed; a
stale key after vLLM eviction can only cause a soft preference and vLLM still
handles the cache miss. The model and KV cache remain only in executor memory
and are not persisted to Flame storage.

The App uses `fail_if_exists=True` so an interrupted earlier run cannot cause
the example to execute a stale uploaded application package. Remove that stale
application before retrying if registration reports that it already exists.

This intentionally single-executor flow verifies retained cache reuse through
an affinity submission. The App DAS E2E test covers selection among multiple
Idle executors.

The engine remains a plain Python class. During a remote invocation it publishes
affinity keys with `app.publish_attributes(...)`.

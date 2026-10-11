# vLLM Serving Example

This example serves batched text generation through `flamepy.serving`.
The client opens a Flame session, submits prompts, and receives generated text.
The worker declares its handler with `@ins.entrypoint`.
The worker creates its global vLLM engine at startup. The entrypoint reads
session settings with `ins.context()` and passes them to the engine with each request.
The worker retains the model and prefix cache
across session bindings until the executor is released.

## Deploy

Load the installed Flame SDK environment, then deploy the worker package:

```bash
source /usr/local/flame/sbin/flmenv.sh --python-version 3.12
cd "$FLAME_HOME/examples/vllm/serving"
flmctl deploy --name vllm-serving --application . --max-instances 1 \
  --env VLLM_MODEL=facebook/opt-125m
```

`flmctl deploy` detects the `vllm-serving` script in `pyproject.toml`, uploads
the package, and registers the worker. FEM installs its pinned vLLM dependencies.
The console client needs only the Flame SDK; run it with `python3` to avoid
installing CUDA dependencies on the console.

FEM assigns the executor's GPU and sets `CUDA_VISIBLE_DEVICES`; no visibility
override is needed on the application. GPU FEMs must expose NVIDIA devices and
preserve their driver `LD_LIBRARY_PATH` in Python executors; see the Helm GPU
worker guidance. To use a container image instead, see
[GPU runtime images](../../../docs/tutorials/gpu-runtime.md).

Set `VLLM_MODEL` during deployment to select the application model; it defaults
to `facebook/opt-125m`. Sessions cannot change the model. Deploy a separate
application for another model. The engine uses one GPU, a 1024-token context,
10% GPU memory utilization, and eager execution for a small-model smoke test.
Workers need Hugging Face access or locally cached model files.

## Run

```bash
python3 client.py \
  --prompt "The future of distributed inference is" \
  --prompt "A useful AI agent can" \
  --max-tokens 32
```

The client places `max_tokens` and `temperature` in session context.
Generation requests contain only `prompts`. Sampling settings are read from the current session context for each request,
while the model remains loaded across bindings.
Each client invocation requests `gpu=1` and closes its session on exit.
Responses contain the prompt, generated text, and cached prompt token count.
The application stays registered for subsequent clients; remove it with
`flmctl unregister -a vllm-serving` after all sessions close.

From a source checkout of this directory, test the worker lifecycle and
serialization without loading a real model:

```bash
PYTHONPATH=.:$PYTHONPATH python3 -m unittest discover -s tests -v
```

# Candle Based Example

This example runs a HazyResearch Based language model through a Flame Rust
service using Candle and the high-level typed `flame-rs` API. It is adapted from
the upstream Candle Based example with a deliberately small Flame integration
layer: the Candle model loading and generation loop stay close to the original
example, while Flame adds the service boundary, typed messages, and deployment
path needed for distributed execution.

The example shows how to:

- Load a model once in a Flame service instance.
- Pass model options to the service as typed common data.
- Send a typed generation request from a Rust client.
- Deploy the service binary so workers can run it without a source checkout.

Based is a completion model, not an instruction-tuned chat model. Prompts should
look like text to continue, for example `The future of distributed inference is`.
The client defaults use sampled decoding (`--temperature 0.8 --top-p 0.95
--repeat-penalty 1.3`) because deterministic argmax decoding can fall into
repeated text. Use `--temperature 0` only when deterministic output is required.

## Files

- `src/service.rs`: Flame service binary. It loads the tokenizer and model in
  `enter`, then handles generation through a typed `generate` entrypoint.
- `src/client.rs`: Example client binary. It creates a Flame session, attaches
  model options as common data, invokes one generation task, prints the text, and
  closes the session.
- `src/api.rs`: Shared typed request, response, and model option messages.

## Flame Integration Surface

The Flame-specific changes are intentionally small:

- Wrap the Candle model in a `#[flame::instance]` service and load it in
  `enter`.
- Expose generation through one typed `#[flame::entrypoint]`.
- Share request, response, and model options with `FlameMessage` types.
- Use the client to create a Flame session, attach model options as common data,
  and invoke the service.
- Deploy the service binary with `flmctl deploy` so Flame workers can download
  and run it.

The rest of the example remains ordinary Candle code: tokenizer/model loading,
device selection, logits processing, repeat penalty, and token decoding.

## Requirements

- A built Flame checkout or an installed Flame environment with examples.
- A running Flame cluster when using `flmctl deploy` and the client.
- Network access from the worker to Hugging Face for first-run model downloads,
  unless you provide local model files.
- Enough memory for the selected model. Start with `--which 360m`; `1b` and
  `1b-50b` require substantially more memory and startup time.

## Build From Source

From the repository root:

```bash
cargo build -p candle-based-example --release
```

This produces:

- `target/release/candle-based-example-service`
- `target/release/candle-based-example`

## Deploy The Service

Deploying packages the service binary into object cache and registers the Flame
application name. Use the same name later with `--app`.

From a source checkout:

```bash
flmctl deploy \
  --name candle-based-example \
  --application ./target/release/candle-based-example-service
```

From an installed Flame environment:

```bash
source /usr/local/flame/sbin/flmenv.sh
flmctl deploy \
  --name candle-based-example \
  --application /usr/local/flame/examples/candle/based/candle-based-example-service
```

## Run The Client

From a source checkout:

```bash
cargo run -p candle-based-example --bin candle-based-example --release -- \
  --app candle-based-example \
  --prompt "The future of distributed inference is" \
  --sample-len 64 \
  --which 360m
```

From an installed Flame environment:

```bash
/usr/local/flame/examples/candle/based/candle-based-example \
  --app candle-based-example \
  --prompt "The future of distributed inference is" \
  --sample-len 64 \
  --which 360m
```

The client prints the completed text followed by the number of generated tokens,
elapsed time, and tokens per second.

## Example Output

From an installed Flame environment, a successful deploy prints the application
name, package object, content hash, and object-cache URL. The hash values vary
with the service binary build.

```bash
root@bda125c204f6:/usr/local/flame# flmctl deploy \
  --name candle-based-example \
  --application ./examples/candle/based/candle-based-example-service
Application <candle-based-example> deployed.
Input Kind: executable-file
Installer: binary
Command: candle-based-example-service
Object: candle-based-example/pkg/candle-based-example-b7572e003c9cf347.tar.gz
SHA256: b7572e003c9cf347e751b29e3ea732a5ff6d8f7c51c153e640b5c2a0ec66cd08
URL: grpcs://flame-object-cache:9090/candle-based-example/pkg/candle-based-example-b7572e003c9cf347.tar.gz
```

You can confirm that Flame registered the application and that it points at the
packaged service binary:

```bash
root@bda125c204f6:/usr/local/flame# flmctl list -a
 Name                  State    Shim  Tags  Created   Command
 flmexec               Enabled  Host        20:31:51  ${FLAME_HOME}/bin/flmexec-service
 candle-based-example  Enabled  Host        20:32:48  candle-based-example-service
 flmrun                Enabled  Host        20:31:51  python3
 flmping               Enabled  Host        20:31:51  ${FLAME_HOME}/bin/flmping-service

root@bda125c204f6:/usr/local/flame# flmctl view -a candle-based-example
Name:          candle-based-example
Shim:          Host
URL:           grpcs://flame-object-cache:9090/candle-based-example/pkg/candle-based-example-b7572e003c9cf347.tar.gz
Installer:     binary
Command:       candle-based-example-service
Max Instances: 1000000
Delay Release: PT60S
```

Then run the installed client with the same application name:

```bash
root@bda125c204f6:/usr/local/flame# ./examples/candle/based/candle-based-example \
  --app candle-based-example \
  --prompt 'Flying monkeys are'
Flying monkeys are the most common species of bird in the world. They are found
throughout the world, from Central and South America to Australia and New
Zealand.

...

128 tokens generated in 6970 ms (18.36 token/s)
```

The exact completion and throughput depend on the model options, seed, hardware,
and whether the model files are already cached on the worker.

## Model Options

The default model is `hazyresearch/based-360m` at revision `refs/pr/1`, with the
GPT-2 tokenizer from `openai-community/gpt2`.

Common options:

| Option | Default | Description |
| --- | --- | --- |
| `--which` | `360m` | Built-in model size: `360m`, `1b`, or `1b-50b`. |
| `--model-id` | model for `--which` | Override the Hugging Face model repository. |
| `--revision` | `refs/pr/1` | Hugging Face model revision. |
| `--tokenizer-id` | `openai-community/gpt2` | Hugging Face tokenizer repository. |
| `--config-file` | download | Service-side local `config.json` path. |
| `--weight-files` | download | Comma-separated service-side local safetensors paths. |
| `--tokenizer-file` | download | Service-side local `tokenizer.json` path. |
| `--cpu` | false | Force CPU execution. |

Local file paths are evaluated in the service process, not on the client. In a
cluster, those files must exist at the same paths on the worker that runs
`candle-based-example-service`.

## Generation Options

| Option | Default | Description |
| --- | --- | --- |
| `--prompt` | required | Text for Based to continue. |
| `--sample-len`, `-n` | `128` | Maximum generated tokens. |
| `--temperature` | `0.8` | Sampling temperature. Use `0` for deterministic argmax. |
| `--top-p` | `0.95` | Nucleus sampling probability cutoff. |
| `--seed` | `299792458` | Sampling seed. |
| `--repeat-penalty` | `1.3` | Penalty for repeated tokens. Use `1.0` for no penalty. |
| `--repeat-last-n` | `64` | Number of recent tokens considered by repeat penalty. |

Flame session options:

| Option | Default | Description |
| --- | --- | --- |
| `--app` | required | Flame application name used during deploy. |
| `--session-id` | generated | Explicit Flame session id. |
| `--min-instances` | `1` | Minimum service instances for the session. |
| `--max-instances` | `1` | Maximum service instances for the session. |
| `--resreq` | unset | Resource request such as `cpu=4,mem=16g,gpu=1`. |

## Troubleshooting

- If startup is slow, the worker is likely downloading model files from Hugging
  Face. Subsequent runs can reuse the local Hugging Face cache.
- If the output repeats itself, keep sampled decoding enabled and use the
  default repeat penalty. Passing `--temperature 0` intentionally uses
  deterministic argmax and can make repetition worse for this model.
- If local model files are not found, confirm the paths exist on the service
  worker, not only on the machine running the client.
- If the client cannot find the application, deploy the service and
  pass that exact name with `--app`.

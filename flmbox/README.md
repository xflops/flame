# flmbox

`flmbox` runs commands and code in isolated sandboxes. It is one binary with two faces:

- a command line to pull images and to create, enter and remove sandboxes (`flmbox pull|images|ps|run|exec|kill`);
- a Flame service (`flmbox -d`) that gives each session its own sandbox. The built-in `flmbox` application and
  `flamepy.agent.Sandbox` use it.

The first runtime is [gVisor](https://gvisor.dev) (`runsc`). The runtime is a trait, so others, such as Firecracker, can follow.
The design is in `docs/designs/RFE601-agent-sandbox-suspend-resume/FS.md`.

This release covers the sandbox lifecycle only. Sandboxes have **no network** (`runsc --network=none`), and nothing
survives a suspend: the sandbox is removed when its session leaves.

## Requirements

- Linux with `runsc` installed, and root. `pull`, `images`, `--help` and the configuration work anywhere.
- Access to the image registry. Registries are reached over HTTPS and honor `https_proxy`; use an `http://` proxy URL.

## Command line

```bash
flmbox pull ubuntu:26.04                 # download into the node cache, print the digest
flmbox images                            # list the cache
flmbox run --name dev ubuntu:26.04       # create and start a sandbox, print its id
flmbox ps                                # list sandboxes
flmbox exec dev -- ls -la /              # several words are an argument vector
flmbox exec dev 'echo hi | wc -c'        # one word is a shell command line
flmbox exec dev -l python 'print(1+1)'   # run source code (python or shell)
flmbox exec dev -w /tmp -e A=1 -t 10 -i -- cat   # cwd, env, timeout in seconds, stdin
flmbox kill dev                          # kill and remove
```

`exec` writes the command's stdout and stderr and exits with its exit code (`124` on a timeout). `run` without an
image uses `ubuntu:26.04`; without `--name` it generates `box-<hex>`. Sandboxes live in `runsc`'s own state directory, so
separate invocations (and the service) see the same ones.

## Service

`flmbox -d` serves one Flame executor: it creates a sandbox when a session is bound and removes it when the session
leaves. Tasks are JSON, `{"code": "...", "language": "shell|python", "cwd": ..., "envs": {...}, "input": [bytes],
"timeout": secs}`, and the result is `{"exit_code", "stdout", "stderr"}`. The application manifest is
`config/applications/flmbox.yaml`, and `flmadm install` registers it. From Python:

```python
from flamepy.agent import Sandbox

with Sandbox.create(image="ubuntu:26.04") as box:
    print(box.run("echo hello").stdout_text())
```

See `examples/sandbox` for a longer example.

## Configuration

`flmbox` reads the `sandbox:` section of a YAML file. Every key is optional.

```yaml
sandbox:
  state_dir: /var/lib/flmbox    # image cache and sandbox state
  runtime:
    type: runsc                 # the only runtime so far
    path: runsc
    platform: kvm               # optional, runsc --platform
    overlay: root:memory        # runsc --overlay2: writes stay in sandbox memory
  images:
    max_size: 4G                # largest image, packed and unpacked
```

The file is the first of `--config <file>`, `$FLMBOX_CONF` (both must exist), `$FLAME_HOME/conf/flame-sandbox.yaml`
and `~/.flame/flame-sandbox.yaml` (the last two only when present). Environment variables override the file, and the
file overrides the defaults:

| Variable | Overrides |
| -------- | --------- |
| `FLMBOX_STATE_DIR` | `sandbox.state_dir` (default `$FLAME_HOME/data/flmbox`, else `/var/lib/flmbox`) |
| `FLMBOX_RUNSC` | `sandbox.runtime.path` |
| `FLMBOX_PLATFORM` | `sandbox.runtime.platform` |
| `FLMBOX_OVERLAY` | `sandbox.runtime.overlay` |
| `FLMBOX_MAX_IMAGE_BYTES` | `sandbox.images.max_size` |

## Images and state

Images are pulled with `oci-client` (anonymous access; the `linux/<host arch>` manifest of an index) and verified by
digest. They are unpacked once into a flattened, read-only root filesystem, and `runsc --overlay2` provides each
sandbox's writable layer. The unpacker rejects path traversal, escaping symlinks and oversized layers.

```
<state_dir>/
  images/sha256/<manifest-digest>/{rootfs/,image.json}   the cache; pulls go through images/.tmp-*
  sandboxes/<id>/                                        bundle and metadata of a sandbox
  runsc/                                                 runsc's own state
```

## Layout

| Path | Contents |
| ---- | -------- |
| `src/main.rs` | the entry point and the `-d` service |
| `src/cli/` | the command line: `Cli`, and one module per subcommand |
| `src/config.rs` | `FlameSandbox`, the configuration |
| `src/images/` | pull, cache and unpack |
| `src/runtime/` | the `Runtime` trait; `runsc/` is the gVisor runtime, its OCI bundle and its process runner |
| `src/sandbox/` | the lifecycle by sandbox id: `create`, `exec`, `kill`, `ps` |
| `src/service.rs` | the Flame service: one sandbox per session |

## Development

```bash
cargo test -p flmbox      # unit tests use a fake runsc and an in-process registry
cargo clippy -p flmbox --all-targets -- -D warnings
```

Real `runsc` runs are covered by the CI end-to-end jobs (`testing/e2e/tests/test_sandbox.py`).

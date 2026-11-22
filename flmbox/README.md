# flmbox

`flmbox` runs commands and code in isolated sandboxes. It is one binary with two faces:

- a command line to pull images and to create, enter and remove sandboxes (`flmbox pull|images|ps|run|exec|kill`);
- a Flame service (`flmbox -d`) that gives each session its own sandbox. The built-in `flmbox` application and
  `flamepy.agent.Sandbox` use it.

The first runtime is [gVisor](https://gvisor.dev) (`runsc`). The runtime is a trait, so others, such as Firecracker, can follow.
The design is in `docs/designs/RFE601-agent-sandbox-suspend-resume/FS.md`.

This release covers the sandbox lifecycle only. By default sandboxes have **no network** (`runsc --network=none`; set
`sandbox.network: host` to give them the host's, see [Network](#network)), and nothing survives a suspend: the sandbox
is removed when its session leaves.

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
`config/applications/flmbox.yaml`, and `setup.sh` registers it on the head node. From Python:

```python
from flamepy.agent import Sandbox

with Sandbox.create(image="ubuntu:26.04") as box:
    print(box.run("echo hello").stdout_text())
```

See `examples/sandbox` for a longer example.

The service remains alive across session bindings. Startup, session leave and instance teardown reconcile only
sandboxes with this executor's `flmbox-<executor-id>.` prefix, including interrupted creation attempts. Cleanup
stops the runtime before deleting its state and bundle. A failure retains metadata and is returned to the caller;
the service refuses to start a replacement sandbox until cleanup succeeds. Sandboxes belonging to other executors
or the command line are left alone. Processes whose runtime state and bundle were already removed require operator
identification and cleanup; the service does not infer ownership from process names.

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
  network: none                 # none (default) or host, see Network
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
| `FLMBOX_NETWORK` | `sandbox.network` (the `--network` option overrides both) |
| `FLMBOX_MAX_IMAGE_BYTES` | `sandbox.images.max_size` |

## Registry mirrors

Registry routing is configured on the executor node in the same sandbox configuration file used by
`flmbox pull` and `flmbox -d`. Clients continue to use canonical references such as
`docker.io/library/python:3.12-slim` or `docker.io/library/python@sha256:...`.
These settings belong to flmbox's OCI client; containerd and Docker Engine registry settings are independent.

```yaml
sandbox:
  images:
    registries:
      docker.io:
        mirrors:
          - endpoint: https://192.168.3.150:5000/dockerhub
            ca_file: /etc/flame/registry-ca.pem
          - endpoint: https://dockerproxy.net
      registry.example.com:
        mirrors:
          - endpoint: https://registry.internal.example.com
            username_env: REGISTRY_USERNAME
            password_env: REGISTRY_PASSWORD
```

Mirrors are tried in order. An endpoint's optional path is a repository prefix: the first mirror above
serves `library/python` at `/v2/dockerhub/library/python/...`. It is not an arbitrary HTTP API path;
do not append `/v2` unless it is actually part of the repository namespace.
The original upstream registry is always tried after all configured mirrors fail. A registry without
mirrors retains the normal direct HTTPS behavior. Mirror settings are evaluated when an image is pulled.
An unusable endpoint, CA file or credential setting is recorded and skipped, as is a failed pull.
Each attempt retries the complete pull. If every mirror and the upstream fail, the final error includes
the attempted endpoints and their underlying errors. Digest checks, image size limits and safe unpacking
apply to every attempt.

HTTPS verifies the server certificate and hostname normally. `ca_file` adds a PEM CA certificate for
that endpoint only. When using an IP address, the registry certificate must include that IP in its
Subject Alternative Name. An internal HTTPS registry requires no authentication fields if it permits
anonymous pulls. `http://` endpoints explicitly select unencrypted transport; use HTTPS for credentials.
To authenticate, provide both environment variable names and set their values in the CLI process or
executor service environment. No credentials belong in endpoint URLs. To configure authentication or
private trust for the original registry itself, list its HTTPS URL as a mirror endpoint.

Routing changes do not change cache identity: aliases use the original reference, and unpacked roots
use the verified platform-manifest digest. OCI index selection still chooses Linux and the node's
architecture; manifest and blob digests are verified through mirrors. A mirror cannot substitute a
different image for a digest-pinned reference. Tag references still trust the configured registry to
resolve the tag. The existing pull policy applies before network access: `ifnotpresent` reuses cached
tag aliases, `always` re-resolves tags, and cached digest references remain immutable. Changing a mirror
does not itself invalidate cached images.

`HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY` (and their lowercase equivalents) control the OCI client's
outbound HTTP transport, including requests to mirrors. They do not map image repositories or select a
registry mirror. Use an HTTP proxy URL such as `http://proxy.example.com:3128` for an HTTPS CONNECT proxy,
and include internal registry IPs/hostnames in `NO_PROXY` when they should be reached directly.
Sandbox network mode controls sandboxed commands, independently of these node-side image pulls.

During session bootstrap, image-pull failures appear in session binding events with the attempted endpoint
and underlying transport error. Each failed bind records its own event, including failures after the
retry limit is reached. Python
`watch_task(..., timeout=...)` appends available session event messages and their codes to its timeout
error in the order returned by the server. Clients that enforce their own bootstrap deadline should read the session's
events when reporting that timeout. Retry exhaustion pauses new executor assignments; queued tasks
and any existing executors retain their normal lifecycle.

## Network

`sandbox.network` selects the network of every sandbox:

- `none` (the default): the sandbox has its own, empty network namespace (`runsc --network=none`). It cannot reach the
  Flame control plane, the node or the internet.
- `host`: the sandbox uses the network of the process that runs `flmbox` (`runsc --network=host`, no network namespace
  in the sandbox spec), and the host's `/etc/resolv.conf` is mounted read-only so names resolve. The sandbox then has
  **every egress that process has**: the internet, but also the Flame control plane, the node's own services, other
  pods and the cloud metadata address. Use it only where the code that sandboxes run is trusted, such as a development
  cluster.

The mode comes from, in order of precedence, `--network <none|host>`, `$FLMBOX_NETWORK`, and `sandbox.network`. The
built-in `flmbox` application (`config/applications/flmbox.yaml`) starts the service as `flmbox -d --network=host`, so
its sandboxes run with the host's network; because the flag wins over the environment and the file, `sandbox.network`
in `flame-sandbox.yaml` does not change it. To run them without a network, remove the argument from the application.
Running `flmbox` yourself, without the application, still defaults to `none`. The network is chosen by the operator
only: a client cannot request it for its own sandbox. A per-sandbox network with egress rules is tracked in #612.

## Images and state

Images are pulled with `oci-client` (anonymous by default, or configured endpoint credentials; the
`linux/<host arch>` manifest of an index) and verified by
digest. They are unpacked once into a flattened, read-only root filesystem, and `runsc --overlay2` provides each
sandbox's writable layer. The unpacker rejects path traversal, escaping symlinks and oversized layers.

The sandbox root is writable subject to the image user's normal permissions. With the default
`root:memory` overlay, root can create `/workspace/project`, write under `/root`, `/usr/local`, `/var` and `/etc`,
and modify existing image files. These changes belong to that sandbox; they do not modify cached image contents
or appear in another sandbox using the same image. Killing the sandbox discards its writable layer. Preserving
changes across suspend/resume is tracked in #657. Explicit read-only mounts, including `/etc/hosts` and the
host resolver configuration when mounted, remain read-only.

Disabling the private overlay (`none`, `root:none` or `all:none`) is rejected because sandboxes share the cached
image root. Memory and file-backed private overlays remain configurable through `sandbox.runtime.overlay`.

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
The filesystem regression covers root writes, existing-file copy-up and isolation after sandbox deletion.
For a direct CLI check against one shared image cache, run the following as root on Linux with `runsc` installed:

```bash
cargo test -p flmbox --test rootfs -- --ignored --nocapture
```

This test creates isolated temporary state and removes its sandboxes afterwards. It uses `ubuntu:26.04` by default;
`FLMBOX_TEST_IMAGE` can select another root-user image containing `/bin/sh`, standard file utilities and `/etc/os-release`.

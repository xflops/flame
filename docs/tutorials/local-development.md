# Local Development with Flame

This guide sets up a single-node Flame cluster from the local source tree without Docker.

## Prerequisites

- Rust toolchain
- Python 3.12, or another version passed with `flmadm install --python-version`
- `uv`
- Git

The helper script also starts a temporary HTTP package server with `dufs`; install `dufs` only if you use `hack/local-test.sh`.

## Install

Build all release artifacts:

```bash
cargo build --release
```

Install all Flame components into `/tmp/flame-dev`:

```bash
./target/release/flmadm install \
    --all \
    --src-dir . \
    --skip-build \
    --no-systemd \
    --prefix /tmp/flame-dev
```

`flmadm` requires an explicit profile flag. For local development, `--all` installs the control plane, worker, object cache, client tools, and Python SDK.

## Start Services

Start the object cache:

```bash
FLAME_HOME=/tmp/flame-dev \
/tmp/flame-dev/bin/flame-object-cache \
    --config /tmp/flame-dev/conf/flame-cluster.yaml \
    > /tmp/flame-dev/logs/cache.log 2>&1 &
```

Start the session manager:

```bash
FLAME_HOME=/tmp/flame-dev \
/tmp/flame-dev/bin/flame-session-manager \
    --config /tmp/flame-dev/conf/flame-cluster.yaml \
    > /tmp/flame-dev/logs/fsm.log 2>&1 &
```

Start the executor manager:

```bash
FLAME_HOME=/tmp/flame-dev \
/tmp/flame-dev/bin/flame-executor-manager \
    --config /tmp/flame-dev/conf/flame-cluster.yaml \
    > /tmp/flame-dev/logs/fem.log 2>&1 &
```

Verify the cluster:

```bash
/tmp/flame-dev/bin/flmctl list -a
```

The built-in applications should include `flmping`, `flmexec`, and `flmrun`.

## Client Configuration

The Python SDK can use environment variables:

```bash
export FLAME_ENDPOINT=http://127.0.0.1:8080
export FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090
```

Or create `~/.flame/flame.yaml`:

```yaml
---
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://127.0.0.1:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
    package:
      excludes:
        - "*.log"
        - "*.pkl"
        - "*.tmp"
```

`package.storage` is optional. When it is absent, Runner packages are uploaded to the object cache through `cache.endpoint`.

## Run Tests

Run Python E2E tests against the local cluster:

```bash
cd e2e
PYTHONPATH="$PWD/src:$PYTHONPATH" \
FLAME_ENDPOINT=http://127.0.0.1:8080 \
pytest -vv --durations=0 .
```

The Make target wraps the same local endpoint:

```bash
make e2e-py-local
```

Run Rust E2E tests:

```bash
FLAME_ROOT="$PWD" cargo test --workspace --exclude cri-rs -- --nocapture
```

## Development Loop

After changing Rust components:

```bash
cargo build --release
./target/release/flmadm install \
    --all \
    --src-dir . \
    --skip-build \
    --no-systemd \
    --prefix /tmp/flame-dev \
    --clean
```

Then restart the three services.

## Helper Script

`hack/local-test.sh` wraps install, start, stop, logs, and test commands:

```bash
./hack/local-test.sh install
./hack/local-test.sh start
./hack/local-test.sh status
./hack/local-test.sh test
./hack/local-test.sh stop
```

The script starts an additional `dufs` server for HTTP package storage, so `dufs` must be installed before `./hack/local-test.sh start`.

## Logs

Service logs are written under `/tmp/flame-dev/logs`:

- `/tmp/flame-dev/logs/cache.log`
- `/tmp/flame-dev/logs/fsm.log`
- `/tmp/flame-dev/logs/fem.log`

View recent logs with:

```bash
tail -f /tmp/flame-dev/logs/cache.log
tail -f /tmp/flame-dev/logs/fsm.log
tail -f /tmp/flame-dev/logs/fem.log
```

## Configuration

`flmadm` generates `/tmp/flame-dev/conf/flame-cluster.yaml`:

```yaml
---
cluster:
  name: flame
  endpoint: "http://127.0.0.1:8080"
  resreq: "cpu=1,mem=2g"
  policies:
    - priority
    - drf
    - gang
  storage: "fs:///tmp/flame-dev/data"
  executors:
    shim: host
  limits:
    max_executors: 128
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "lo"
  storage: "/tmp/flame-dev/data/cache"
```

Restart the services after changing this file.

## Troubleshooting

If services do not start, inspect the matching log file first. Common issues are an occupied port, an invalid configuration file, or a missing `FLAME_HOME` environment variable when starting services manually.

If a Python client cannot connect, verify `FLAME_ENDPOINT` and `FLAME_CACHE_ENDPOINT`, then confirm the corresponding services are listening on ports `8080` and `9090`.

If Runner fails to find the `flmrun` template, check that the session manager is running and that `flmctl list -a` shows `flmrun`.

## Cleanup

Stop the service processes, then uninstall:

```bash
./target/release/flmadm uninstall \
    --prefix /tmp/flame-dev \
    --no-backup \
    --force
```

## See Also

- [flmadm README](../../flmadm/README.md)
- [Runner Setup Guide](runner-setup.md)
- [RFE333 Functional Specification](../designs/RFE333-flmadm/FS.md)

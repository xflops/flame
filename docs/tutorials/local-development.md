# Local Development with Flame

This guide sets up a single-node Flame cluster from the local source tree without Docker.

## Prerequisites

- Rust toolchain
- Python 3.12, or another version passed with `setup.sh --python-version`
- `uv`
- Git

## Install

Build the release package (`scripts/build.sh` builds everything), then install the head node and a worker that joins it into `/tmp/flame-dev`, and the client into `~/.flame` (`--force` replaces an earlier install, the config included; without it each script asks before overwriting, and stops when it cannot ask):

```bash
scripts/build.sh --output /tmp/flame-package --force
/tmp/flame-package/package/sbin/setup.sh --prefix /tmp/flame-dev --no-systemd --force
/tmp/flame-package/package/sbin/setup.sh --prefix /tmp/flame-dev --join --no-systemd --force
FLAME_ENDPOINT=http://127.0.0.1:8080 FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090 /tmp/flame-package/package/sbin/sdk.sh --force
source ~/.flame/sbin/flmenv.sh
```

`setup.sh` without `--join` installs the head node (the session manager) and with `--join` a worker (the executor manager and the object cache). `setup.sh` generates `/tmp/flame-dev/conf/flame-cluster.yaml` from `FLAME_*` settings (see [Configuration](#configuration)); the defaults suit one machine. `sdk.sh` installs `flmctl`, `flmping`, `flmexec`, `flmbench` and the Python SDK; it writes `~/.flame/flame.yaml` from `FLAME_ENDPOINT` and `FLAME_CACHE_ENDPOINT` (the environment or `./.env`), which is why they are set above; without `FLAME_ENDPOINT` it writes none, and there is no default (see [the install scripts README](../../scripts/README.md)).

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

The standard manifests installed in `/tmp/flame-dev/conf/applications` should
register `flmping`, `flmexec`, and `flmrun`.

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
```

`package.storage` is optional. When it is absent, App packages are uploaded to the object cache through `cache.endpoint`.

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

After changing Rust components, rebuild the package and install it over the old one (`--force` replaces what is already installed, the config included):

```bash
scripts/build.sh --output /tmp/flame-package --force
/tmp/flame-package/package/sbin/setup.sh --prefix /tmp/flame-dev --no-systemd --force
/tmp/flame-package/package/sbin/setup.sh --prefix /tmp/flame-dev --join --no-systemd --force
```

Then restart the three services.

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

`setup.sh` generates `/tmp/flame-dev/conf/flame-cluster.yaml` from `FLAME_*` settings in the environment or `./.env` (defaults below; see [the install scripts README](../../scripts/README.md)); for example on one machine:

```yaml
---
cluster:
  name: flame
  endpoint: "http://127.0.0.1:8080"
  resreq: "cpu=1,mem=2g"
  policies:
    - priority
    - drf
    - das
  storage: "fs:///tmp/flame-dev/data"
  executors:
    shims: [host]
  limits:
    application: {}
    session: {}
    node:
      max_executors: 128
cache:
  endpoint: "grpc://127.0.0.1:9090"
  advertise: "lo"
  storage: "/tmp/flame-dev/data/cache"
```

Restart the services after changing this file.

`limits.node.max_executors` caps active executors on each node. Set
`limits.application.max_executors` or `limits.session.max_executors` to add hard
ceilings per application or session. Each ceiling also applies when the
application or session requests a higher `max_instances`; lower requested limits
still apply. An omitted application or session ceiling leaves its requested
limit in effect. Idle executors continue to count toward application and node
limits; unbinding removes an executor only from its session count.

## Troubleshooting

If services do not start, inspect the matching log file first. Common issues are an occupied port, an invalid configuration file, or a missing `FLAME_HOME` environment variable when starting services manually.

If a Python client cannot connect, verify `FLAME_ENDPOINT` and `FLAME_CACHE_ENDPOINT`, then confirm the corresponding services are listening on ports `8080` and `9090`.

If App fails to find the `flmrun` template, check that the session manager is running and that `flmctl list -a` shows `flmrun`.

## Cleanup

Stop the service processes, then remove the directory (see [Removing Flame](../../scripts/README.md#removing-flame)):

```bash
rm -rf /tmp/flame-dev ~/.flame
```

## See Also

- [Install scripts README](../../scripts/README.md)
- [App Setup Guide](app-setup.md)
- [RFE683 Functional Specification](../designs/RFE683-script-install/FS.md)

# Runner API Setup Guide

The Runner API packages the current Python project, registers a temporary Flame application from the `flmrun` template, and exposes Python functions, classes, or instances as remote services.

## Prerequisites

- A running Flame cluster with the session manager, executor manager, and object cache.
- The built-in `flmrun` application registered in Flame.
- A Python environment that can import `flamepy`.

Verify the template application:

```python
import flamepy

flmrun = flamepy.get_application("flmrun")
if flmrun is None:
    raise RuntimeError("flmrun application is not registered")
```

## Client Configuration

Runner reads `~/.flame/flame.yaml` through `flamepy.core.FlameContext`. A minimal local configuration is:

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

`package.storage` is optional. When it is absent, Runner uploads packages to the Flame object cache through `cache.endpoint`.

Supported package storage schemes:

- `grpc://` and `grpcs://`: Flame object cache storage.
- `file://`: shared filesystem storage.
- `http://` and `https://`: HTTP storage with PUT, GET, and DELETE support.

Example with explicit HTTP package storage:

```yaml
package:
  storage: "http://127.0.0.1:5050/packages"
  excludes:
    - "*.log"
    - "data/"
    - "models/"
```

Use `runner.template` only when the cluster uses a custom template application name:

```yaml
runner:
  template: "flmrun"
```

## Basic Usage

### Verify A Cluster

The Python SDK includes a small Runner E2E command for checking that a configured cluster can run Runner workloads:

```bash
python -m flamepy.runner.e2e
```

Installed wheels also expose the same check as:

```bash
flamepy-runner-e2e
```

The check packages a temporary project, verifies the configured `flmrun` template, runs a function service, passes `ObjectFuture` values between services, and runs a stateful instance service. Use `--json` for machine-readable output, `--tasks N` to change the function-task count, and `--python-version 3.12` to verify a specific executor Python runtime.

### Function Service

```python
from flamepy.runner import Runner

def sum_fn(a: int, b: int) -> int:
    return a + b

with Runner("sum-app") as runner:
    sum_service = runner.service(sum_fn)
    result = sum_service(1, 3)
    print(result.get())
```

Functions default to autoscaling sessions.

### Class or Instance Service

```python
from flamepy.runner import Runner

class Counter:
    def __init__(self, initial: int = 0):
        self._count = initial

    def add(self, value: int) -> int:
        self._count += value
        return self._count

    def get(self) -> int:
        return self._count

with Runner("counter-app") as runner:
    counter = runner.service(Counter(10), warmup=1)
    counter.add(1).wait()
    counter.add(3).wait()
    print(counter.get().get())
```

Functions, builtins, and classes are stateless. Object instances are stateful fixed services and support only `warmup=0` or `warmup=1`.

### Passing ObjectFuture Values

Remote calls return `ObjectFuture`. Passing an `ObjectFuture` into another remote call sends the underlying `ObjectRef` instead of fetching and re-uploading the object.

```python
from flamepy.runner import Runner

def double(value: int) -> int:
    return value * 2

def add(a: int, b: int) -> int:
    return a + b

with Runner("chain-app") as runner:
    double_service = runner.service(double)
    add_service = runner.service(add)

    first = double_service(21)
    total = add_service(first, 8)
    print(total.get())
```

## API Reference

### Runner

Constructor:

```python
Runner(name: str, fail_if_exists: bool = False)
```

- `name`: application and package name.
- `fail_if_exists`: when `True`, raise if the application already exists. The default reuses an existing application and skips cleanup for it.

Methods:

- `service(execution_object, autoscale=None, warmup=0, resreq=None)`: create a `RunnerServiceInstance`.
- `get(futures)`: resolve multiple `ObjectFuture` values to concrete objects.
- `ref(futures)`: resolve multiple `ObjectFuture` values to `ObjectRef` values.
- `wait(futures)`: wait for multiple futures without fetching objects.
- `select(futures)`: iterate over futures as they complete.
- `put_object(obj)`: store a shared object under the runner application prefix.
- `close()`: close services, unregister the application if Runner registered it, delete cached objects, and remove the uploaded package.

`resreq` accepts `flamepy.ResourceRequirement`, for example:

```python
import flamepy

with Runner("cpu-app") as runner:
    svc = runner.service(
        sum,
        resreq=flamepy.ResourceRequirement.from_string("cpu=1,mem=1g"),
    )
```

### RunnerServiceInstance

`RunnerServiceInstance` exposes public methods of the function, class, or instance passed to `Runner.service()`.

This client proxy was previously exported as `RunnerService`. It is renamed to
`RunnerServiceInstance`; `RunnerService` now names the optional executor-side
base described below.

- Function services are callable directly.
- Class and instance services expose one wrapper method for each public method.
- Every remote call returns `ObjectFuture`.
- `close()` closes the underlying Flame session.

Default service behavior with `warmup=0`:

| Execution object | Default `stateful` | Default `autoscale` | Effective `min_instances` | Effective `max_instances` |
|------------------|--------------------|---------------------|---------------------------|---------------------------|
| Function or builtin | `False` | `True` | `0` | unlimited |
| Class | `False` | `True` | `0` | unlimited |
| Instance | `True` | `False` | `1` | `1` |

For functions, builtins, and classes, `autoscale` is configurable. When `warmup=N` and `N > 0`, autoscaled services use `min_instances=N` and no max limit; fixed services use `min_instances=N` and `max_instances=N`. Object instances are always fixed, always stateful, and reject `warmup` values other than `0` or `1`.

### Data-Aware Instance Selection

`RunnerService` is optional. Continue passing plain functions, classes, or
instances to `Runner.service()` unless the execution object needs the active
session context or attribute publication.

Subclass `flamepy.runner.RunnerService` and use `publish_attributes(attrs)` to
add opaque `bytes` keys held by that instance. Repeated calls during one method
accumulate, and Runner drains the published set after each response. Session
Manager unions every response into the executor's retained attribute set.
`session_context()` also exposes the active Flame session context:

```python
import os
from pathlib import Path

import flamepy
from flamepy.runner import Runner, RunnerService


class Cache(RunnerService):
    def __init__(self):
        # Runner retains class execution objects with the executor process.
        self.path = Path(f"/tmp/flame-runner-cache-{os.getpid()}")
        self.keys = set()

    def store(self, key: bytes, value: str) -> str:
        self.path.write_text(value)
        self.keys = {key}
        self.publish_attributes(self.keys)
        return value

    def load(self, key: bytes) -> str:
        self.keys.add(key)
        self.publish_attributes(self.keys)
        return self.path.read_text()


with Runner("cache-app") as runner:
    key = b"model:block:7"

    warm_cache = runner.service(Cache, warmup=2)
    warm_cache.store(key, "value").wait()

    target_cache = runner.service(Cache, warmup=0)
    warm_cache.close()
    result = target_cache.load(key, option=flamepy.TaskOptions(affinity={key}))
    print(result.get())
```

Runner publishes accumulated keys after session entry and every task, including
a failed task. Session Manager retains previously accepted keys, and publishing
nothing is a no-op. Keys must be nonempty and at most 256 bytes. One publication
round supports at most 1,024 distinct keys and 64 KiB after deduplication.

Runner retains class execution objects by fully qualified class name in each
executor process and reuses them when that executor binds to another session.
Supplied object instances remain session-scoped and stateful, while functions
remain session-scoped and can use their module-level state. The second session
is created before the first one closes so its task is ready to use the retained
executor. Idle instances remain reusable for twice the application's configured
`delay_release` before Shuffle releases them: 120 seconds with the default
60-second setting.

### ObjectFuture

Methods:

- `get()`: fetch and deserialize the concrete object.
- `ref()`: return the underlying `flamepy.core.ObjectRef`.
- `wait()`: wait for completion without fetching the object.

## Package Contents

Runner packages the current working directory into `dist/<name>.tar.gz`.

Default exclusions include:

- `.venv`, `venv`
- `__pycache__`
- `.pytest_cache`, `.ruff_cache`, `.mypy_cache`
- `*.egg-info`
- `.git`, `.tox`
- `node_modules`
- `*.pyc`, `*.pyo`
- `.DS_Store`

Additional `package.excludes` patterns from `~/.flame/flame.yaml` are merged with those defaults.

## Working Directory

Runner derives the registered application's `working_directory` from the `flmrun` template. If the template has a working directory, Runner appends `/<runner-name>`. If the template has no working directory, Runner leaves it unset.

## Troubleshooting

`Failed to get application template 'flmrun'`: confirm the session manager is running and `flmrun` appears in `flmctl list -a`.

`Storage not configured`: configure `cache.endpoint` or `package.storage`. In a local setup, `cache.endpoint: "grpc://127.0.0.1:9090"` is enough.

`Storage directory does not exist`: for `file://` storage, create the directory on a filesystem that both the client and executor nodes can access.

Package upload or download failures: verify the selected storage backend is reachable from both the client and executor nodes. For object-cache storage, check the object-cache service on port `9090`.

Pickle or import errors: keep service functions and classes importable from the packaged project, and make sure executor nodes have the required Python dependencies.

## See Also

- [Local Development](local-development.md)
- [Python SDK](../sdk/python.md)
- [Python SDK README](../../sdk/python/README.md)
- [Runner implementation](../../sdk/python/src/flamepy/runner/runner.py)

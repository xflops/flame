# App API Setup Guide

The App API packages the current Python project, registers a temporary Flame application from the `flmrun` template, and exposes Python functions and classes as remote services.

## Prerequisites

- A running Flame cluster with the session manager, executor manager, and object cache.
- A configured `flmrun` application registered from
  `<config-dir>/applications/flmrun.yaml` when the session manager starts.
- A Python environment that can import `flamepy`.

Verify the template application:

```python
import flamepy

flmrun = flamepy.get_application("flmrun")
if flmrun is None:
    raise RuntimeError("flmrun application is not registered")
```

## Client Configuration

App reads `~/.flame/flame.yaml` through `flamepy.core.FlameContext`. A minimal local configuration is:

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

`package.storage` is optional. When it is absent, App uploads packages to the Flame object cache through `cache.endpoint`.

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

The optional `app` field defaults to `flmrun`. Set it only when the cluster
uses a custom template application name:

```yaml
app: "custom-flmrun"
```

## Basic Usage

### Verify A Cluster

The Python SDK includes a small App E2E command for checking that a configured cluster can run App workloads:

```bash
uv run --project e2e python -m e2e.app
```

The check packages a temporary project, verifies the configured `flmrun` template, runs a function service, passes `ObjectFuture` values between services, and runs a remotely constructed class service. Use `--json` for machine-readable output, `--tasks N` to change the function-task count, and `--python-version 3.12` to verify a specific executor Python runtime.

### Function Service

```python
import flamepy.app as app

app.init("sum-app")


@app.service()
def sum_fn(a: int, b: int) -> int:
    return a + b


result = sum_fn(1, 3)
print(result.get())
app.destroy()
```

Functions default to autoscaling sessions.

### Class Service

```python
import flamepy.app as app


app.init("counter-app")


@app.service(warmup=1)
class Counter:
    def __init__(self, initial: int = 0):
        self._count = initial

    def add(self, value: int) -> int:
        self._count += value
        return self._count

    def get(self) -> int:
        return self._count

counter = Counter(10)
counter.add(1).wait()
counter.add(3).wait()
counter.add(5).wait()
print(counter.get().get())
app.destroy()
```

Decorating `Counter` declares the service class but does not create a session.
`Counter(10)` creates the service handle and session, and `flmrun` runs
`Counter.__init__(10)` in the executor. Call methods directly on the handle;
Flame does not add a `.remote()` suffix. Class-level calls such as
`Counter.add(...)` are not supported. The options on `@app.service(...)` apply
to the session created for the handle.

### Passing ObjectFuture Values

Remote calls return `ObjectFuture`. Passing an `ObjectFuture` into another remote call sends the underlying `ObjectRef` instead of fetching and re-uploading the object.

```python
import flamepy.app as app


app.init("chain-app")


@app.service()
def double(value: int) -> int:
    return value * 2


@app.service()
def add(a: int, b: int) -> int:
    return a + b


first = double(21)
total = add(first, 8)
print(total.get())
app.destroy()
```

Services can also call one another from an executor. A captured service proxy
is serialized as a reference to the already-created session:

```python
@app.service()
def fn_a(value: int) -> int:
    return value * 2


@app.service()
def fn_b(value: int) -> int:
    return fn_a(value).get()
```

## API Reference

### Process-wide application

Initialize the application once before declaring services:

```python
flamepy.app.init(name, fail_if_exists=False)
```

- `name`: application and package name. Repeating `init()` with the same name
  returns the same runtime handle.
- `fail_if_exists`: when `True`, raise if the application already exists. The default reuses an existing application and skips cleanup for it.

Top-level functions:

- `app.service(autoscale=None, warmup=0, resreq=None)`: canonical decorator.
  Functions become service proxies after `app.init()`. For classes, decoration
  declares the service without creating a session; calling the decorated class
  creates a handle and session, and its constructor runs in the executor.
- `get(futures)`: resolve multiple `ObjectFuture` values to concrete objects.
- `ref(futures)`: resolve multiple `ObjectFuture` values to `ObjectRef` values.
- `wait(futures)`: wait for multiple futures without fetching objects.
- `select(futures)`: iterate over futures as they complete.
- `flamepy.app.put(obj)`: store a shared object under the active application prefix.
- `destroy()`: close services, unregister the application, delete cached objects, and remove the uploaded package.

Recursive services are the lifecycle exception. An `@app.service()` declaration
made while service code is running automatically borrows that invocation's
existing session. It requires neither `app.init()` nor `app.destroy()` and does
not own or close the parent session. Nested declarations reuse the existing
session configuration, so they do not accept `autoscale`, `warmup`, or `resreq`.
They must remain in the invocation's execution context, and nested calls must
complete before the parent invocation returns.

`resreq` accepts a resource string, for example:

```python
import flamepy.app as app

app.init("cpu-app")


@app.service(resreq="cpu=1,mem=1g")
def add(left: int, right: int) -> int:
    return left + right
```

### ServiceInstance

`app.service()` is the canonical decorator. A decorated function immediately
becomes a `ServiceInstance`. A decorated class becomes a service factory;
calling it creates a `ServiceInstance` and sends the constructor arguments to
the executor. Each constructed handle has its own service ID and retained
object; constructing the same decorated class twice does not share object
state. `app.destroy()` closes those service sessions.

Public methods on a decorated class must not use names reserved by
`ServiceInstance`, such as `close`. A collision is rejected when the handle is
created rather than silently hiding the user method.

When declarations live in another module, initialize the application before
importing that module so its decorators execute against the active application:

```python
import flamepy.app as app

app.init("pipeline-app")

# pipeline.services uses @app.service() for its declarations.
from pipeline import services  # noqa: E402

result = services.transform("input")
print(result.get())
app.destroy()
```

- Function services are callable directly.
- Constructed class handles expose one wrapper method for each public method.
- Every remote call returns `ObjectFuture`.

Default service behavior with `warmup=0`:

| Execution object | Default `autoscale` | Effective `min_instances` | Effective `max_instances` |
|------------------|---------------------|---------------------------|---------------------------|
| Function or builtin | `True` | `0` | unlimited |
| Class | `True` | `0` | unlimited |

For functions, builtins, and classes, `autoscale` is configurable. When
`warmup=N` and `N > 0`, autoscaled services use `min_instances=N` and no max
limit; fixed services use `min_instances=N` and `max_instances=N`. Passing an
already constructed object to `app.service()` is not supported.

### Data-Aware Instance Selection

App services can be plain functions or classes. During a service
invocation, call `app.publish_attributes(attrs)` to add opaque `bytes` keys held
by that executor. Repeated calls during one method accumulate, and App drains
the published set after each response. Session Manager unions every response
into the executor's retained attribute set. `app.session_context()` exposes the
active Flame session context during the same invocation:

```python
import os
from pathlib import Path

import flamepy.app as app
from flamepy import TaskOptions


app.init("cache-app")


class CacheBase:
    def __init__(self):
        # App retains class execution objects with the executor process.
        self.path = Path(f"/tmp/flame-app-cache-{os.getpid()}")
        self.keys = set()

    def store(self, key: bytes, value: str) -> str:
        self.path.write_text(value)
        self.keys = {key}
        app.publish_attributes(self.keys)
        return value

    def load(self, key: bytes) -> str:
        self.keys.add(key)
        app.publish_attributes(self.keys)
        return self.path.read_text()


@app.service(warmup=2)
class WarmCache(CacheBase):
    pass


@app.service(warmup=0)
class TargetCache(CacheBase):
    pass


key = b"model:block:7"
warm_cache = WarmCache()
target_cache = TargetCache()
warm_cache.store(key, "value").wait()
warm_cache.close()
result = target_cache.load(key, option=TaskOptions(affinity={key}))
print(result.get())
app.destroy()
```

App publishes accumulated keys after every task invocation, including a failed
task. Session Manager retains previously accepted keys, and publishing nothing
is a no-op. Keys must be nonempty and at most 256 bytes. One publication
round supports at most 1,024 distinct keys and 64 KiB after deduplication.

App constructs class execution objects from the supplied constructor arguments
in each executor process. It retains an object by its handle's service ID when
that executor binds to another session for the same service. Distinct handles
have distinct service IDs and do not share objects, even if they were created
from the same decorated class with identical arguments. Functions remain
session-scoped and can use their module-level state. The second session is
created before the first one closes so its task is ready to use the retained
executor. Idle instances remain reusable for twice the application's configured
`delay_release` before Shuffle releases them: 120 seconds with the default
60-second setting.

### ObjectFuture

Methods:

- `get()`: fetch and deserialize the concrete object.
- `ref()`: return the underlying `flamepy.core.ObjectRef`.
- `wait()`: wait for completion without fetching the object.

## Package Contents

App packages the current working directory into `dist/<name>.tar.gz`.

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

App derives the registered application's `working_directory` from the `flmrun` template. If the template has a working directory, App appends `/<app-name>`. If the template has no working directory, App leaves it unset.

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
- [App implementation](../../sdk/python/src/flamepy/app/__init__.py)

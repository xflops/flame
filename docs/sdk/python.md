# Flame Python SDK

The Python SDK is distributed as `flamepy`. It provides:

- A synchronous client for sessions, tasks, and application registration.
- A host-shim service base class for Python services.
- Object-cache helpers for pickled objects, files, and versioned references.
- The App API for packaging Python code and invoking functions or objects remotely.

## Install

Install the SDK package:

```bash
pip install flamepy
```

The package requires Python 3.9 or newer.

For local development from this repository:

```bash
python3 -m pip install -e sdk/python --user --no-build-isolation
```

## Configure A Client

The SDK reads `~/.flame/flame.yaml` by default:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://127.0.0.1:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
```

Environment variables override the file:

- `FLAME_ENDPOINT`
- `FLAME_CACHE_ENDPOINT`
- `FLAME_CACHE_STORAGE`
- `FLAME_CA_FILE`

Use `https://` for the session-manager endpoint when TLS is enabled. Use `grpcs://` for the object-cache endpoint when cache TLS is enabled.

## Create Sessions And Run Tasks

The core client API uses bytes for task input, task output, and common data:

```python
from concurrent.futures import wait

import flamepy

session = flamepy.create_session(
    "flmping",
    min_instances=1,
    resreq=flamepy.ResourceRequirement.from_string("cpu=1,mem=1g"),
)

output = session.run(b"hello")
print(output)

futures = [session.submit(f"task {idx}".encode()) for idx in range(10)]
wait(futures)
outputs = [future.result() for future in futures]

session.close()
```

`submit()` returns a `TaskFuture` before the `CreateTask` RPC replies, so
tasks can be submitted concurrently; submission and execution errors surface
through `future.result()`. `run()` blocks until task completion and returns its
output, raising submission or execution errors directly.

Use `session.create_task()`, `session.get_task()`, `session.list_tasks()`, and
`session.watch_task()` when callers need explicit task objects or streamed task
updates. `watch_task()` first yields the latest available status, which may
already be terminal, then yields updates until the task reaches a terminal
state. Intermediate updates may be coalesced when the iterator is not read.

The Python SDK uses one task watch stream per session. Calls to `run()`,
`submit()`, and `watch_task()` register their task IDs on that stream;
multiple watchers of the same task share incoming updates. Each registration
asks the frontend for its current task status, then receives later updates.
The SDK retains each Future and watcher until its task finishes or the stream
closes.
A terminal update removes that task's registration, and a stream error is
delivered to every remaining Future and watcher. If you stop reading a
watcher before its task finishes, the SDK still retains it until that task
finishes or the stream closes.

### AsyncIO Core Client

Use `flamepy.core.aio` for native asyncio calls. It has the same core object
and method names as `flamepy.core`; I/O methods use `await`, and task streams
use `async for`. `Session.submit()` returns an `asyncio.Task` immediately;
await that task for the result:

```python
import asyncio

import flamepy.core.aio as flame_aio


async def main():
    async with await flame_aio.connect("http://127.0.0.1:8080") as connection:
        session = await connection.create_session(
            flame_aio.SessionAttributes(application="flmping", min_instances=1)
        )
        futures = [session.submit(f"task {idx}".encode()) for idx in range(10)]
        outputs = await asyncio.gather(*futures)
        await session.close()
        return outputs


print(asyncio.run(main()))
```

The synchronous core uses the aio implementation through a private loop
thread. Aio connections belong to the event loop that created them; close
them before that loop exits.

## Register Applications

Most users deploy applications with `flmctl deploy`. Pass a project directory
to package it as a `.tar.gz`, upload it to object cache, and register the
application:

```bash
flmctl deploy --name agent-app --application ./agent-app
```

The directory should provide a detectable command, such as a matching
`[project.scripts]` entry in `pyproject.toml`. Otherwise pass `--command`
explicitly. Put `.flmignore` or `.flameignore` in the directory to omit files
from its archive. Both use Gitignore-style patterns and also apply to
`flamepy.app.init()` packages.

A directory can also provide an application profile in `flame.yaml` or
`flm.yaml`, using the same `metadata` and `spec` fields as `flmctl register`:

```yaml
metadata:
  name: agent-app
spec:
  installer: python
  command: python3
  arguments: [-m, agent_app]
```

With this profile, `flmctl deploy --application ./agent-app` gets the name and
runtime settings from the file. Explicit CLI options override profile fields;
`--env` updates matching environment variables while retaining the others.
The cache URL always comes from the newly uploaded package. If both profile
filenames exist, `flame.yaml` takes precedence. A standalone script can also
be deployed:

```bash
flmctl deploy --name script-app --application ./main.py
```

This packages `main.py` as `main.py.tar.gz` and runs it with `python3` without
installing project dependencies. Use a directory with Python package metadata
when the script needs dependencies. The SDK can also register an application
directly:

```python
import flamepy

flamepy.register_application(
    "echo",
    {
        "shim": flamepy.Shim.HOST,
        "command": "python /opt/echo/service.py",
        "description": "Echo service",
    },
)
```

Use the same application name when creating a session:

```python
session = flamepy.create_session("echo")
```

## Write A Service

Subclass `FlameService` and run it with `flamepy.run()`:

```python
from typing import Optional

import flamepy


class Echo(flamepy.FlameService):
    def on_session_enter(self, context: flamepy.SessionContext):
        self.session_id = context.session_id
        self.common_data = context.common_data()

    def on_task_invoke(self, context: flamepy.TaskContext) -> Optional[bytes]:
        self.publish({b"echo-cache"})
        return context.input

    def on_session_leave(self):
        self.session_id = None


if __name__ == "__main__":
    flamepy.run(Echo())
```

The service runtime provides `FLAME_INSTANCE_ENDPOINT` and calls the service through a Unix domain socket. Service methods should return bytes or `None`.

`self.publish()` adds opaque locality keys to the next successful session-enter
response or the next task response, including a failed task. Calls within one
response are unioned, and Session Manager extends the executor's retained set
with every response. Keys are nonempty `bytes` values of at most 256 bytes. One
publication round supports at most 1,024 distinct keys and 64 KiB after
deduplication.

## Use The Serving Helper

For object-oriented or agent-style applications, `flamepy.serving` provides a higher-level API that serializes Python objects through object cache:

```python
from flamepy import serving

instance = serving.Instance()


@instance.entrypoint
def answer(question: str) -> str:
    history = instance.context() or []
    history.append(question)
    instance.update_context(history)
    return f"received {question}"


if __name__ == "__main__":
    instance.run()
```

Clients use `flamepy.serving.open_session()` with the deployed application name:

```python
from flamepy.serving import open_session

with open_session("agent-app", ctx=[]) as session:
    print(session.run("hello"))
    future = session.submit("another request")
    print(future.result())
    print(session.context())
```

`run()` waits for a deserialized Python result. `submit()` returns a future whose
`result()` returns that same Python result; task failures surface through the
future. Use this helper when request, response, or session context objects are
easier to model as Python objects than raw bytes. Use the core `FlameService`
API when you need explicit byte-level protocol control.

## Use Object Cache

Top-level helpers store Python objects in Flame object cache:

```python
import flamepy

ref = flamepy.put_object("my-app/shared", {"temperature": 0.8})
value = flamepy.get_object(ref)

next_ref = flamepy.update_object(ref, {"temperature": 0.7})
next_value = flamepy.get_object(next_ref)
```

Object references are versioned. `version=0` forces a fresh download. Nonzero versions allow the client to reuse cached state and request newer patches when the cache server can provide them.

Lower-level helpers under `flamepy.core` expose `ObjectKey`,
`patch_object()`, `upload_object()`, `download_object()`, and
`delete_objects()`. The `flamepy.cache` namespace provides common object cache
operations and reference types.

For asyncio callers, `flamepy.core.aio.cache` exposes awaitable versions of
these cache operations with the same names. Import it as `aio_cache` and call
`await aio_cache.close()` before the owning event loop exits to close its
channels. Synchronous cache helpers use this aio transport through a private
loop thread.

## Use App

App packages the current Python project, registers a Flame application based on the configured app template, and exposes Python functions or classes as remote services:

```python
import flamepy.app as app


app.init("square-app")


@app.service(warmup=2)
def square(value: int) -> int:
    return value * value


futures = [square.remote(idx) for idx in range(8)]
print(app.get(futures))
app.destroy()
```

App returns `ObjectFuture` values. Use `future.get()` to retrieve a concrete
result and `future.ref()` to get its `Ref`: `ValueRef` for an inline value or
`ObjectRef` for an explicitly cached value. Use `app.wait()` to wait for a batch
and `app.select()` to iterate as results complete.

App result retrieval remains synchronous through `future.get()` or
`app.get(futures)`. Remote service functions and class methods may be defined
with `async def`; the worker awaits them before returning the result.

`app.init(name, fail_if_exists=False, dependencies=None,
python_version=None)` initializes the process-wide application and returns its
runtime handle. Calling it again with the same name returns that handle. Set
`fail_if_exists=True` when an existing registration should be an error. A
disabled existing application is never reused and always produces an error.
`dependencies` is used only to generate a `pyproject.toml` when the packaged
project has no Python package metadata; otherwise, declare dependencies in the
project's existing metadata. `python_version` selects the executor Python
version through the application template. `app.service()` is the canonical
decorator and must run after initialization. Decorated functions and classes
run locally when called directly. `fn.remote(*args, **kwargs)` submits a remote
function call on a shared session and returns an `ObjectFuture`. Use
`app.remote(fn)` to create a separate function proxy and session. For a class,
`Clazz.remote(*args, **kwargs)` or `app.remote(Clazz, *args, **kwargs)` creates
a proxy and passes constructor arguments to `flmrun`:

```python
@app.service(autoscale=False, warmup=1)
class Counter:
    def __init__(self, value=0):
        self.value = value

    def increment(self):
        self.value += 1
        return self.value


counter = Counter.remote(10)  # creates a proxy; flmrun constructs Counter(10)
counter.increment()
```

Class-level calls such as `Counter.increment()` are not supported. Call methods
on the remote proxy with `counter.increment()`. The decorator options configure
the session created by each `.remote()` call. `app.destroy()` closes the
sessions created by this process. If this process registered the application,
`app.destroy()` also unregisters it and removes its package and cache. With
`fail_if_exists=False`,
an existing application is borrowed and its lifecycle remains the user's
responsibility; `app.destroy()` does not unregister it. Application execution
objects are functions or classes; already constructed objects are not accepted.
Each constructed handle has a unique service ID. With one fixed executor, as
in the counter above, each handle has one retained object and predictable
mutable state. With autoscaling enabled (the default), every executor retains
its own copy, so mutable fields are replica-local rather than a distributed
singleton.
Creating two remote proxies from the same decorated class does not share
object state. Public class methods must not collide with `ServiceInstance`
API names such as `close`.

During an App invocation, `app.session_context()` returns the active session
context, while `app.publish_attributes(attrs)` adds opaque `bytes` keys to the
current response. Repeated calls in one response accumulate; App publishes and
drains the set at the response boundary. Session Manager unions every response
into the executor's retained attribute set. Task calls can request a matching
instance with `TaskOptions(affinity={key})`.

The returned service-side context is the core `flamepy.SessionContext`. An inner
`@app.service()` declaration made during a service invocation can call
`fn.remote(...)` or `Clazz.remote()` to reuse that invocation's session. This
recursive path is the
exception to the normal lifecycle ordering: it needs neither `app.init()` nor
`app.destroy()`, because it owns neither the parent application nor the reused
session. Nested declarations must remain in the invocation's execution context
and do not accept `autoscale`, `warmup`, or `resreq`, because those settings
cannot change an existing session. Nested calls must complete before the parent
invocation returns when the parent waits for their result. Submission itself is
valid at any capacity; synchronous waiting requires enough free executor
capacity to run the nested task.

The App process retains a constructed service object by service ID on its
executor and reuses it across bindings for that service. Separate constructed
handles use separate IDs and objects, even for the same decorated class and
constructor arguments. Functions remain session-scoped while their imported
module-level state naturally follows Python module lifetime. Service affinity
keys may therefore identify data held directly by the retained object. This
executor-local state is volatile: it is not persisted or migrated and is
discarded when the executor process exits.

To verify a configured cluster end to end with App, run:

```bash
uv run --project e2e python -m e2e.app
```

The repository-level check validates the `flmrun` template, App package upload/install, function execution, `ObjectFuture` chaining, and a remotely constructed class service.

## API Map

| Area | Python API |
|------|------------|
| Connect | `flamepy.connect()` |
| Sessions | `create_session()`, `open_session()`, `get_session()`, `list_sessions()`, `close_session()` |
| Tasks | `Session.run()`, `Session.submit()`, `Session.create_task()`, `Session.watch_task()` |
| Applications | `register_application()`, `unregister_application()`, `get_application()`, `list_applications()` |
| Services | `FlameService`, `flamepy.run()`, `flamepy.serving.Instance`, `flamepy.serving.open_session()` |
| Objects | `put_object()`, `get_object()`, `update_object()`, `patch_object()`, `upload_object()`, `download_object()` |
| App | `flamepy.app.init()`, `flamepy.app.service()`, `flamepy.app.destroy()`, `flamepy.app.session_context()`, `flamepy.app.publish_attributes()`, `ServiceInstance`, `ObjectFuture` |

See also:

- [Python SDK API reference](../../sdk/python/docs/API.md)
- [Python SDK README](../../sdk/python/README.md)
- [Python Pi App example](../../examples/pi/python/README.md)
- [OpenAI agent service example](../../examples/agents/openai/README.md)

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
    package:
      excludes:
        - "*.log"
        - "*.pkl"
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

output = session.invoke(b"hello")
print(output)

futures = [session.run(f"task {idx}".encode()) for idx in range(10)]
wait(futures)
outputs = [future.result() for future in futures]

session.close()
```

Use `session.create_task()`, `session.get_task()`, `session.list_tasks()`, and `session.watch_task()` when callers need explicit task objects or streamed task updates.

## Register Applications

Most users deploy applications with `flmctl deploy`. The SDK can also register an application directly:

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

## Use The Service Helper

For object-oriented or agent-style applications, `flamepy.service` provides a higher-level API that serializes Python objects through object cache:

```python
from flamepy import service

instance = service.FlameInstance()


@instance.entrypoint
def answer(question: str) -> str:
    history = instance.context() or []
    history.append(question)
    instance.update_context(history)
    return f"received {question}"


if __name__ == "__main__":
    instance.run()
```

Clients use `flamepy.service.Session` with the deployed application name:

```python
from flamepy.service import Session

with Session("agent-app", ctx=[]) as session:
    print(session.invoke("hello"))
    print(session.context())
```

Use this helper when request, response, or session context objects are easier to model as Python objects than raw bytes. Use the core `FlameService` API when you need explicit byte-level protocol control.

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

Lower-level helpers under `flamepy.core` and `flamepy.cache` also expose `ObjectKey`, `patch_object()`, `upload_object()`, `download_object()`, and `delete_objects()`.

## Use App

App packages the current Python project, registers a Flame application based on the configured app template, and exposes Python functions or classes as remote services:

```python
import flamepy.app as app


app.init("square-app")


@app.service(warmup=2)
def square(value: int) -> int:
    return value * value


futures = [square(idx) for idx in range(8)]
print(app.get(futures))
app.destroy()
```

App returns `ObjectFuture` values. Use `future.get()` to fetch a concrete result, `future.ref()` to get the `ObjectRef`, `app.wait()` to wait for a batch, and `app.select()` to iterate as results complete.

`app.init(name, fail_if_exists=False, dependencies=None,
python_version=None)` initializes the process-wide application and returns its
runtime handle. Calling it again with the same name returns that handle. Set
`fail_if_exists=True` when an existing registration should be an error. A
disabled existing application is never reused and always produces an error.
`dependencies` is used only to generate a `pyproject.toml` when the packaged
project has no Python package metadata; otherwise, declare dependencies in the
project's existing metadata. `python_version` selects the executor Python
version through the application template. `app.service()` is the canonical
decorator and must run after initialization. Functions become
`ServiceInstance` proxies. Decorating a class declares a service class without
creating a session. Calling the decorated class creates its service handle and
session; `flmrun` runs the class constructor, including its arguments, in the
executor:

```python
@app.service(autoscale=False, warmup=1)
class Counter:
    def __init__(self, value=0):
        self.value = value

    def increment(self):
        self.value += 1
        return self.value


counter = Counter(10)     # creates a handle; flmrun constructs Counter(10)
counter.increment()
```

Class-level calls such as `Counter.increment()` are not supported. Flame method
calls remain direct Python calls—use `counter.increment()`, without a
`.remote()` suffix. The decorator options configure the session created by each
class construction. `app.destroy()` closes the sessions created by this
process. If this process registered the application, `app.destroy()` also
unregisters it and removes its package and cache. With `fail_if_exists=False`,
an existing application is borrowed and its lifecycle remains the user's
responsibility; `app.destroy()` does not unregister it. Application execution
objects are functions or classes; already constructed objects are not accepted.
Each constructed handle has a unique service ID. With one fixed executor, as
in the counter above, each handle has one retained object and predictable
mutable state. With autoscaling enabled (the default), every executor retains
its own copy, so mutable fields are replica-local rather than a distributed
singleton.
Constructing the same decorated class twice does not share object state. Public
class methods must not collide with `ServiceInstance` API names such as `close`.

During an App invocation, `app.session_context()` returns the active session
context, while `app.publish_attributes(attrs)` adds opaque `bytes` keys to the
current response. Repeated calls in one response accumulate; App publishes and
drains the set at the response boundary. Session Manager unions every response
into the executor's retained attribute set. Task calls can request a matching
instance with `TaskOptions(affinity={key})`.

The returned service-side context is the core `flamepy.SessionContext`. An inner
`@app.service()` declaration made during a service invocation automatically
reuses that invocation's session. This recursive declaration is the exception
to the normal lifecycle ordering: it needs neither `app.init()` nor
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
| Tasks | `Session.invoke()`, `Session.run()`, `Session.create_task()`, `Session.watch_task()` |
| Applications | `register_application()`, `unregister_application()`, `get_application()`, `list_applications()` |
| Services | `FlameService`, `flamepy.run()`, `flamepy.service.FlameInstance`, `flamepy.service.Session` |
| Objects | `put_object()`, `get_object()`, `update_object()`, `patch_object()`, `upload_object()`, `download_object()` |
| App | `flamepy.app.init()`, `flamepy.app.service()`, `flamepy.app.destroy()`, `flamepy.app.session_context()`, `flamepy.app.publish_attributes()`, `ServiceInstance`, `ObjectFuture` |

See also:

- [Python SDK API reference](../../sdk/python/docs/API.md)
- [Python SDK README](../../sdk/python/README.md)
- [Python Pi App example](../../examples/pi/python/README.md)
- [OpenAI agent service example](../../examples/agents/openai/README.md)

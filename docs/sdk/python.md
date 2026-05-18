# Flame Python SDK

The Python SDK is distributed as `flamepy`. It provides:

- A synchronous client for sessions, tasks, and application registration.
- A host-shim service base class for Python services.
- Object-cache helpers for pickled objects, files, and versioned references.
- The Runner API for packaging Python code and invoking functions or objects remotely.

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
        return context.input

    def on_session_leave(self):
        self.session_id = None


if __name__ == "__main__":
    flamepy.run(Echo())
```

The service runtime provides `FLAME_INSTANCE_ENDPOINT` and calls the service through a Unix domain socket. Service methods should return bytes or `None`.

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

## Use Runner

Runner packages the current Python project, registers a Flame application based on the configured runner template, and exposes Python callables or objects as remote services:

```python
from flamepy.runner import Runner


def square(value: int) -> int:
    return value * value


with Runner("square-app") as runner:
    svc = runner.service(square, warmup=2)
    futures = [svc(idx) for idx in range(8)]
    print(runner.get(futures))
```

Runner returns `ObjectFuture` values. Use `future.get()` to fetch a concrete result, `future.ref()` to get the `ObjectRef`, `runner.wait()` to wait for a batch, and `runner.select()` to iterate as results complete.

## API Map

| Area | Python API |
|------|------------|
| Connect | `flamepy.connect()` |
| Sessions | `create_session()`, `open_session()`, `get_session()`, `list_sessions()`, `close_session()` |
| Tasks | `Session.invoke()`, `Session.run()`, `Session.create_task()`, `Session.watch_task()` |
| Applications | `register_application()`, `unregister_application()`, `get_application()`, `list_applications()` |
| Services | `FlameService`, `flamepy.run()`, `flamepy.service.FlameInstance`, `flamepy.service.Session` |
| Objects | `put_object()`, `get_object()`, `update_object()`, `patch_object()`, `upload_object()`, `download_object()` |
| Runner | `Runner`, `Runner.service()`, `ObjectFuture` |

See also:

- [Python SDK API reference](../../sdk/python/docs/API.md)
- [Python SDK README](../../sdk/python/README.md)
- [Python Pi Runner example](../../examples/pi/python/README.md)
- [OpenAI agent service example](../../examples/agents/openai/README.md)

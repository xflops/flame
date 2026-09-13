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
response are unioned; the transported set is a complete replacement for the
instance's previous snapshot. Keys are nonempty `bytes` values of at most 256
bytes. One publication round supports at most 1,024 distinct keys and 64 KiB
after deduplication.

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

`Runner.service()` returns a public `RunnerServiceInstance` client handle. It
exposes the execution object's remote methods and owns that session's `close()`
lifecycle. Application execution objects subclass `RunnerService` when they
need service-side session context or instance-attribute publication.
Plain functions, classes, and object instances do not need to subclass it when
they use neither API.

Migration: the client proxy previously exported as `RunnerService` is renamed
to `RunnerServiceInstance`. The `RunnerService` name now refers to the optional
executor-side base class.

Subclass `flamepy.runner.RunnerService` to access Runner-managed service state.
`self.session_context()` returns the active session context, while
`self.publish_attributes(attrs)` adds opaque `bytes` keys to the current
session-entry or invocation response. Repeated calls in one response accumulate;
Runner publishes and drains the set at the response boundary. Every response is
a complete replacement, so each invoked method must publish all currently valid
keys, including methods that do not change the cache. Task calls can request a
matching instance with `TaskOptions(affinity={key})`.

The returned service-side context is `flamepy.SessionContext`. It is distinct
from `flamepy.runner.SessionContext`, which configures Runner session creation.

The Runner process is retained with its executor, but Runner reloads the
execution object on each session entry and clears it on session leave. Affinity
keys intended for reuse across sessions should identify data retained outside
that session's execution object, such as process-local or external cached data.

To verify a configured cluster end to end with Runner, run:

```bash
python -m flamepy.runner.e2e
```

Installed wheels also provide the `flamepy-runner-e2e` command. The check validates the `flmrun` template, Runner package upload/install, function execution, `ObjectFuture` chaining, and a stateful instance service.

## API Map

| Area | Python API |
|------|------------|
| Connect | `flamepy.connect()` |
| Sessions | `create_session()`, `open_session()`, `get_session()`, `list_sessions()`, `close_session()` |
| Tasks | `Session.invoke()`, `Session.run()`, `Session.create_task()`, `Session.watch_task()` |
| Applications | `register_application()`, `unregister_application()`, `get_application()`, `list_applications()` |
| Services | `FlameService`, `flamepy.run()`, `flamepy.service.FlameInstance`, `flamepy.service.Session` |
| Objects | `put_object()`, `get_object()`, `update_object()`, `patch_object()`, `upload_object()`, `download_object()` |
| Runner | `Runner`, `RunnerService`, `RunnerServiceInstance`, `ObjectFuture` |

See also:

- [Python SDK API reference](../../sdk/python/docs/API.md)
- [Python SDK README](../../sdk/python/README.md)
- [Python Pi Runner example](../../examples/pi/python/README.md)
- [OpenAI agent service example](../../examples/agents/openai/README.md)

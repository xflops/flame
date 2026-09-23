# Flame Python SDK API Reference

The `flamepy` package provides a synchronous client for Flame sessions and tasks, a service base class for host-shim applications, object-cache helpers, the App API for packaging Python workloads, and the Agent Session API for remote script execution.

## Configuration

By default the SDK reads `~/.flame/flame.yaml`:

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
```

Environment variables override the file:

- `FLAME_ENDPOINT`
- `FLAME_CACHE_ENDPOINT`
- `FLAME_CACHE_STORAGE`
- `FLAME_CA_FILE`

## Client API

Top-level helpers use the configured default connection:

```python
import flamepy

session = flamepy.create_session(
    "flmping",
    min_instances=1,
    resreq=flamepy.ResourceRequirement.from_string("cpu=1,mem=1g"),
)
output = session.invoke(b"hello")
session.close()
```

Public helpers:

- `connect(addr, tls_config=None) -> Connection`
- `create_session(application, common_data=None, session_id=None, min_instances=0, max_instances=None, batch_size=1, resreq=None) -> Session`
- `open_session(session_id, spec=None) -> Session`
- `register_application(name, app_attrs) -> None`
- `unregister_application(name) -> None`
- `list_applications() -> list[Application]`
- `get_application(name) -> Application | None`
- `list_sessions() -> list[Session]`
- `get_session(session_id) -> Session`
- `close_session(session_id) -> Session`

## Session

`Session` represents an open or closed Flame session.

Methods:

- `create_task(input_data: bytes) -> Task`
- `get_task(task_id) -> Task`
- `list_tasks() -> Iterator[Task]`
- `watch_task(task_id, timeout=None) -> TaskWatcher`
- `invoke(input_data) -> bytes | None`
- `run(input_data) -> concurrent.futures.Future`
- `close() -> None`
- `common_data() -> bytes | None`

`create_task()` expects bytes. `run()` creates a task, watches it in the background, and resolves the returned `Future` with the task output.

## Data Classes

`SessionAttributes`:

- `application: str`
- `id: str | None`
- `common_data: bytes | None`
- `min_instances: int`
- `max_instances: int | None`
- `batch_size: int` (reserved for future implementation; currently normalized to `1`)
- `resreq: ResourceRequirement | None`

`ApplicationAttributes`:

- `shim: Shim | None`
- `image: str | None`
- `description: str | None`
- `labels: list[str] | None`
- `command: str | None`
- `arguments: list[str] | None`
- `environments: dict[str, str] | None`
- `working_directory: str | None`
- `max_instances: int | None`
- `delay_release: int | None`
- `schema: ApplicationSchema | None`
- `url: str | None`
- `installer: str | None`

`ResourceRequirement.from_string("cpu=1,mem=1g,gpu=0")` parses user-friendly resource strings into CPU, memory bytes, and GPU counts.

## Object Cache

Top-level `flamepy` exports:

- `ObjectRef`
- `put_object(key_prefix, obj)`
- `get_object(ref, deserializer=None)`
- `update_object(ref, new_obj)`

`flamepy.core` also exports:

- `ObjectKey`
- `WILDCARD_SESSION`
- `patch_object(ref, delta)`
- `upload_object(key_or_prefix, file_path)`
- `download_object(ref, dest_path)`

`flamepy.cache` exports `delete_objects(key_prefix)` in addition to the basic cache helpers.

Object references are versioned:

- `version=0` forces a fresh download.
- `version>=1` lets the client reuse a cached base object and fetch only newer patches when the server can provide them.
- Without a custom deserializer, `get_object()` returns only the base object for backward compatibility.
- With a deserializer, `get_object(ref, deserializer)` calls `deserializer(base, deltas)`.

## Service API

Host-shim Python services subclass `flamepy.FlameService` and run with `flamepy.run()`:

```python
import flamepy

class Echo(flamepy.FlameService):
    def on_session_enter(self, context):
        self.session_id = context.session_id

    def on_task_invoke(self, context):
        return context.input

    def on_session_leave(self):
        pass

if __name__ == "__main__":
    flamepy.run(Echo())
```

Service contexts expose bytes-oriented APIs:

- `SessionContext.session_id`
- `SessionContext.application`
- `SessionContext.common_data()`
- `TaskContext.task_id`
- `TaskContext.session_id`
- `TaskContext.input`

## App API

The process-wide application API is exported from `flamepy.app`:

```python
import flamepy.app as app


app.init("add-app")


@app.service()
def add(a, b):
    return a + b


print(add(1, 2).get())
app.destroy()
```

A service may invoke another declared service. The captured proxy is restored
in the executor as a reference to its existing session:

```python
@app.service()
def fn_a(value):
    return value * 2


@app.service()
def fn_b(value):
    return fn_a(value).get()
```

Key classes and helpers:

- `app.session_context()` returns the active service-side session context
  (`flamepy.SessionContext`) during an App invocation.
- `app.publish_attributes(attrs)` adds opaque `bytes` locality keys to the
  current App response; repeated calls in the response accumulate. Session
  Manager unions every response into the executor's retained set.
- `init(name, fail_if_exists=False)` packages and initializes the application
  and returns its runtime handle. Repeating it for the same name returns the
  same handle.
- `app.service(autoscale=None, warmup=0, resreq=None)` is the canonical service
  decorator. Functions become `ServiceInstance` proxies. Decorating a class
  declares a service class without creating a session. Calling
  `DecoratedClass(args...)` creates its service handle and session, and
  `flmrun` runs the constructor with those arguments in the executor. The
  decorator accepts functions and classes, not already constructed objects.
- Decorated classes do not expose class-level service methods. Invoke methods
  directly on a constructed handle, for example `instance.method(...)`; Flame
  does not require a `.remote()` suffix.
- Each constructed handle has a unique service ID and its own retained object;
  two handles created from the same decorated class do not share object state.
- Public class methods must not collide with `ServiceInstance` API names such
  as `close`; handle construction rejects such definitions.
- The handle returned by `app.init()` also exposes `service(...)` when direct
  runtime access is useful. A declaration made during another App invocation
  is the sole no-init exception; it reuses the current session.
- `ServiceInstance` is the client proxy returned for a decorated function or by
  calling a decorated class with its remote constructor arguments.
- A recursive declaration may call the service decorator without `init()` and
  does not call `destroy()`; it reuses and does not own the parent session.
- `get(futures)`, `ref(futures)`, `wait(futures)`, `select(futures)`
- `put(obj)` stores a shared object under the active application's cache prefix.
- `destroy()` closes services and releases the process-wide application.
- `ObjectFuture.get()`, `ObjectFuture.ref()`, `ObjectFuture.wait()`

## Agent Session API

The agent `Session` lives under `flamepy.agent`. It runs remote Python or shell scripts through the built-in `flmexec` application without exposing the core `Session` or task JSON.

```python
from flamepy.agent import open_session
from flamepy import ResourceRequirement, FlameError

with open_session() as ssn:
    print(ssn.run_code("print(1 + 2)").text())
```

Key classes and methods:

- `open_session(*, ssn_id=None, language="python", runtime=None, min_instances=0, max_instances=None, resreq=None)`
- `open_session()` creates a Python session using the server-default runtime
- `open_session(ssn_id="...")` reopens a session by ID
- `Session.run_code(code, input=None)`
- `Session.submit_code(code, input=None)`
- `Session.close()`
- `ssn.id`, `ssn.attr`
- `SessionOutput.data`, `SessionOutput.text()`

Creation options are ignored when `ssn_id` is provided. `ssn.attr` is a read-only view of the effective creation settings; its type is intentionally not exported. `language` and `runtime` are create-time only. `close()` destroys the session. Reopen works only while the session is still open. `ResourceRequirement` and `FlameError` are imported from `flamepy`.

## Enums

- `SessionState.OPEN`, `SessionState.CLOSED`
- `TaskState.PENDING`, `TaskState.RUNNING`, `TaskState.SUCCEED`, `TaskState.FAILED`
- `ApplicationState.ENABLED`, `ApplicationState.DISABLED`
- `Shim.HOST`, `Shim.WASM`
- `FlameErrorCode.INVALID_CONFIG`, `INVALID_STATE`, `INVALID_ARGUMENT`, `INTERNAL`, `ALREADY_EXISTS`, `NOT_FOUND`

## Errors

SDK operations raise `FlameError` with a `code` and `message`:

```python
import flamepy

try:
    flamepy.connect("invalid://address")
except flamepy.FlameError as exc:
    print(exc.code, exc.message)
```

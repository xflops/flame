# Flame Python SDK API Reference

The `flamepy` package provides a synchronous client for Flame sessions and tasks, a service base class for host-shim applications, object-cache helpers, and the Runner API for packaging Python workloads.

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

## Runner API

Runner lives under `flamepy.runner`:

```python
from flamepy.runner import Runner

def add(a, b):
    return a + b

with Runner("add-app") as runner:
    svc = runner.service(add)
    print(svc(1, 2).get())
```

Key classes and helpers:

- `Runner(name, fail_if_exists=False)`
- `Runner.service(execution_object, autoscale=None, warmup=0, resreq=None)`
- `Runner.get(futures)`, `Runner.ref(futures)`, `Runner.wait(futures)`, `Runner.select(futures)`
- `ObjectFuture.get()`, `ObjectFuture.ref()`, `ObjectFuture.wait()`
- `get_data(data)` for decoding Runner task input/output payloads

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

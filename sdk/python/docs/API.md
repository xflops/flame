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
```

Environment variables override endpoint settings and fill TLS credentials with
neither a file nor embedded value configured:

- `FLAME_ENDPOINT`
- `FLAME_CACHE_ENDPOINT`
- `FLAME_CACHE_STORAGE`
- `FLAME_CA_FILE`
- `FLAME_CERT_FILE`
- `FLAME_KEY_FILE`

### Client TLS

`cluster.tls` and `cache.tls` accept the fields of
`flamepy.core.types.FlameClientTls`:

- `ca_file`, `cert_file`, `key_file`: paths to PEM credentials.
- `ca_data`, `cert_data`, `key_data`: single-line standard base64-encoded PEM.

Each credential accepts one source. A client identity requires both its
certificate and matching private key; CA-only TLS omits both. File and inline
sources can be mixed across credentials. Embedded credentials are decoded and
validated in memory, with server certificate and hostname verification enabled.
The same configuration works with synchronous and asynchronous clients.
See [Portable TLS Configuration](../../../docs/sdk/index.md#portable-tls-configuration)
for a complete YAML example and encoding instructions.

## Client API

Top-level helpers use the configured default connection:

```python
import flamepy

session = flamepy.create_session(
    "flmping",
    min_instances=1,
    resreq=flamepy.ResourceRequirement.from_string("cpu=1,mem=1g"),
)
output = session.run(b"hello")
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
- `run(input_data) -> bytes`
- `submit(input_data) -> TaskFuture`
- `close() -> None`
- `common_data() -> bytes | None`

`create_task()` expects bytes. `run()` blocks until the task completes and
returns its output. `submit()` returns immediately with a `TaskFuture`, a
`concurrent.futures.Future` subclass that resolves with the task output and
dispatches user completion callbacks away from the shared aio loop.

## Data Classes

`SessionAttributes`:

- `application: str`
- `id: str | None`
- `common_data: bytes | None`
- `min_instances: int` (long-lived instances to keep; not a gang)
- `max_instances: int | None`
- `batch_size: int` (gang size: executors are bound only once this many can be bound together; `1` is not a gang)
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

`flamepy.core` also exports:

- `ObjectKey`
- `WILDCARD_SESSION`
- `patch_object(ref, delta)`
- `upload_package(app_name, file_path, filename=None)`
- `download_package(app_name, filename, dest_path)`

`flamepy.cache` exposes object helpers without a deletion API. Objects remain
until server garbage collection removes data whose owning session is gone.
Packages use `<app>/pkg/<filename>` and remain tied to application lifetime;
uploading the same filename again appends another part. Package downloads
require one part, so choose a new filename for each package.

PUT and PATCH declare the exact encoded byte count in their write header. The
SDK derives this after serialization and compression, or from the opened file
for package uploads; callers do not supply a size. Zero declares an empty
payload. A truncated or oversized stream is rejected without committing a part.

Object references are versioned:

- `version=0` forces a fresh download.
- `version>=1` lets the client reuse its cached parts and fetch only the data after that version.
- The version is a committed part count. Parts are indexed from zero; they do not have independent versions.
- The client also tracks the object's UUID. If another client recreates the same key, the old parts are discarded and the new object is fetched in full, even if its part count is equal or greater.
- Without a deserializer, `get_object()` returns the newest data part of the object.
- With a deserializer, `get_object(ref, deserializer)` calls `deserializer(parts)` with all the data parts in the order they were written.

The cache protocol has three GET modes: `METADATA` returns only object metadata;
`OBJECT` returns the prefix before the requested part count (zero selects the
latest object); and `PATCHES` returns parts after that count. Metadata reads do
not advance the client's cached payload count. The regular `get_object()` API
chooses `OBJECT` for an initial read and `PATCHES` for subsequent reads.

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
- `FlameService.publish(keys)` and `retract(keys)` publish and withdraw data-aware scheduling keys
  for the next response; a `flamepy.serving.Instance` has them too.
- On the client, `flamepy.serving.Session.run(req, option=None)` and `submit(req, option=None)` accept
  `TaskOptions(affinity={key})` to prefer an executor that published `key`.

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
- `app.context()` returns an `app.InstanceContext` with the `session_id`, `task_id`
  and `batch_size` of the current App invocation. `batch_size` is the session's
  gang size: while the session has pending tasks, the scheduler binds its
  executors all at once or not at all. A task that blocks until `batch_size`
  members have joined, such as a collective `init_process_group`, can use it as
  the world size. It raises `RuntimeError` outside an invocation.
- `app.publish_attributes(attrs)` adds opaque `bytes` locality keys to the
  current App response; repeated calls in the response accumulate.
- `app.retract_attributes(attrs)` retracts keys that are no longer valid,
  such as an evicted cache entry. The last call on a key within a response wins.
  Session Manager adds published keys to the executor's retained set and
  removes retracted ones; retracting an absent key is harmless.
- `init(name, fail_if_exists=False, dependencies=None, python_version=None)`
  packages and initializes the application and returns its runtime handle.
  Repeating it for the same name returns the same handle. `dependencies`
  populates an auto-generated `pyproject.toml` only when the project has no
  existing Python package metadata. `python_version` selects the executor
  Python version through the application template.
- `app.service(autoscale=None, warmup=0, resreq=None, batch_size=1)` is the canonical service
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
  The retained object is executor-local: autoscaling may create one copy per
  executor. Use `autoscale=False, warmup=1` when mutable state must stay on one
  executor.
- `warmup` keeps that many instances alive (`min_instances`); it does not make
  them a gang. `batch_size` is the gang size: use `batch_size=N` for work that
  blocks until N members are present, together with `warmup=N` to keep them. For
  a gang `warmup` must be 0 or N, and `autoscale=False` with `warmup=N` also fixes
  the maximum at N; any other `min_instances` or `max_instances` is rejected
  when the session is created.
- Public class methods must not collide with `ServiceInstance` API names such
  as `close`; handle construction rejects such definitions.
- The handle returned by `app.init()` also exposes `service(...)` when direct
  runtime access is useful. A declaration made during another App invocation
  is the sole no-init exception; it reuses the current session.
- `ServiceInstance` is the client proxy returned for a decorated function or by
  calling a decorated class with its remote constructor arguments.
- A recursive declaration may call the service decorator without `init()` and
  does not call `destroy()`; it reuses and does not own the parent session.
  Nested submission works at any capacity. If the parent waits synchronously,
  the session needs enough free executor capacity to run the nested task.
- `get(futures)`, `ref(futures)`, `wait(futures)`, `select(futures)`
- `put(obj)` stores a shared object under the active application's cache prefix.
- `destroy()` closes services. If this process registered the application, it
  also unregisters it. Server garbage collection reclaims its objects and packages after their owning session or application is gone. With
  `fail_if_exists=False`, an existing application is borrowed and remains the
  user's lifecycle responsibility.
- `ObjectFuture.get()`, `ObjectFuture.ref()`, `ObjectFuture.wait()`

## Agent Session API

The agent `Session` lives under `flamepy.agent`. It runs remote Python or shell scripts in sandboxed containers through the built-in `flmbox` application, the same backend as `Sandbox`, without exposing the core `Session` or task JSON.

```python
from flamepy.agent import open_session
from flamepy import ResourceRequirement, FlameError

with open_session() as ssn:
    print(ssn.run_code("echo hello").stdout_text())
```

Key classes and methods:

- `open_session(*, ssn_id=None, image=None, min_instances=0, max_instances=None, resreq=None)`
- `open_session()` creates a session in the `flmbox` service's default image; pass `image` to choose the sandbox container image. To run Python, the image must provide it (for example `python:3.12-slim`)
- `open_session(ssn_id="...")` reopens the session with that ID, or creates it with the given options when it does not exist
- `Session.run_code(code, *, language="shell", cwd=None, envs=None, input=None, timeout=None) -> CommandResult` blocks for the result; `language` is `"shell"` (default) or `"python"`
- `Session.submit_code(code, *, language="shell", cwd=None, envs=None, input=None, timeout=None) -> Future[CommandResult]` returns immediately
- `Session.close()`
- `ssn.id`, `ssn.attr`
- `CommandResult.exit_code`, `.stdout`, `.stderr`, `.ok`, `.stdout_text()`, `.stderr_text()`. A nonzero exit is a normal result, not an exception.

Creation options apply only when the session is created: an existing session keeps its own, so they are ignored when `ssn_id` names one that exists. `ssn.attr` is a read-only view of the effective creation settings; its type is intentionally not exported. `image` is create-time only. `close()` destroys the session. Reopen works only while the session is still open. `ResourceRequirement` and `FlameError` are imported from `flamepy`.

## Sandbox API

`flamepy.agent.Sandbox` runs commands and scripts inside a container sandbox served by the built-in `flmbox` application.

```python
from flamepy.agent import Sandbox

with Sandbox.create(image="ubuntu:26.04") as box:
    result = box.run("echo hello", cwd="/tmp", envs={"A": "1"}, timeout=30)
    print(result.exit_code, result.stdout_text())
    print(box.run_code("print(1 + 2)", language="python").stdout_text())
```

- `Sandbox.create(*, sandbox_id=None, image=None, resreq=None)` creates a sandbox (one instance). A missing `sandbox_id` generates a `sandbox-<random>` id; an id that already exists is an error (use `Sandbox.connect`). A missing `image` uses the service default. `resreq` is the session resource requirement only; it is not part of the sandbox configuration.
- `Sandbox.connect(sandbox_id)` attaches to an existing sandbox; a session of another application raises `INVALID_ARGUMENT`.
- `box.id`, `box.state` (a fresh query returning `"open"`, `"suspending"`, `"suspended"` or `"closed"`)
- `box.run(cmd, *, cwd=None, envs=None, timeout=None) -> CommandResult` and `box.submit(...) -> Future[CommandResult]`
- `box.run_code(code, *, language="python", cwd=None, envs=None, input=None, timeout=None) -> CommandResult` and `box.submit_code(...)`; `language` is `"python"` or `"shell"`, `input` is bytes for stdin.
- `box.suspend(*, timeout=60.0)` requests suspension and polls until the state is `suspended`. The signal is asynchronous; `INTERNAL` is raised on timeout.
- `box.resume()` returns when the sandbox is `open` again. Phase 1 does NOT preserve process or filesystem state: a resumed sandbox starts fresh.
- `box.close()` is idempotent; the context manager closes on exit.
- A command is sent without asking the server for the state first. The server queues a command for a suspending or suspended sandbox and runs it after the resume, so `run` blocks (and `submit` stays pending) until then; check `box.state` first if that matters.
- `CommandResult.exit_code`, `.stdout`, `.stderr` (bytes), `.stdout_text()`, `.stderr_text()`, `.ok`. A nonzero exit code is a normal result, not an error.

Errors are `FlameError`: `INVALID_ARGUMENT` for bad arguments, `INVALID_STATE` when this handle is closed (after `close()`); a command for a sandbox closed through another handle is rejected by the server with a `FlameError` saying the session is closed, and `INTERNAL` for a malformed service response or a suspend timeout.

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

## Paged resource lists

Existing `list_applications`, `list_sessions`, `list_nodes`, `list_executors`, and
`Session.list_tasks` retain their return types and list all matching records.
Use the corresponding `Connection.list_*_page` methods for bounded queries:

```python
from flamepy import PageRequest, SessionState

page = connection.list_sessions_page(
    PageRequest(page_size=100, order_by="creation_time desc"),
    state=SessionState.OPEN,
    application="trainer",
)
for session in page.items:
    print(session.id)
print(page.paging.total_count, page.paging.state_counts)
```

`PageRequest()` selects 100 records; sizes above 1000 or below zero are rejected.
`page_number` is one-based; zero selects page 1 and negative numbers are rejected.
`PageInfo` contains the effective `page_number`, `page_size`, `total_pages`,
filtered `total_count`, overall `resource_count`, `state_counts`, and
resource-specific aggregate `totals`. Pages describe live data rather than a
frozen snapshot: concurrent insertions or deletions can shift later pages.

`iter_applications_pages`, `iter_sessions_pages`, `iter_nodes_pages`,
`iter_executors_pages`, and `iter_tasks_pages` fetch lazily. They preserve the
query and filters while incrementing page numbers and stop after the final page:

```python
for page in connection.iter_tasks_pages(session.id, PageRequest(page_size=200)):
    for task in page.items:
        print(task.id, task.state)
```

Applications and nodes accept `state` and `names`; sessions accept `state`, `ids`,
and `application`; executors accept `state`, `ids`, `application`, `node`, and
`session_id`. Tasks accept `state`, numeric task `ids`, and `include_payloads`.
Task pages omit input, output, affinity, and events by default; pass
`include_payloads=True` when these values are required. Session handles expose
`list_tasks_page` and `iter_tasks_pages` conveniences.

The same methods exist on `flamepy.core.aio.Connection` and asynchronous session
handles. Await individual page methods and use `async for` for page iterators.
Paged methods reject servers that do not support pagination. Task pages read
page information from the initial `flame-page-bin` gRPC metadata before consuming
the existing task stream, including when the page is empty.

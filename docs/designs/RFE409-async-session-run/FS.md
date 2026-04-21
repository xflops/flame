# Design: Enhance flamepy.session.run for Performance

**Status:** Draft  
**Author:** TBD  
**Created:** 2026-04-20  
**Issue:** [#409](https://github.com/xflops/flame/issues/409)

---

## 1. Motivation

### Background

The current implementation of `flamepy.session.run()` exhibits a blocking behavior that impacts system throughput. When invoked, the method occupies a thread from the `ThreadPoolExecutor` for the entire duration of the task lifecycle, which includes:

1. Task creation via gRPC (`create_task()`) — a blocking remote procedure call
2. Task completion monitoring via streaming gRPC (`watch_task()`) — blocks until terminal state is reached
3. Result extraction and return

The relevant implementation is shown below:

```python
# Current implementation (client.py:740-786)
def run(self, input_data: Any, informer: Optional[TaskInformer] = None) -> Future:
    return self.connection._executor.submit(self._invoke_impl, input_data, informer)

def _invoke_impl(self, input_data: Any, informer: Optional[TaskInformer] = None) -> Any:
    task = self.create_task(input_data)      # Blocking gRPC call
    watcher = self.watch_task(task.id)       # Returns streaming iterator
    for task in watcher:                      # Blocks until terminal state
        if task.is_completed():
            return task.output
```

This architecture presents several performance limitations:

1. **Thread pool contention**: For N concurrent tasks, N threads remain occupied for the entire task lifecycle, limiting parallelism to the thread pool size
2. **Sequential task creation**: Tasks are created serially within each worker thread, creating a bottleneck for high-throughput scenarios
3. **Resource inefficiency**: Worker threads spend the majority of their time in an idle waiting state on `watch_task()` streams

### Objectives

This enhancement aims to address the identified limitations through the following objectives:

1. **Decouple task creation from completion monitoring**: The `run()` method shall return immediately after task creation, deferring completion monitoring to the point of result retrieval
2. **Preserve API compatibility**: The existing `runner.ObjectFuture` interface (`ObjectFuture.get()`, `ObjectFuture.ref()`) shall remain unchanged
3. **Improve system throughput**: Enable creation of thousands of tasks without incurring blocking overhead on each individual completion
4. **Maintain backward compatibility**: Existing client code utilizing `invoke()` (synchronous) and `run()` (asynchronous) shall continue to function without modification

---

## 2. Function Specification

### Configuration

This enhancement does not introduce new configuration options. The implementation leverages existing `Session` and `Connection` infrastructure.

### API

#### Python SDK Modifications

**Session class (`flamepy/core/client.py`):**

| Method                         | Current Behavior                                              | Proposed Behavior                                                                                                   |
| ------------------------------ | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `run(input_data, informer)`    | Returns `Future` that occupies a thread until task completion | Returns `Future` immediately after task creation; completion monitoring is deferred to `Future.result()` invocation |
| `invoke(input_data, informer)` | Blocks until task completion                                  | Unchanged (implemented as `run().result()`)                                                                         |

**Internal class `_LazyTaskFuture`:**

This internal class extends `concurrent.futures.Future` with deferred completion monitoring. The public API returns the standard `Future` type to hide implementation details:

| Method                    | Description                                                                   |
| ------------------------- | ----------------------------------------------------------------------------- |
| `result(timeout=None)`    | Initiates task monitoring and blocks until completion; returns task output    |
| `done()`                  | Returns `True` if task has reached a terminal state                           |
| `cancelled()`             | Returns `False` (cancellation is not supported in the current implementation) |
| `exception(timeout=None)` | Returns the exception instance if the task failed; otherwise returns `None`   |

#### runner.ObjectFuture Compatibility

The existing `ObjectFuture` class wraps the `Future` returned by `Session.run()`:

```python
# runner/runner.py — no modifications required
class ObjectFuture:
    def __init__(self, future: Future):
        self._future = future

    def get(self) -> Any:
        result = self._future.result()  # Triggers deferred monitoring
        # ... decode ObjectRef and fetch object ...
```

The `ObjectFuture` implementation requires no modifications, as it already delegates to `Future.result()` for value retrieval.

### CLI

No command-line interface modifications are required for this enhancement.

### Scope

**In Scope:**

- Implementation of deferred task completion monitoring in `Session.run()`
- Introduction of internal `_LazyTaskFuture` class extending `concurrent.futures.Future`
- Public API returns standard `Future` type (hides implementation detail)
- Thread-safe completion state management
- Timeout support for `result()` and `exception()` methods

**Out of Scope:**

- Native Python async/await support (designated for future enhancement)
- Batch task creation API (designated for future enhancement)
- Modifications to gRPC protocol definitions
- Server-side implementation changes

**Limitations:**

- Task creation remains synchronous, incurring approximately 1-5ms latency per task
- Each `Future.result()` invocation establishes a new `watch_task` stream
- Task cancellation is not supported, consistent with existing behavior

### Feature Interaction

**Related Features:**

- **runner.ObjectFuture**: Encapsulates `Session.run()` result; depends on `Future.result()` for value retrieval
- **runner.ObjectFutureIterator**: Utilizes `concurrent.futures.as_completed()` for completion-order iteration
- **TaskInformer**: Callback interface for task progress notification; propagated to the deferred monitoring implementation

**Required Modifications:**

| Component                | Modification                                                                      |
| ------------------------ | --------------------------------------------------------------------------------- |
| `Session.run()`          | Return `Future` (internally uses `_LazyTaskFuture`) instead of `ThreadPoolExecutor.submit()` result |
| `Session._invoke_impl()` | Remove method (logic relocated to background `_watch()` closure)                  |
| `Session.invoke()`       | Simplify to `run().result()` invocation                                           |

**Compatibility:**

- Full backward compatibility is maintained
- Existing client code utilizing `run()` and `invoke()` functions without modification
- Standard library utilities `concurrent.futures.wait()` and `as_completed()` remain compatible with the new `Future` implementation

---

## 3. Implementation Detail

### Architecture

The following diagram illustrates the proposed architecture:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              Client Application                             │
│                                                                             │
│  futures = [session.run(data) for data in inputs]  # Returns immediately   │
│  results = [f.result() for f in futures]           # Waits on Event        │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                    ┌───────────────┴───────────────┐
                    ▼                               ▼
        ┌─────────────────────┐         ┌─────────────────────────────────┐
        │   Session.run()     │         │  _LazyTaskFuture (internal)     │
        │                     │         │                                 │
        │ 1. create_task()    │────────▶│ Contains:                       │
        │ 2. Return Future    │         │   - task_id, session            │
        │    immediately      │         │   - (inherited Future state)    │
        └─────────────────────┘         │                                 │
                                        │ On creation:                    │
                                        │   Submit _watch() to thread pool│
                                        │                                 │
                                        │ result():                       │
                                        │   Wait on _condition (inherited)│
                                        │   Return cached result          │
                                        └─────────────────────────────────┘
                                                      │
                                    ┌─────────────────┴─────────────────┐
                                    ▼                                   ▼
                        ┌─────────────────────┐             ┌─────────────────────┐
                        │  Background Thread  │             │   Caller Thread     │
                        │                     │             │                     │
                        │ _watch():           │             │ result():           │
                        │ 1. watch_task()     │             │ 1. Event.wait()     │
                        │ 2. Iterate stream   │────────────▶│ 2. Return _result   │
                        │ 3. Set _done_event  │  (notify)   │    or raise         │
                        └─────────────────────┘             └─────────────────────┘
```

**Behavioral Changes Summary:**

| Aspect                  | Original Implementation             | New Implementation                                    |
| ----------------------- | ----------------------------------- | ----------------------------------------------------- |
| `run()` return timing   | Blocks until task completion        | Returns immediately after task creation               |
| Thread pool utilization | One thread occupied per active task | One thread per task for background watching           |
| Monitoring execution    | In caller thread during `result()`  | Background thread; `result()` waits on Future         |
| `result()` blocking     | Blocks on gRPC stream directly      | Inherits from `concurrent.futures.Future`             |
| `done()` check          | Custom implementation               | Inherited from `Future` (thread-safe)                 |
| `add_done_callback`     | Not supported                       | Inherited from `Future`                               |
| `wait()/as_completed()` | Not compatible                      | Fully compatible (inherits from `Future`)             |

### Components

#### 1. \_LazyTaskFuture Class (Internal)

**Location:** `sdk/python/src/flamepy/core/client.py`

This is an internal implementation class. The public `Session.run()` API returns `Future` type.

```python
from concurrent.futures import Future

class _LazyTaskFuture(Future):
    """Internal: A Future that tracks a Flame task, compatible with concurrent.futures.wait()/as_completed()."""

    def __init__(
        self,
        session: "Session",
        task_id: TaskID,
    ):
        super().__init__()
        self._session = session
        self._task_id = task_id
```

By inheriting from `concurrent.futures.Future`, the class gains:
- Full compatibility with `wait()`, `as_completed()`, and other standard library utilities
- Thread-safe `result()`, `exception()`, `done()`, `cancelled()`, `running()` methods
- Proper `add_done_callback()` support
- Internal `_state`, `_condition`, `_waiters` attributes required by the standard library

#### 2. \_FutureTaskInformer Class

**Location:** `sdk/python/src/flamepy/core/client.py`

```python
class _FutureTaskInformer(TaskInformer):
    """TaskInformer that updates a Future when task state changes."""

    def __init__(self, future: Future):
        self._future = future

    def on_update(self, task: Task) -> None:
        if self._future.done():
            return
        if task.is_failed():
            for event in task.events:
                if event.code == TaskState.FAILED:
                    self._future.set_exception(FlameError(FlameErrorCode.INTERNAL, f"{event.message}"))
                    return
            self._future.set_exception(FlameError(FlameErrorCode.INTERNAL, "Task failed without error message"))
        elif task.is_completed():
            self._future.set_result(task.output)

    def on_error(self, error: FlameError) -> None:
        if not self._future.done():
            self._future.set_exception(error)
```

#### 3. Session.run() Modification

**Location:** `sdk/python/src/flamepy/core/client.py`

```python
class Session:
    def run(self, input_data: Any) -> Future:
        """Execute a task asynchronously and return a Future."""
        task = self.create_task(input_data)
        future = _LazyTaskFuture(self, task.id)
        future_informer = _FutureTaskInformer(future)

        def _watch():
            try:
                watcher = self.watch_task(task.id)
                for t in watcher:
                    future_informer.on_update(t)
                    if t.is_completed() or t.is_failed():
                        return
            except Exception as e:
                if isinstance(e, FlameError):
                    future_informer.on_error(e)
                else:
                    future_informer.on_error(FlameError(FlameErrorCode.INTERNAL, f"Watch failed: {str(e)}"))

        self.connection._executor.submit(_watch)
        return future

    def invoke(self, input_data: Any) -> Any:
        """Execute a task synchronously. Equivalent to run().result()."""
        return self.run(input_data).result()
```

#### 4. Deprecation of \_invoke_impl

The `_invoke_impl` method has been removed. Its functionality is now split between:
- Background `_watch()` closure in `Session.run()` - Task monitoring via gRPC stream
- `_FutureTaskInformer` - Updates the Future when task state changes
- `Future.result()` - Inherited from `concurrent.futures.Future`

### Data Structures

**\_LazyTaskFuture State (Internal):**

| Field          | Type                      | Description                                               |
| -------------- | ------------------------- | --------------------------------------------------------- |
| `_session`     | `Session`                 | Reference to the session (internal)                       |
| `_task_id`     | `TaskID`                  | Identifier of the task to monitor (internal)              |
| (inherited)    | `_condition`              | `threading.Condition` for synchronization                 |
| (inherited)    | `_state`                  | Future state: PENDING, RUNNING, FINISHED, etc.            |
| (inherited)    | `_result`                 | Result value after successful completion                  |
| (inherited)    | `_exception`              | Exception after failure                                   |
| (inherited)    | `_waiters`                | List of waiters for `wait()`/`as_completed()` support     |
| (inherited)    | `_done_callbacks`         | Callbacks registered via `add_done_callback()`            |

**Note:** Users interact with the standard `Future` interface. Internal fields `_session` and `_task_id` are not exposed.

### Algorithms

**Background Monitoring Flow:**

```
┌─────────────────────────────────────────────────────────────────┐
│ Client invokes session.run(input_data)                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Session.create_task(input_data)                                 │
│ [gRPC: CreateTask — blocking, typical latency: 1-5ms]           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Create _LazyTaskFuture(task_id) [internal]                      │
│   → Submit _watch() to ThreadPoolExecutor                       │
│   → Return Future to caller                                     │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┴─────────────────────┐
        ▼                                           ▼
┌───────────────────────┐               ┌───────────────────────┐
│   Background Thread   │               │   Caller Thread       │
│                       │               │                       │
│ _watch():             │               │ (continues execution) │
│ 1. watch_task()       │               │                       │
│ 2. for task in stream │               │ ... later ...         │
│ 3. if completed:      │               │                       │
│    future.set_result()│               │ future.result():      │
│                       │──────────────▶│ 1. Wait on _condition │
│                       │   (notify)    │ 2. return _result     │
└───────────────────────┘               └───────────────────────┘
```

**Key Design Points:**

1. **Immediate background watching**: The `_watch()` closure is submitted to the thread pool immediately when `run()` is called. This ensures task completion is detected as soon as possible.

2. **Inherits from `concurrent.futures.Future`**: By inheriting from the standard `Future` class, we get thread-safe `result()`, `done()`, `add_done_callback()`, and full compatibility with `wait()` and `as_completed()`.

3. **`_FutureTaskInformer` pattern**: The informer receives task state updates from the gRPC stream and calls `future.set_result()` or `future.set_exception()` to complete the future.

4. **Thread-safe by inheritance**: All synchronization is handled by the parent `Future` class using its internal `_condition` lock.

**Thread Safety Considerations:**

- `Future.done()` is thread-safe (uses internal `_condition` lock)
- `Future.result()` blocks on internal `_condition` until completion
- `Future.set_result()`/`set_exception()` notify all waiters atomically
- `add_done_callback()` handles both already-completed and pending cases

### System Considerations

**Performance Characteristics:**

| Metric                  | Original Implementation      | New Implementation                              |
| ----------------------- | ---------------------------- | ----------------------------------------------- |
| Task creation latency   | Included in `run()` duration | Included in `run()` duration (unchanged)        |
| `run()` return latency  | Equal to task lifetime       | ~1-5ms (task creation only)                     |
| Thread pool utilization | 1 thread per active task     | 1 thread per active task (for watching)         |
| `result()` blocking     | Blocks on gRPC stream        | Waits on `Future._condition` (efficient)        |
| `done()` overhead       | Custom implementation        | Inherited from `Future` (thread-safe)           |
| Memory per pending task | Thread stack (~1MB)          | Future object (~200 bytes) + watcher thread     |

**Scalability:**

- Task creation returns immediately, enabling rapid submission of large batches
- Background watching starts immediately, so results are ready sooner
- Thread pool shared across all futures for efficient resource utilization
- Memory footprint: Future object + background thread stack per active task

**Reliability:**

- No modification to task execution reliability characteristics
- Background thread handles all gRPC stream errors gracefully
- Exceptions are cached and re-raised on `result()` call
- Timeout support via `Future.result(timeout)` prevents indefinite blocking

**Resource Utilization:**

- Thread pool manages background watchers (default: 10 workers)
- gRPC watch streams established immediately on task creation
- Connection pooling behavior unchanged; reuses existing `Session` connection

**Observability:**

No additional logging or metrics instrumentation is required. Existing task creation and completion logging remains applicable.

### Dependencies

This enhancement introduces no new dependencies. The implementation utilizes existing components:

- `concurrent.futures` (Python standard library)
- `threading` (Python standard library)
- `grpc` (existing project dependency)

---

## 4. Use Cases

### Basic Use Cases

**Use Case 1: High-Throughput Task Submission**

**Description:** Submit a large batch of tasks with minimal blocking, then collect results.

**Current Implementation (suboptimal performance):**

```python
session = flamepy.create_session("my-app")

# Each run() invocation blocks until task completion
results = []
for i in range(1000):
    future = session.run(f"input-{i}".encode())
    results.append(future.result())  # Blocks for each task
```

**Proposed Implementation (improved performance):**

```python
session = flamepy.create_session("my-app")

# All run() invocations return immediately
futures = [session.run(f"input-{i}".encode()) for i in range(1000)]

# Result collection occurs separately
results = [f.result() for f in futures]
```

**Expected Outcome:**

- Task creation phase: ~5 seconds for 1000 tasks
- Result collection phase: Parallel monitoring as tasks complete

**Use Case 2: Runner Service Integration**

**Description:** Demonstrate that the existing runner API operates correctly without modification.

```python
from flamepy.runner import Runner

with Runner("my-app") as runner:
    service = runner.service(my_function)

    # Task submission (returns immediately with proposed implementation)
    futures = [service(arg) for arg in args]

    # Result retrieval (triggers deferred monitoring)
    results = runner.get(futures)
```

**Expected Outcome:** Identical API semantics with improved task submission throughput.

**Use Case 3: Completion-Order Processing**

**Description:** Process results in the order of task completion using standard library utilities.

```python
from concurrent.futures import as_completed

session = flamepy.create_session("my-app")
futures = [session.run(f"input-{i}".encode()) for i in range(100)]

# Process results as tasks complete
for future in as_completed(futures):
    try:
        result = future.result()
        process(result)
    except FlameError as e:
        handle_error(e)
```

**Expected Outcome:** Results are processed immediately upon individual task completion.

### Advanced Use Cases

**Use Case 4: Timeout-Bounded Execution**

**Description:** Enforce maximum wait duration for task completion.

```python
session = flamepy.create_session("my-app")
future = session.run(b"long-running-task-input")

try:
    result = future.result(timeout=30.0)  # Maximum 30-second wait
except TimeoutError:
    logger.warning("Task execution exceeded timeout threshold")
```

**Expected Outcome:** `TimeoutError` raised if task does not complete within the specified duration.

---

## 5. References

### Related Documents

- [RFE306 - Reorg Python SDK](../RFE306-Reorg-Python-SDK/FS.md)
- [RFE323 - Runner v2](../RFE323-runner-v2/FS.md)

### External References

- [Python concurrent.futures Documentation](https://docs.python.org/3/library/concurrent.futures.html)
- [gRPC Python Server-Side Streaming](https://grpc.io/docs/languages/python/basics/#server-side-streaming-rpc)

### Implementation References

| File                                      | Description                                   |
| ----------------------------------------- | --------------------------------------------- |
| `sdk/python/src/flamepy/core/client.py`   | Session and Connection class implementations  |
| `sdk/python/src/flamepy/runner/runner.py` | Runner and ObjectFuture class implementations |
| `sdk/python/src/flamepy/core/types.py`    | TaskState and TaskInformer definitions        |
| `rpc/protos/frontend.proto`               | WatchTask RPC specification                   |

---

## 6. Design Decisions

| Decision                                              | Rationale                                                                                                                         |
| ----------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **Inherit from `concurrent.futures.Future`**          | Provides full compatibility with `wait()`, `as_completed()`, and standard library utilities without reimplementing thread-safety  |
| **Background thread for watching**                    | Enables immediate task monitoring; results are ready as soon as task completes, not when `result()` is called                     |
| **Separate `_FutureTaskInformer` class**              | Clean separation of concerns; informer updates Future via public `set_result()`/`set_exception()` methods                         |
| **Synchronous task creation**                         | gRPC `CreateTask` latency is minimal (~1-5ms); keeping it synchronous simplifies the API                                          |
| **Remove informer parameter from run()/invoke()**     | Simplifies API; users can use `add_done_callback()` on the returned Future for progress monitoring                                |
| **No task cancellation support**                      | Consistent with existing implementation; server-side cancellation infrastructure not available                                    |

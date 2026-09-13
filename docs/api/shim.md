# Instance Service (Shim)

The Instance service defines the interface between executors and application instances. It handles session and task lifecycle events for the actual workload execution.

## Service Definition

```protobuf
service Instance {
  rpc OnSessionEnter(SessionContext) returns (OnSessionEnterResponse) {}
  rpc OnTaskInvoke(TaskContext) returns (OnTaskInvokeResponse) {}
  rpc OnSessionLeave(EmptyRequest) returns (Result) {}
}
```

## Overview

The Instance service is implemented by application shims that manage the actual workload. When an executor binds to a session, it calls these methods to notify the application of lifecycle events.

### Lifecycle Flow

```text
┌─────────────────────────────────────────────────────────────┐
│                     Executor Bound                          │
└─────────────────────────────────────────────────────────────┘
                            │
                            v
┌─────────────────────────────────────────────────────────────┐
│                   OnSessionEnter()                          │
│  - Receive application context                              │
│  - Initialize resources (DB connections, etc.)              │
│  - Load common_data if provided                             │
└─────────────────────────────────────────────────────────────┘
                            │
                            v
┌─────────────────────────────────────────────────────────────┐
│                   OnTaskInvoke() (repeated)                 │
│  - Receive task input                                       │
│  - Execute task logic                                       │
│  - Return task result                                       │
└─────────────────────────────────────────────────────────────┘
                            │
                            v
┌─────────────────────────────────────────────────────────────┐
│                   OnSessionLeave()                          │
│  - Clean up resources                                       │
│  - Close connections                                        │
└─────────────────────────────────────────────────────────────┘
                            │
                            v
┌─────────────────────────────────────────────────────────────┐
│                    Executor Unbound                         │
└─────────────────────────────────────────────────────────────┘
```

## Methods

### OnSessionEnter

Called when an executor binds to a session. Use this to initialize application-specific resources.

**Request:** `SessionContext`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session identifier |
| `application` | `ApplicationContext` | Application details |
| `common_data` | bytes | Shared data for all tasks in session (optional) |

**ApplicationContext:**

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Application name |
| `shim` | [Shim](types.md#shim) | Shim type (Host or Wasm) |
| `image` | string | Container/WASM image (optional) |
| `command` | string | Command to execute (optional) |
| `working_directory` | string | Working directory (optional) |
| `url` | string | Service URL (optional) |
| `installer` | string | Installer name (optional) |

**Response:** `OnSessionEnterResponse`

| Field | Type | Description |
|-------|------|-------------|
| `result` | [Result](types.md#result) | Session-enter result |
| `attributes` | `ExecutorAttributes` | Complete instance-attribute snapshot (optional) |

On success, SDK shims include `attributes`, including a present empty snapshot
that clears the previously accepted attributes. A failed session enter omits
the field and does not consume attributes accumulated by the publisher.

**Example Implementation (Python):**

```python
import json

import flamepy

class MyService(flamepy.FlameService):
    def on_session_enter(self, context):
        self.session_id = context.session_id
        self.db = Database.connect()  # Initialize resources
        common_data = context.common_data()
        if common_data:
            self.config = json.loads(common_data)
```

### OnTaskInvoke

Called for each task that needs to be executed.

**Request:** `TaskContext`

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Task identifier |
| `session_id` | string | Session identifier |
| `input` | bytes | Task input data (optional) |

**Response:** `OnTaskInvokeResponse`

| Field | Type | Description |
|-------|------|-------------|
| `task_result` | [TaskResult](types.md#taskresult) | Task result and optional output |
| `attributes` | `ExecutorAttributes` | Complete instance-attribute snapshot (optional) |

SDK shims include the current snapshot for both successful and failed task
invocations. A present empty snapshot clears the previously accepted
attributes.

**Example Implementation (Python):**

```python
def on_task_invoke(self, context):
    input_data = json.loads(context.input)
    result = self.process(input_data)
    return json.dumps(result).encode()
```

### OnSessionLeave

Called when the executor unbinds from the session. Use this to clean up resources.

**Request:** `EmptyRequest` (empty message)

**Response:** [Result](types.md#result)

**Example Implementation (Python):**

```python
def on_session_leave(self):
    self.db.close()  # Clean up resources
```

## Implementing a Shim

### Host Shim

For native applications running on the host:

```python
import flamepy

class MyApplication(flamepy.FlameService):
    def on_session_enter(self, context):
        # Initialize
        pass
    
    def on_task_invoke(self, context):
        # Process task
        return do_work(context.input)
    
    def on_session_leave(self):
        # Cleanup
        pass

if __name__ == "__main__":
    flamepy.run(MyApplication())
```

### Wasm Shim

For WebAssembly modules, the shim interfaces with the WASM runtime:

```rust
// The executor loads and calls the WASM module's exported functions
let module = WasmModule::load(application.image)?;

// OnSessionEnter
module.call("on_session_enter", session_context)?;

// OnTaskInvoke (for each task)
let result = module.call("on_task_invoke", task_context)?;

// OnSessionLeave
module.call("on_session_leave")?;
```

## Error Handling

Return non-zero `return_code` values to indicate failures:

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Invalid input |
| 3 | Resource unavailable |
| >0 | Application-specific error |

Errors are propagated back to the client through task status.

# Backend Service

The Backend service is used by executor managers to communicate with the Flame control plane. It handles node registration, executor lifecycle, and task execution.

## Service Definition

```protobuf
service Backend {
  // Node Management
  rpc RegisterNode(RegisterNodeRequest) returns (Result) {}
  rpc SyncNode(SyncNodeRequest) returns (SyncNodeResponse) {}  // Deprecated
  rpc ReleaseNode(ReleaseNodeRequest) returns (Result) {}
  rpc WatchNode(stream WatchNodeRequest) returns (stream WatchNodeResponse) {}

  // Executor Management
  rpc RegisterExecutor(RegisterExecutorRequest) returns (Result) {}
  rpc UnregisterExecutor(UnregisterExecutorRequest) returns (Result) {}

  // Session Binding
  rpc BindExecutor(BindExecutorRequest) returns (BindExecutorResponse) {}
  rpc BindExecutorCompleted(BindExecutorCompletedRequest) returns (Result) {}
  rpc UnbindExecutor(UnbindExecutorRequest) returns (Result) {}
  rpc UnbindExecutorCompleted(UnbindExecutorCompletedRequest) returns (Result) {}

  // Task Execution
  rpc LaunchTask(LaunchTaskRequest) returns (LaunchTaskResponse) {}
  rpc CompleteTask(CompleteTaskRequest) returns (Result) {}
}
```

## Node Management

### RegisterNode

Registers a node with the Flame control plane.

**Request:** `RegisterNodeRequest`

| Field | Type | Description |
|-------|------|-------------|
| `node` | [Node](types.md#node) | Node information |
| `executors` | [Executor](types.md#executor)[] | Current executors on this node for state alignment |

**Response:** [Result](types.md#result)

### WatchNode

Bidirectional streaming for node-executor synchronization. Replaces polling-based `SyncNode` with a server-push mechanism.

**Request:** `stream WatchNodeRequest`

| Field | Type | Description |
|-------|------|-------------|
| `heartbeat` | `NodeHeartbeat` | Heartbeat message with node status |

**NodeHeartbeat:**

| Field | Type | Description |
|-------|------|-------------|
| `node_name` | string | Node name |
| `status` | [NodeStatus](types.md#nodestatus) | Current node status |

**Response:** `stream WatchNodeResponse`

| Field | Type | Description |
|-------|------|-------------|
| `executor` | [Executor](types.md#executor) | Executor state update (oneof) |
| `ack` | `Acknowledgement` | Heartbeat acknowledgement (oneof) |

**Acknowledgement:**

| Field | Type | Description |
|-------|------|-------------|
| `timestamp` | int64 | Server acknowledgement timestamp |

### ReleaseNode

Removes an `Unknown` or `NotReady` node that has no retained executors. `Ready`
nodes and nodes with executor records of any state are rejected; missing nodes
return `NOT_FOUND`. This uses the same guarded storage release operation as
frontend `DeleteNode` and does not force-release executors.

**Request:** `ReleaseNodeRequest`

| Field | Type | Description |
|-------|------|-------------|
| `node_name` | string | Name of node to release |

**Response:** [Result](types.md#result)

### SyncNode (Deprecated)

> **Deprecated:** Use `WatchNode` streaming RPC instead for better efficiency.

**Request:** `SyncNodeRequest`

| Field | Type | Description |
|-------|------|-------------|
| `node` | [Node](types.md#node) | Node information |
| `executors` | [Executor](types.md#executor)[] | Current executors on node |

**Response:** `SyncNodeResponse`

| Field | Type | Description |
|-------|------|-------------|
| `node` | [Node](types.md#node) | Updated node information |
| `executors` | [Executor](types.md#executor)[] | Expected executor states |

## Executor Management

### RegisterExecutor

Registers a new executor with the control plane.

**Request:** `RegisterExecutorRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Unique executor identifier |
| `executor_spec` | [ExecutorSpec](types.md#executorspec) | Executor specification |

**Response:** [Result](types.md#result)

### UnregisterExecutor

Removes an executor from the control plane.

**Request:** `UnregisterExecutorRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor ID to unregister |

**Response:** [Result](types.md#result)

## Session Binding

The session binding workflow connects executors to sessions for task execution.

### Binding Workflow

```
Idle ──BindExecutor──> Binding ──BindExecutorCompleted──> Bound
                                                            │
                                                      UnbindExecutor
                                                            │
                                                            v
Idle <──UnbindExecutorCompleted── Unbinding <──────────────┘
```

### BindExecutor

Initiates binding an executor to a session.

**Request:** `BindExecutorRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor ID |

**Response:** `BindExecutorResponse`

| Field | Type | Description |
|-------|------|-------------|
| `application` | [Application](types.md#application) | Application to run (optional) |
| `session` | [Session](types.md#session) | Session to bind to (optional) |

### BindExecutorCompleted

Signals that executor binding is complete.

**Request:** `BindExecutorCompletedRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor ID |

**Response:** [Result](types.md#result)

### UnbindExecutor

Initiates unbinding an executor from its session.

**Request:** `UnbindExecutorRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor ID |

**Response:** [Result](types.md#result)

### UnbindExecutorCompleted

Signals that executor unbinding is complete.

**Request:** `UnbindExecutorCompletedRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor ID |

**Response:** [Result](types.md#result)

## Task Execution

### LaunchTask

Requests a task to execute.

**Request:** `LaunchTaskRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor requesting work |

**Response:** `LaunchTaskResponse`

| Field | Type | Description |
|-------|------|-------------|
| `task` | [Task](types.md#task) | Task to execute (optional, None if no tasks) |

### CompleteTask

Reports task completion.

**Request:** `CompleteTaskRequest`

| Field | Type | Description |
|-------|------|-------------|
| `executor_id` | string | Executor that completed the task |
| `task_result` | [TaskResult](types.md#taskresult) | Task execution result |

**Response:** [Result](types.md#result)

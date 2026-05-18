# Type Definitions

This document describes the common message types used across Flame's gRPC services.

> **Note on Timestamps**: Most timestamps in Flame use Unix milliseconds. However, `last_heartbeat_time` in `NodeStatus` uses Unix seconds for compatibility with Kubernetes conventions.

> **Note on Field Numbering**: Some message types (e.g., `SessionSpec`, `TaskSpec`) have field numbers starting at 2. This is intentional to maintain backward compatibility with earlier versions of the API where field 1 was reserved or removed.

## Core Types

### EmptyRequest

Empty request message used when no parameters are needed.

```protobuf
message EmptyRequest {}
```

### Metadata

Common metadata for all Flame objects.

```protobuf
message Metadata {
  string id = 1;
  string name = 2;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Unique identifier |
| `name` | string | Human-readable name |

### Result

Generic result message for operations.

```protobuf
message Result {
  int32 return_code = 1;
  optional string message = 2;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `return_code` | int32 | 0 for success, non-zero for error |
| `message` | string | Optional error or status message |

### Event

Event recorded during object lifecycle.

```protobuf
message Event {
  int32 code = 1;
  optional string message = 2;
  int64 creation_time = 3;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `code` | int32 | Event code |
| `message` | string | Optional event description |
| `creation_time` | int64 | Unix timestamp in milliseconds |

---

## Session Types

### Session

Represents a group of related tasks.

```protobuf
message Session {
  Metadata metadata = 1;
  SessionSpec spec = 2;
  SessionStatus status = 3;
}
```

### SessionSpec

Session configuration.

```protobuf
message SessionSpec {
  string application = 2;
  // Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse.
  reserved 3;
  reserved "slots";
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  uint32 batch_size = 7;
  uint32 priority = 8;
  optional ResourceRequirement resreq = 9;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `application` | string | Name of the application to run |
| `common_data` | bytes | Data shared across all tasks (optional) |
| `min_instances` | uint32 | Minimum executor instances (default: 0) |
| `max_instances` | uint32 | Maximum executor instances (optional, unlimited if not set) |
| `batch_size` | uint32 | Executors per batch for gang scheduling (default: 1) |
| `priority` | uint32 | Session priority; higher = more important (default: 0) |
| `resreq` | ResourceRequirement | Per-task resource request (optional); when absent, the server applies `cluster.resreq`, with a hardcoded `cpu=1,mem=1g,gpu=0` fallback. |

### SessionStatus

Current session state.

```protobuf
message SessionStatus {
  SessionState state = 1;
  int64 creation_time = 2;
  optional int64 completion_time = 3;
  int32 pending = 4;
  int32 running = 5;
  int32 succeed = 6;
  int32 failed = 7;
  int32 cancelled = 9;
  repeated Event events = 8;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `state` | SessionState | Current session state |
| `creation_time` | int64 | Session creation timestamp (ms) |
| `completion_time` | int64 | Session completion timestamp (ms, optional) |
| `pending` | int32 | Number of pending tasks |
| `running` | int32 | Number of running tasks |
| `succeed` | int32 | Number of successful tasks |
| `failed` | int32 | Number of failed tasks |
| `cancelled` | int32 | Number of cancelled tasks |
| `events` | Event[] | Session lifecycle events |

### SessionState

```protobuf
enum SessionState {
  Open = 0;
  Closed = 1;
}
```

| Value | Description |
|-------|-------------|
| `Open` | Session is accepting new tasks |
| `Closed` | Session is closed, no new tasks accepted |

### SessionList

```protobuf
message SessionList {
  repeated Session sessions = 1;
}
```

---

## Task Types

### Task

Represents a computing task.

```protobuf
message Task {
  Metadata metadata = 1;
  TaskSpec spec = 2;
  TaskStatus status = 3;
}
```

### TaskSpec

Task specification.

```protobuf
message TaskSpec {
  string session_id = 2;
  optional bytes input = 3;
  optional bytes output = 4;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Parent session ID |
| `input` | bytes | Task input data (optional) |
| `output` | bytes | Task output data (optional, set on completion) |

### TaskStatus

Current task state.

```protobuf
message TaskStatus {
  TaskState state = 1;
  int64 creation_time = 2;
  optional int64 completion_time = 3;
  repeated Event events = 4;
}
```

### TaskState

```protobuf
enum TaskState {
  Pending = 0;
  Running = 1;
  Succeed = 2;
  Failed = 3;
  Cancelled = 4;
}
```

| Value | Description |
|-------|-------------|
| `Pending` | Task is queued, waiting for execution |
| `Running` | Task is currently executing |
| `Succeed` | Task completed successfully |
| `Failed` | Task failed |
| `Cancelled` | Task was cancelled |

### TaskResult

Result of task execution.

```protobuf
message TaskResult {
  int32 return_code = 1;
  optional bytes output = 2;
  optional string message = 3;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `return_code` | int32 | 0 for success, non-zero for failure |
| `output` | bytes | Task output data (optional) |
| `message` | string | Error or status message (optional) |

---

## Application Types

### Application

Represents a registered application.

```protobuf
message Application {
  Metadata metadata = 1;
  ApplicationSpec spec = 2;
  ApplicationStatus status = 3;
}
```

### ApplicationSpec

Application configuration.

```protobuf
message ApplicationSpec {
  Shim shim = 1;
  optional string description = 2;
  repeated string labels = 3;
  optional string image = 4;
  optional string command = 5;
  repeated string arguments = 6;
  repeated Environment environments = 7;
  optional string working_directory = 8;
  optional uint32 max_instances = 9;
  optional int64 delay_release = 10;
  optional ApplicationSchema schema = 11;
  optional string url = 12;
  optional string installer = 13;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `shim` | Shim | Shim type (Host or Wasm) |
| `description` | string | Human-readable description |
| `labels` | string[] | Labels for filtering/organization |
| `image` | string | Container or WASM image |
| `command` | string | Command to execute |
| `arguments` | string[] | Command arguments |
| `environments` | Environment[] | Environment variables |
| `working_directory` | string | Working directory |
| `max_instances` | uint32 | Maximum concurrent instances |
| `delay_release` | int64 | Delay before releasing idle executors (ms) |
| `schema` | ApplicationSchema | Input/output schema definitions |
| `url` | string | Service URL for remote services |
| `installer` | string | Optional installer name used by the executor before launching the application |

### Shim

```protobuf
enum Shim {
  Host = 0;
  Wasm = 1;
}
```

| Value | Description |
|-------|-------------|
| `Host` | Native process on host |
| `Wasm` | WebAssembly module |

### ApplicationState

```protobuf
enum ApplicationState {
  Enabled = 0;
  Disabled = 1;
}
```

### ApplicationStatus

```protobuf
message ApplicationStatus {
  ApplicationState state = 1;
  int64 creation_time = 2;
}
```

### ApplicationSchema

Defines JSON schemas for validation.

```protobuf
message ApplicationSchema {
  optional string input = 1;
  optional string output = 2;
  optional string common_data = 3;
}
```

### Environment

```protobuf
message Environment {
  string name = 1;
  string value = 2;
}
```

### ApplicationList

```protobuf
message ApplicationList {
  repeated Application applications = 1;
}
```

---

## Executor Types

### Executor

Represents a task executor.

```protobuf
message Executor {
  Metadata metadata = 1;
  ExecutorSpec spec = 2;
  ExecutorStatus status = 3;
}
```

### ExecutorSpec

Executor specification.

```protobuf
message ExecutorSpec {
  string node = 1;
  ResourceRequirement resreq = 2;
  // Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse.
  reserved 3;
  reserved "slots";
  Shim shim = 4;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `node` | string | Node hosting this executor |
| `resreq` | ResourceRequirement | Resource requirements (cpu, memory, gpu) |
| `shim` | Shim | Supported shim type |

### ExecutorStatus

Current executor state.

```protobuf
message ExecutorStatus {
  ExecutorState state = 1;
  optional string session_id = 2;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `state` | ExecutorState | Current state |
| `session_id` | string | Bound session ID (optional) |

### ExecutorState

State machine for executor lifecycle:

```
void ──> idle ──> releasing ──> released
          ↑  │
          │  binding
  unbinding  │
          │  ↓
          bound
```

```protobuf
enum ExecutorState {
  ExecutorUnknown = 0;
  ExecutorVoid = 1;
  ExecutorIdle = 2;
  ExecutorBinding = 3;
  ExecutorBound = 4;
  ExecutorUnbinding = 5;
  ExecutorReleasing = 6;
  ExecutorReleased = 7;
}
```

| Value | Description |
|-------|-------------|
| `Void` | Initial state, not yet registered |
| `Idle` | Registered, waiting for session binding |
| `Binding` | Being bound to a session |
| `Bound` | Bound to a session, executing tasks |
| `Unbinding` | Being unbound from session |
| `Releasing` | Being released from cluster |
| `Released` | Fully released |

### ExecutorList

```protobuf
message ExecutorList {
  repeated Executor executors = 1;
}
```

---

## Node Types

### Node

Represents a cluster node.

```protobuf
message Node {
  Metadata metadata = 1;
  NodeSpec spec = 2;
  NodeStatus status = 3;
}
```

### NodeSpec

Node specification.

```protobuf
message NodeSpec {
  string hostname = 1;
}
```

### NodeStatus

Current node state.

```protobuf
message NodeStatus {
  NodeState state = 1;
  ResourceRequirement capacity = 2;
  ResourceRequirement allocatable = 3;
  NodeInfo info = 4;
  repeated NodeAddress addresses = 5;
  int64 last_heartbeat_time = 6;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `state` | NodeState | Current node state |
| `capacity` | ResourceRequirement | Total node resources |
| `allocatable` | ResourceRequirement | Available resources |
| `info` | NodeInfo | System information |
| `addresses` | NodeAddress[] | Network addresses |
| `last_heartbeat_time` | int64 | Last heartbeat timestamp (Unix seconds) |

### NodeState

```protobuf
enum NodeState {
  Unknown = 0;
  Ready = 1;
  NotReady = 2;
}
```

### NodeInfo

```protobuf
message NodeInfo {
  string arch = 1;
  string os = 2;
}
```

### NodeAddress

```protobuf
message NodeAddress {
  string type = 1;
  string address = 2;
}
```

| Type | Description |
|------|-------------|
| `InternalIP` | Internal cluster IP |
| `ExternalIP` | External/public IP |
| `Hostname` | DNS hostname |

### NodeList

```protobuf
message NodeList {
  repeated Node nodes = 1;
}
```

### GetNodeRequest

Request to get a specific node by name.

```protobuf
message GetNodeRequest {
  string name = 1;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Node name to retrieve |

### GetNodeResponse

Response containing the requested node.

```protobuf
message GetNodeResponse {
  Node node = 1;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `node` | Node | The requested node |

---

## Resource Types

### ResourceRequirement

Resource requirements or capacity.

```protobuf
message ResourceRequirement {
  uint64 cpu = 1;
  uint64 memory = 2;
  int32 gpu = 3;
}
```

| Field | Type | Description |
|-------|------|-------------|
| `cpu` | uint64 | Logical CPU units |
| `memory` | uint64 | Memory in bytes |
| `gpu` | int32 | Number of GPUs |

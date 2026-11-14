# Frontend Service

The Frontend service is the client-facing API for Flame. It handles session management, task operations, and application registration.

## Service Definition

```protobuf
service Frontend {
  // Application Management
  rpc RegisterApplication(RegisterApplicationRequest) returns (Result) {}
  rpc UnregisterApplication(UnregisterApplicationRequest) returns (Result) {}
  rpc UpdateApplication(UpdateApplicationRequest) returns (Result) {}
  rpc GetApplication(GetApplicationRequest) returns (Application) {}
  rpc ListApplications(ListApplicationsRequest) returns (ApplicationList) {}

  // Executor Listing
  rpc ListExecutors(ListExecutorsRequest) returns (ExecutorList) {}

  // Node Operations
  rpc ListNodes(ListNodesRequest) returns (NodeList) {}
  rpc GetNode(GetNodeRequest) returns (GetNodeResponse) {}
  rpc DeleteNode(DeleteNodeRequest) returns (Result) {}

  // Session Management
  rpc CreateSession(CreateSessionRequest) returns (Session) {}
  rpc DeleteSession(DeleteSessionRequest) returns (Session) {}
  rpc OpenSession(OpenSessionRequest) returns (Session) {}
  rpc CloseSession(CloseSessionRequest) returns (Session) {}
  rpc SuspendSession(SuspendSessionRequest) returns (Session) {}
  rpc ResumeSession(ResumeSessionRequest) returns (Session) {}
  rpc GetSession(GetSessionRequest) returns (Session) {}
  rpc ListSessions(ListSessionsRequest) returns (SessionList) {}

  // Task Operations
  rpc CreateTask(CreateTaskRequest) returns (Task) {}
  rpc GetTask(GetTaskRequest) returns (Task) {}
  rpc WatchTasks(stream WatchTaskRequest) returns (stream Task) {}
  rpc ListTasks(ListTasksRequest) returns (stream Task) {}
}
```

## Application Management

### RegisterApplication

Registers a new application with Flame.

**Request:** `RegisterApplicationRequest`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Unique name for the application |
| `application` | [ApplicationSpec](types.md#applicationspec) | Application specification |

**Response:** [Result](types.md#result)

**Example:**
```python
import flamepy

flamepy.register_application("my-app", {
    "shim": flamepy.Shim.HOST,
    "image": "my-registry/my-app:latest",
    "command": "/usr/bin/my-app",
})
```

### UnregisterApplication

Removes an application registration.

**Request:** `UnregisterApplicationRequest`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Name of the application to unregister |

**Response:** [Result](types.md#result)

### UpdateApplication

Updates an existing application registration.

**Request:** `UpdateApplicationRequest`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Name of the application to update |
| `application` | [ApplicationSpec](types.md#applicationspec) | Replacement application specification |

**Response:** [Result](types.md#result)

### GetApplication

Retrieves application details by name.

**Request:** `GetApplicationRequest`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Application name |

**Response:** [Application](types.md#application)

### ListApplications

Lists registered applications, optionally filtered by state.

**Request:** `ListApplicationsRequest`

| Field | Type | Description |
|-------|------|-------------|
| `state` | optional [ApplicationState](types.md#applicationstate) | Application state filter |

**Response:** [ApplicationList](types.md#applicationlist)

## Session Management

### CreateSession

Creates a new session for task execution.

**Request:** `CreateSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Unique identifier for the session |
| `session` | [SessionSpec](types.md#sessionspec) | Session specification |

**Response:** [Session](types.md#session)

**Example:**
```python
import flamepy

session = flamepy.create_session(
    application="my-app",
    resreq=flamepy.ResourceRequirement.from_string("cpu=1,mem=1g"),
    min_instances=2,
    max_instances=10,
)
```

### DeleteSession

Deletes a session and its persisted task records.

**Request:** `DeleteSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID to delete |

**Response:** [Session](types.md#session)

### OpenSession

Opens an existing session or creates one if spec is provided.

**Request:** `OpenSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID to open |
| `session` | [SessionSpec](types.md#sessionspec) | Optional spec for creation |

**Response:** [Session](types.md#session)

### CloseSession

Closes a session, preventing new task submissions. Close rejects sessions with
running tasks and cancels remaining pending tasks. The response includes final
accurate task counters for every storage backend, including `none`; repeating a
successful close preserves those counters. A successful close removes all
session and task events; events are retained while the session remains open.

**Request:** `CloseSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID to close |

**Response:** [Session](types.md#session)

### SuspendSession

Suspends an `Open` session. The session moves to `Suspending`: no new task is
launched, running tasks finish normally, and the session's executors unbind, after
which the session becomes `Suspended` (the scheduler completes it in its next cycle).
The returned session is always `Suspending`. Tasks can still be submitted
to a suspending or suspended session; they stay `Pending` until the session is
resumed. Suspending a session that is already `Suspending` or `Suspended`, or a
`Closed` session, fails with `INVALID_ARGUMENT`.

**Request:** `SuspendSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID to suspend |

**Response:** [Session](types.md#session)

### ResumeSession

Resumes a `Suspended` session, moving it back to `Open` so that pending tasks are
scheduled again. Resuming an `Open` session is a no-op that succeeds. Other states
(`Suspending`, `Closed`) fail with `INVALID_ARGUMENT`.

**Request:** `ResumeSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID to resume |

**Response:** [Session](types.md#session)

### GetSession

Retrieves session details.

**Request:** `GetSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID |

**Response:** [Session](types.md#session)

### ListSessions

Lists sessions, optionally filtered by application and state.

**Request:** `ListSessionsRequest`

| Field | Type | Description |
|-------|------|-------------|
| `application` | optional string | Application name filter |
| `state` | optional [SessionState](types.md#sessionstate) | Session state filter |

**Response:** [SessionList](types.md#sessionlist)

## Task Operations

### CreateTask

Creates a new task within a session.

**Request:** `CreateTaskRequest`

| Field | Type | Description |
|-------|------|-------------|
| `task` | [TaskSpec](types.md#taskspec) | Task specification |

**Response:** [Task](types.md#task)

**Example:**
```python
task = session.create_task(b"input data")
```

### GetTask

Retrieves task details.

Finished tasks remain readable from the selected storage engine after they
leave the session manager's active task cache. Filesystem retains
records on disk; `none` retains all task records and results in memory for the
life of its engine instance. Closing a session or evicting it from the scheduling
cache does not delete engine task history. Explicit session deletion removes
that history; a new `none` engine instance starts empty after restart. Session
status counters include finished tasks for every backend.

**Request:** `GetTaskRequest`

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Task ID |
| `session_id` | string | Session ID containing the task |

**Response:** [Task](types.md#task)

### WatchTasks

Registers task IDs on one bidirectional stream and returns status updates for
those tasks. The first response for each registered task is its current status,
which may already be terminal. The stream then sends later status updates until
the task reaches a terminal state. Intermediate updates may be coalesced under
load, so callers should use the latest received status rather than expect every
transition. All registrations on a stream must use the same session ID.
Closing the request side after registration still allows outstanding task
updates to arrive.

**Request:** `stream WatchTaskRequest`

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Task ID to register |
| `session_id` | string | Session ID |

**Response:** `stream` [Task](types.md#task)

**Example:**
```python
for update in session.watch_task(task.id):
    # The first update is the current status, not necessarily Pending.
    print(f"State: {update.state}")
    if update.is_completed():
        break
```

### ListTasks

Streams all active tasks copied from the session cache, followed by all
finished tasks read from the storage engine, with no limit or pagination.
Task input/output is preserved. A task that finishes between the cache snapshot
and storage read appears once, using the active snapshot already selected.
These reads are not a transactional snapshot: concurrent creation and state
changes can affect which tasks and states are returned. No task-ID ordering is
promised. This includes completed history retained in memory by `none` storage.

**Request:** `ListTasksRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID |

**Response:** `stream` [Task](types.md#task)

## Node Operations

### ListNodes

Lists all registered nodes in the cluster.

**Request:** `ListNodesRequest` (empty)

**Response:** [NodeList](types.md#nodelist)

### GetNode

Retrieves details for a specific node.

**Request:** `GetNodeRequest`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Node name |

**Response:** `GetNodeResponse`

| Field | Type | Description |
|-------|------|-------------|
| `node` | [Node](types.md#node) | Node details |

### DeleteNode

Deletes one explicitly named inactive node. The node must be `Unknown` or
`NotReady` and have no retained executors. A `Ready` node or a node with any
executor record is rejected; the operation does not force-release executors.
Missing nodes return `NOT_FOUND`. Successful deletion is logged by the server.
Nodes are removed only on explicit request; there is no automatic expiry.

**Request:** `DeleteNodeRequest`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Required node name |

**Response:** [Result](types.md#result)

**CLI:**

```sh
flmctl delete --node worker-a
flmctl delete -n worker-a,worker-b
flmctl delete -n                    # Every NotReady node
```

`--node` accepts comma-separated or space-separated names and cannot be combined
with `--session`. With no names, it selects all nodes whose state is `NotReady`;
`Unknown` nodes are excluded from this bulk selection while they are draining.
Explicitly named `Unknown` nodes remain eligible when they have no executors.
Review `flmctl list --node` before selecting nodes.

Each selected node is validated by the server at deletion time. A node whose
executor records remain registered cannot be deleted, even if it is `NotReady`.
Batch deletion continues after individual failures, reports deleted/failed
counts, and exits unsuccessfully if any deletion failed.

Existing session deletion is unchanged:

```sh
flmctl delete -s                    # Every closed session
flmctl delete -s session-a,session-b
```

## Executor Operations

### ListExecutors

Lists all executors in the cluster.

**Request:** `ListExecutorsRequest` (empty)

**Response:** [ExecutorList](types.md#executorlist)

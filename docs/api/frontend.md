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

  // Session Management
  rpc CreateSession(CreateSessionRequest) returns (Session) {}
  rpc DeleteSession(DeleteSessionRequest) returns (Session) {}
  rpc OpenSession(OpenSessionRequest) returns (Session) {}
  rpc CloseSession(CloseSessionRequest) returns (Session) {}
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

Closes a session, preventing new task submissions.

**Request:** `CloseSessionRequest`

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Session ID to close |

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

Streams all tasks in a session.

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

## Executor Operations

### ListExecutors

Lists all executors in the cluster.

**Request:** `ListExecutorsRequest` (empty)

**Response:** [ExecutorList](types.md#executorlist)

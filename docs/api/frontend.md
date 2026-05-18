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
  rpc ListApplication(ListApplicationRequest) returns (ApplicationList) {}

  // Executor Listing
  rpc ListExecutor(ListExecutorRequest) returns (ExecutorList) {}

  // Node Operations
  rpc ListNodes(ListNodesRequest) returns (NodeList) {}
  rpc GetNode(GetNodeRequest) returns (GetNodeResponse) {}

  // Session Management
  rpc CreateSession(CreateSessionRequest) returns (Session) {}
  rpc DeleteSession(DeleteSessionRequest) returns (Session) {}
  rpc OpenSession(OpenSessionRequest) returns (Session) {}
  rpc CloseSession(CloseSessionRequest) returns (Session) {}
  rpc GetSession(GetSessionRequest) returns (Session) {}
  rpc ListSession(ListSessionRequest) returns (SessionList) {}

  // Task Operations
  rpc CreateTask(CreateTaskRequest) returns (Task) {}
  rpc GetTask(GetTaskRequest) returns (Task) {}
  rpc WatchTask(WatchTaskRequest) returns (stream Task) {}
  rpc ListTask(ListTaskRequest) returns (stream Task) {}
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

### ListApplication

Lists all registered applications.

**Request:** `ListApplicationRequest` (empty)

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
    batch_size=1
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

### ListSession

Lists all sessions.

**Request:** `ListSessionRequest` (empty)

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

### WatchTask

Streams task status updates until completion.

**Request:** `WatchTaskRequest`

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Task ID to watch |
| `session_id` | string | Session ID |

**Response:** `stream` [Task](types.md#task)

**Example:**
```python
for update in session.watch_task(task.id):
    print(f"State: {update.state}")
    if update.is_completed():
        break
```

### ListTask

Streams all tasks in a session.

**Request:** `ListTaskRequest`

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

### ListExecutor

Lists all executors in the cluster.

**Request:** `ListExecutorRequest` (empty)

**Response:** [ExecutorList](types.md#executorlist)

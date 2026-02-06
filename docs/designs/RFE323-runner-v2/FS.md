# RFE323: Enhanced Runner Service Configuration

## 1. Motivation

**Background:**

Currently, `Runner.service()` accepts a `kind` parameter of type `RunnerServiceKind` to configure whether a service is stateful or stateless. When the `kind` parameter is not explicitly provided, a default value is determined automatically based on the execution object type (functions/builtins are stateless, everything else is stateful). However, this simple model has significant limitations:

1. **Instance Count Control**: There's no way to control how many instances of a service should be created. Some use cases require a single long-lived service instance (e.g., a database connection pool), while others benefit from multiple instances that can be created dynamically based on workload (e.g., stateless request handlers).

2. **Conflated Concerns**: The binary `RunnerServiceKind` (stateful/stateless) conflates two separate concerns: state persistence and instance scaling. A service might need state persistence but also benefit from multiple instances, or might be stateless but require a single instance for resource management (e.g., connection pools). This design replaces `RunnerServiceKind` with explicit `stateful` and `autoscale` parameters to decouple these concerns.

3. **Inefficient Execution Model**: Currently, the execution object is loaded on every task invocation (`on_task_invoke`), which is inefficient for stateful services that should maintain state across multiple tasks.

4. **Class Instantiation Timing**: When a class is passed to `Runner.service()`, it's instantiated immediately in the client code. This means the class state is serialized and sent to executors, which may not be the desired behavior. For better distribution, the class itself should be serialized, and instances should be created on each executor.

**Target:**

This design aims to enhance `Runner.service()` configuration to achieve:

1. **Explicit State Control**: Allow users to explicitly specify whether a service should persist state, independent of the execution object type.

2. **Autoscaling Support**: Enable services to scale automatically based on pending tasks, or maintain a single instance for services that require it.

3. **Efficient Execution**: Load execution objects once per session rather than per task, improving performance.

4. **Better Distribution**: Serialize classes rather than instances, allowing each executor to create its own instance.

5. **Sensible Defaults**: Maintain intelligent defaults based on execution object type to minimize required configuration.

## 2. Function Specification

### Configuration

**Runner.service() Parameters:**

The `Runner.service()` method will be enhanced with the following parameters:

```python
def service(
    self, 
    execution_object: Any, 
    stateful: Optional[bool] = None,
    autoscale: Optional[bool] = None
) -> RunnerService:
    """Create a RunnerService for the given execution object.
    
    Args:
        execution_object: A function, class, or class instance to expose as a service
        stateful: If True, persist the execution object state back to flame-cache
                 after each task. If False, do not persist state. If None, use default
                 based on execution_object type.
        autoscale: If True, create instances dynamically based on pending tasks (min=0, max=None).
                  If False, create exactly one instance (min=1, max=1).
                  If None, use default based on execution_object type.
    
    Returns:
        A RunnerService instance
    """
```

**Default Values:**

When `stateful` or `autoscale` are not explicitly set (None), the following defaults apply:

| Execution Object Type   | stateful (default) | autoscale (default) |
| ----------------------- | ------------------ | ------------------- |
| Function                | False              | True                |
| Class                   | False              | False               |
| Class Instance (object) | False              | False               |

**Rationale for Defaults:**
- **Functions**: Typically stateless and benefit from autoscaling (e.g., request handlers)
- **Classes**: Users usually want a single instance per executor; state management is explicit
- **Objects**: Already instantiated, typically represent a single stateful entity

**Configuration Validation:**

The following validations will be enforced:
- If `execution_object` is a class and `stateful=True`, an error will be raised (classes themselves cannot maintain state; only instances can)
- If `stateful=True`, the execution object must be pickleable (will be validated at runtime)

### API

**RunnerContext Structure:**

The `RunnerContext` dataclass will be updated to replace `kind` with `stateful` and `autoscale` fields:

```python
@dataclass
class RunnerContext:
    """Context for runner session containing the shared execution object.
    
    Attributes:
        execution_object: The execution object for the session. Can be:
                         - A function (callable)
                         - A class (will be instantiated in on_session_enter)
                         - A class instance (already instantiated object)
        stateful: Whether to persist the execution object state back to cache
        autoscale: Whether to autoscale instances based on pending tasks
        min_instances: Minimum number of instances (derived from autoscale)
        max_instances: Maximum number of instances (derived from autoscale)
    """
    execution_object: Any
    stateful: bool = False
    autoscale: bool = True
    min_instances: int = field(init=False)
    max_instances: Optional[int] = field(init=False)
    
    def __post_init__(self) -> None:
        # Set min/max instances based on autoscale
        if self.autoscale:
            self.min_instances = 0
            self.max_instances = None  # No limit
        else:
            self.min_instances = 1
            self.max_instances = 1
```

**Session Configuration:**

The session manager will use `min_instances` and `max_instances` from `RunnerContext` to control executor allocation:
- `min_instances`: Minimum number of executors to keep alive for this session
- `max_instances`: Maximum number of executors allowed for this session (None = unlimited)

### CLI

**flmctl session view command:**

The `flmctl view -s <session_id>` command should display the `min_instances` and `max_instances` configuration for each session. This helps administrators understand the scaling behavior of running sessions.

Example output:
```bash
$ flmctl view -s sess_abc123
Session ID: sess_abc123
Application: my-runner-app
Status: Running
Min Instances: 0
Max Instances: unlimited
...
```

For sessions with `autoscale=False`:
```bash
$ flmctl view -s sess_xyz789
Session ID: sess_xyz789
Application: stateful-service
Status: Running
Min Instances: 1
Max Instances: 1
...
```

### Other Interfaces

**Session Manager Interface (RPC API):**

The `SessionSpec` message in the RPC API (`rpc/protos/types.proto`) needs to include `min_instances` and `max_instances` fields:

```protobuf
message SessionSpec {
  string application = 2;
  uint32 slots = 3;
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;  // NULL means unlimited
}
```

**Session Creation Flow:**
1. **Python SDK (runner.py)**: Creates `RunnerContext` with `stateful`, `autoscale`, `min_instances`, `max_instances`
2. **Python SDK (runner.py)**: Calls `create_session()` with `CreateSessionRequest` containing `SessionSpec`
3. **Python SDK (client.py)**: Populates `SessionSpec.min_instances` and `SessionSpec.max_instances` from `RunnerContext.min_instances` and `RunnerContext.max_instances`
4. **Session Manager**: Receives `CreateSessionRequest`, reads `min_instances` and `max_instances` directly from `SessionSpec`
5. **Session Manager**: Applies fallback - if `max_instances` is None, uses `Application.max_instances`
6. **Session Manager**: Validates that `min_instances <= max_instances` (if max_instances is not None)
7. **Session Manager**: Inserts session into database table with `min_instances` and effective `max_instances` columns
8. **Session Manager**: Creates internal `Session` struct and returns (executor allocation is asynchronous)
9. **Scheduler Loop** (runs every ~1 second): Checks if sessions are underused via FairShare plugin
10. **Scheduler**: If underused (including when `allocated < min_instances`), allocates executors via AllocateAction
11. **Scheduler**: Continues until session reaches `deserved` allocation (which is >= min_instances * slots)

**Session Loading Flow (Recovery after restart):**
1. **Session Manager**: Loads sessions from database (e.g., after session manager restart)
2. **Session Manager**: Reads all fields including `min_instances` and `max_instances` from session table
3. **Session Manager**: Constructs `SessionSpec` protobuf message with values from database
4. **Session Manager**: Sets `SessionSpec.min_instances` from table's `min_instances` column
5. **Session Manager**: Sets `SessionSpec.max_instances` from table's `max_instances` column (NULL maps to None)
6. **Session Manager**: Constructs `Session` protobuf message with populated `SessionSpec`
7. **Scheduler**: Restores session state and ensures `min_instances` executors are allocated
8. **Scheduler**: Resumes normal scheduling based on `min_instances` and `max_instances` configuration

**Validation Rules:**
- `min_instances` must be >= 0
- If `max_instances` is `Some(value)`, then `min_instances <= value`
- If validation fails, return an error when creating the session

**SDK Client Interface:**
- **Python SDK (runner.py)**: When calling `create_session()`, populates `SessionSpec` in `CreateSessionRequest`
- **Python SDK**: Sets `SessionSpec.min_instances = RunnerContext.min_instances` (as uint32)
- **Python SDK**: Sets `SessionSpec.max_instances = RunnerContext.max_instances` (as optional uint32, None means unlimited)
- **Python SDK**: Includes serialized `RunnerContext` in `SessionSpec.common_data` for use by executors
- The protobuf-generated `SessionSpec` structure is used across all SDKs (Python, Rust, Go)
- All language SDKs use the same `SessionSpec` message format from `rpc/protos/types.proto`

**Session Query Interface (flmctl):**
- `flmctl view -s <session_id>` reads `min_instances` and `max_instances` from `SessionSpec` in the session table
- Display these values in the command output

**Executor Manager Interface (Internal):**

No changes required. The executor manager continues to handle executor lifecycle as directed by the session manager.

### Scope

**In Scope:**
- Add `stateful` and `autoscale` parameters to `Runner.service()`
- Update `RunnerContext` to include `stateful`, `autoscale`, `min_instances`, `max_instances`
- Remove `RunnerServiceKind` enum and `kind` parameter from `Runner.service()`
- Move execution object loading from `on_task_invoke` to `on_session_enter` in `FlameRunpyService`
- Implement class instantiation in `on_session_enter` (if execution object is a class)
- Persist execution object state back to cache after each task if `stateful=True`
- Update `Runner.service()` to serialize classes (not instances) when a class is provided
- Update `SessionSpec` protobuf message to add `min_instances` and `max_instances` fields
- Regenerate protobuf bindings for all supported languages
- Create database migration `20260123000000_add_session_instances.sql`
- Update session table schema to add `min_instances` and `max_instances` columns
- Update `SessionDao` struct and `TryFrom` implementation
- Update internal `Session` struct in `common/src/apis.rs`
- Update `SessionInfo` struct in `session_manager/src/model/mod.rs`
- Update INSERT/SELECT queries in `sqlite.rs` to handle new columns
- Implement validation in session creation to ensure `min_instances <= max_instances`
- Implement fallback logic: if `SessionSpec.max_instances` is None, use `Application.max_instances`
- Persist `min_instances` and `max_instances` in session table for durability
- Update fairshare scheduler plugin to:
  - Respect session's `min_instances` and `max_instances`
  - Ensure `desired >= min_instances * slots` (minimum guarantee)
  - Cap allocation at `max_instances` (already includes app limit from creation)
  - Update `SSNInfo` struct with new fields
- Update `AllocateAction` scheduler action to:
  - Add explicit `max_instances` check before creating executors
  - Prevent over-allocation when multiple executors are created in one scheduler cycle
- Update `flmctl view -s` command to display `min_instances` and `max_instances` from session table

**Out of Scope:**
- Advanced autoscaling policies (e.g., based on CPU/memory metrics)
- Custom scaling rules (e.g., scale by time of day)
- Load balancing algorithms (continue to use existing session manager logic)
- Dynamic adjustment of min/max instances after session creation

**Limitations:**
- Autoscaling is reactive (based on pending tasks), not predictive
- No support for scaling down executors (min_instances executors remain allocated)
- State persistence is all-or-nothing (cannot persist partial state)
- Class instantiation uses default constructor only (no custom initialization parameters)
- `min_instances` and `max_instances` are derived from `autoscale` and cannot be set independently

### Feature Interaction

**Related Features:**
- **RPC API (protobuf)**: Session configuration is part of the core RPC API interface
- **Session Management**: Session manager must respect min/max instances when allocating executors
- **Executor Management**: Executor lifecycle is controlled by session manager based on instance limits
- **Object Cache**: Stateful services use cache to persist execution object state
- **RL Module**: Uses RunnerService for remote execution of Python code

**Updates Required:**

1. **runner.py**:
   - Update `Runner.service()` signature to accept `stateful` and `autoscale` parameters
   - Remove class instantiation logic (keep class as-is, don't instantiate)
   - Update `RunnerService.__init__()` to pass new parameters to `RunnerContext`
   - Update default logic to determine `stateful` and `autoscale` when not specified
   - When calling `create_session()`, populate `SessionSpec.min_instances` and `SessionSpec.max_instances` from `RunnerContext.min_instances` and `RunnerContext.max_instances`

2. **runpy.py**:
   - Move execution object loading from `on_task_invoke` to `on_session_enter`
   - Store execution object as instance variable (`self._execution_object`)
   - In `on_session_enter`, if execution object is a class, instantiate it
   - In `on_task_invoke`, use `self._execution_object` instead of loading from common data
   - After task execution, if `stateful=True`, persist updated execution object to cache
   - Update common data handling to store both execution object and configuration

3. **types.py**:
   - Update `RunnerContext` dataclass with new fields
   - Remove `RunnerServiceKind` enum entirely
   - Add `__post_init__` logic to compute `min_instances` and `max_instances`

4. **rpc/protos/types.proto**:
   - Update `SessionSpec` message to add `min_instances` (uint32) and `max_instances` (optional uint32) fields
   - Regenerate protobuf bindings for all languages (Rust, Python)

5. **session_manager (Rust)**:
   
   **a. Database Migration:**
   - Create `session_manager/migrations/sqlite/20260123000000_add_session_instances.sql`
   - Add `min_instances INTEGER NOT NULL DEFAULT 0` column
   - Add `max_instances INTEGER` column (NULL means unlimited)
   
   **b. Storage Layer (`session_manager/src/storage/engine/`):**
   - Update `SessionDao` struct in `types.rs`:
     - Add `min_instances: i64` field
     - Add `max_instances: Option<i64>` field
   - Update `TryFrom<&SessionDao> for Session` impl in `types.rs`:
     - Map `min_instances` from i64 to u32
     - Map `max_instances` from Option<i64> to Option<u32>
   - Update INSERT query in `sqlite.rs` (around line 384):
     - Current: `INSERT INTO sessions (id, application, slots, common_data, creation_time, state)`
     - Updated: `INSERT INTO sessions (id, application, slots, common_data, creation_time, state, min_instances, max_instances)`
     - Add `.bind(min_instances)` and `.bind(max_instances)` to query
   - Update SELECT queries to include new columns (SQLx should handle this automatically via `SessionDao`)
   
   **c. Internal Session Struct (`common/src/apis.rs`):**
   - Update `Session` struct:
     - Add `min_instances: u32` field
     - Add `max_instances: Option<u32>` field
   
   **d. Session Creation Logic:**
   - When receiving `CreateSessionRequest`:
     - Read `min_instances` and `max_instances` directly from `SessionSpec` in the request
       - These were already populated by the Python SDK from `RunnerContext`
       - No need to deserialize `RunnerContext` from `common_data` again
     - **Fallback Logic**: If `SessionSpec.max_instances` is None (unlimited), use `Application.max_instances` as fallback
       - Query the application from the database
       - If `Application.max_instances` exists, use it as the effective limit
       - If both are None, session remains truly unlimited (effective `max_instances = None`)
       - This ensures sessions respect application limits when set, but allows unlimited if both are None
     - Validate that `min_instances <= max_instances` (if max_instances is Some), return error if validation fails
     - Store `SessionSpec` with the effective `max_instances` value in the database
   
   **e. Session Loading Logic:**
   - When loading sessions from database:
     - `SessionDao` automatically populated by SQLx with new columns
     - `TryFrom` converts to internal `Session` struct
     - Scheduler reads `session.min_instances` and `session.max_instances` for allocation
   
   **f. Model Layer (`session_manager/src/model/mod.rs`):**
   - Update `SessionInfo` struct to include:
     - `min_instances: u32` field
     - `max_instances: Option<u32>` field
   - Update `From<&Session> for SessionInfo` implementation to populate these fields
   
  **g. Scheduler Plugins (`session_manager/src/scheduler/plugins/`):**
  - Update `fairshare.rs` plugin:
    - Update `SSNInfo` struct to include `min_instances` and `max_instances` fields
    - In `setup()` method (around line 127-146):
      - Read `session.min_instances` and `session.max_instances` from `SessionInfo`
      - Note: `session.max_instances` already includes application limit (applied during session creation)
      - Cap desired executors: `desired = desired.min((session.max_instances * ssn.slots) as f64)` if max_instances is Some
      - Ensure minimum allocation: `desired = desired.max((session.min_instances * ssn.slots) as f64)`
     - During the fairshare loop (calculating deserved from remaining slots):
       - Initialize `deserved = min_instances * slots` (guarantee minimum)
       - Only sessions with `deserved < desired` participate in fairshare distribution
       - When distributing slots, cap each session's deserved by its `desired` value
       - This ensures `min_instances <= deserved <= desired <= max_instances` for all sessions
     - Update `is_underused()` method to check if `allocated < deserved`
       - Note: deserved is already guaranteed to be >= min_instances from the calculation above
   
   **h. Scheduler (`session_manager/src/scheduler/`):**
   - Read `min_instances` and `max_instances` from `Session` struct and pass to `SessionInfo`
   - Executor allocation happens asynchronously in the scheduler loop (not immediately on session creation):
     - **Scheduler Loop**: Runs periodically (every `schedule_interval` ms, typically 1000ms)
     - **AllocateAction** (`scheduler/actions/allocate.rs`): Executed as part of scheduler actions
       - Gets all open sessions, orders them by fairshare plugin's `ssn_order_fn`
       - For each session, checks `plugins.is_underused(ssn)`
       - **NEW**: Add explicit `max_instances` check by counting actual executors from snapshot
         - This prevents over-allocation when multiple executors are created in one cycle
         - Fairshare's cached `allocated` count doesn't update within a cycle
       - If underused and below `max_instances`, allocates executors
       - Calls `ctx.create_executor(node, ssn)` to actually create executor
   - Delegate allocation decisions to scheduler plugins (fairshare)
   - Plugins respect both `min_instances` (guaranteed) and `max_instances` (limit)
   - Note: Sessions with `min_instances > 0` will be allocated executors in the first scheduler cycle (within ~1 second)

5. **Examples and Tests**:
   - Update existing examples to use new API
   - Add new examples demonstrating `stateful` and `autoscale` usage
   - Update integration tests to verify new behavior

**Integration Points:**
- **Python SDK → Cache**: `RunnerContext` is serialized with cloudpickle and stored in the object cache (embedded in executor-manager)
- **Python SDK → Session Manager**: `SessionSpec` protobuf message (via RPC) contains `min_instances`, `max_instances`, and reference to cached `RunnerContext`
- **Session Manager → Database**: `SessionSpec` fields are persisted to session table
- **Session Manager → Scheduler**: Scheduler reads `min_instances` and `max_instances` from `SessionSpec` to control executor allocation
- **Executor → Cache**: `FlameRunpyService` loads `RunnerContext` from cache using reference in `SessionSpec.common_data`
- **Executor → Cache**: If `stateful=True`, persists updated execution object back to cache

**Compatibility:**
- **Database Migration**: 
  - New columns have default values (min_instances=0, max_instances=NULL)
  - Existing sessions will be assigned defaults upon migration
  - Migration can be applied before deploying new session manager version
  - Sessions in database are compatible with new schema

**Breaking Changes:**
- `RunnerServiceKind` enum is removed from `sdk/python/src/flamepy/rl/types.py`
- `kind` parameter is removed from `Runner.service()` signature
- **Migration Required**: Users must update their code:
  - Old: `runner.service(obj, kind=RunnerServiceKind.Stateful)` 
  - New: `runner.service(obj, stateful=True, autoscale=False)`
  - Old: `runner.service(obj, kind=RunnerServiceKind.Stateless)`
  - New: `runner.service(obj, stateful=False, autoscale=True)` (or omit parameters, this is the default for functions)

## 3. Implementation Detail

### Architecture

The enhancement spans multiple layers from SDK to session manager to executors:

```
┌──────────────────────────────────────────────────────────┐
│           Python SDK (runner.py)                         │
│  - Accept stateful/autoscale parameters                  │
│  - Serialize class (not instance)                        │
│  - Create RunnerContext with min/max instances           │
│  - Store RunnerContext in cache                          │
└────────────────┬─────────────────────────────────────────┘
                 │
                 ▼ create_session() with CreateSessionRequest
┌──────────────────────────────────────────────────────────┐
│           Python SDK (client.py)                         │
│  - Create SessionSpec protobuf message                   │
│  - Set SessionSpec.min_instances from RunnerContext      │
│  - Set SessionSpec.max_instances from RunnerContext      │
│  - Set SessionSpec.common_data (RunnerContext ObjectRef) │
└────────────────┬─────────────────────────────────────────┘
                 │
                 ▼ RPC: CreateSessionRequest(SessionSpec)
┌──────────────────────────────────────────────────────────┐
│       Session Manager (Rust)                             │
│  - Receive CreateSessionRequest with SessionSpec         │
│  - Read min_instances and max_instances from SessionSpec │
│  - Apply fallback: if max=None, use app.max_instances    │
│  - Validate min_instances <= max_instances               │
│  - Store Session in database with these values           │
│  - Return (executor allocation is asynchronous)          │
└────────────────┬─────────────────────────────────────────┘
                 │
                 ▼ Asynchronous executor allocation (scheduler loop ~1s)
┌──────────────────────────────────────────────────────────┐
│       Scheduler Loop (Rust)                              │
│  - FairShare: calculate deserved (>= min_instances)      │
│  - AllocateAction: check is_underused() for sessions     │
│  - Create executors until allocated >= deserved          │
│  - Respect max_instances limit                           │
└────────────────┬─────────────────────────────────────────┘
                 │
                 ▼ Task execution
┌──────────────────────────────────────────────────────────┐
│      FlameRunpyService (runpy.py)                        │
│  - on_session_enter: Load RunnerContext from cache       │
│  - Load execution object, instantiate if class           │
│  - on_task_invoke: Use cached execution object           │
│  - Persist state to cache if stateful=True               │
└──────────────────────────────────────────────────────────┘
```

**Key Data Flows:**
1. **RunnerContext**: Created by SDK, stored in cache, contains execution object and config
2. **SessionSpec**: RPC API message, contains min/max instances and common_data reference
3. **Session Table**: Database storage, persists SessionSpec fields for recovery
4. **Scheduler Loop** (asynchronous executor allocation):
   - Session created → stored in database
   - Scheduler loop (every ~1 second) creates snapshot
   - FairShare plugin calculates deserved (>= min_instances * slots)
   - AllocateAction checks is_underused() for each session
   - If underused, creates executors via ctx.create_executor()
   - Process repeats until session reaches deserved allocation

### Components

**1. Runner.service() (Python SDK)**
- **Location**: `sdk/python/src/flamepy/rl/runner.py`
- **Responsibilities**:
  - Accept `stateful` and `autoscale` parameters
  - Determine defaults based on execution object type
  - Validate configuration (e.g., class with stateful=True is invalid)
  - Do NOT instantiate classes (keep as class)
  - Create `RunnerContext` with configuration
  - Serialize and store `RunnerContext` in cache

**2. RunnerContext (Python SDK)**
- **Location**: `sdk/python/src/flamepy/rl/types.py`
- **Responsibilities**:
  - Store execution object and configuration
  - Compute `min_instances` and `max_instances` based on `autoscale`
  - Validate configuration (e.g., classes cannot be stateful)

**3. FlameRunpyService (Python SDK)**
- **Location**: `sdk/python/src/flamepy/rl/runpy.py`
- **Responsibilities**:
  - Load execution object in `on_session_enter` (not `on_task_invoke`)
  - Store execution object as instance variable
  - If execution object is a class, instantiate it using default constructor
  - Use stored execution object in `on_task_invoke`
  - After task execution, if `stateful=True`, update cache with modified execution object

**4. SessionSpec (RPC Protobuf)**
- **Location**: `rpc/protos/types.proto`
- **Responsibilities**:
  - Define the core API interface for session configuration
  - Include `min_instances` and `max_instances` as part of session specification
  - Used by all components (session manager, SDK, flmctl) to communicate session configuration
  - Generated into Rust, Python, and other language bindings

**5. Session Table (Database)**
- **Location**: `session_manager` database schema
- **Responsibilities**:
  - Store persistent session data including `min_instances` and `max_instances`
  - Support session recovery after session manager restarts
  - Enable querying sessions by instance configuration
  - Map to/from `SessionSpec` protobuf message

**6. SessionInfo Struct (Scheduler Model)**
- **Location**: `session_manager/src/model/mod.rs`
- **Responsibilities**:
  - Snapshot model used by scheduler and scheduler plugins
  - Include `min_instances` and `max_instances` fields from `Session`
  - Provide session configuration to scheduler plugins during scheduling decisions
  - Populated via `From<&Session> for SessionInfo` conversion

**7. Session Manager (Rust)**
- **Location**: `session_manager/src/manager.rs`
- **Responsibilities**:
  - **Session Creation**:
    - Receive `CreateSessionRequest` with `SessionSpec` containing `min_instances` and `max_instances`
    - Read values directly from `SessionSpec` (already populated by SDK from `RunnerContext`)
    - **Apply Fallback**: If `SessionSpec.max_instances` is None, query `Application` and use `Application.max_instances`
    - Validate that `min_instances <= max_instances`, return error if validation fails
    - Store `Session` with effective `max_instances` in database table
    - Return created session (executor allocation happens asynchronously in scheduler)
  - **Session Loading** (recovery):
    - Load session data from database table
    - Construct internal `Session` struct with `min_instances` and `max_instances` from table columns
    - Restore session state (scheduler will allocate executors as needed in next cycle)

**8. Scheduler (Rust)**
- **Location**: `session_manager/src/scheduler/`
- **Responsibilities**:
  - **Scheduler Loop** (`mod.rs`):
    - Runs continuously in background thread
    - Every `schedule_interval` milliseconds (typically 1000ms):
      - Creates snapshot of current cluster state
      - Executes scheduling actions (AllocateAction, DispatchAction, etc.)
  - **AllocateAction** (`actions/allocate.rs`):
    - Gets all open sessions from snapshot
    - Orders sessions by fairshare priority
    - For each session:
      - Checks `plugins.is_underused(ssn)` - returns true if `allocated < deserved`
      - **NEW**: Explicit `max_instances` check - counts actual executors from snapshot to prevent over-allocation
      - If underused and below max_instances, finds available nodes and calls `ctx.create_executor(node, ssn)`
    - Sessions with `min_instances > 0` will have `deserved >= min_instances * slots`
    - Therefore, they will be allocated executors until reaching at least `min_instances`
    - Sessions will not exceed `max_instances` even when multiple executors are allocated in one cycle
  - **Context** (`ctx.rs`):
    - Maintains snapshot of cluster state
    - Delegates allocation decisions to plugins
    - Calls controller to actually create executors

**9. FairShare Plugin (Scheduler)**
- **Location**: `session_manager/src/scheduler/plugins/fairshare.rs`
- **Responsibilities**:
  - **setup()**: Called at the start of each scheduler cycle
    - Calculates `desired` for each session based on pending/running tasks
    - Caps desired by session's `max_instances` (already includes app limit from creation)
    - Initializes `deserved = min_instances * slots` (guaranteed minimum)
    - Distributes remaining cluster resources fairly across sessions
    - Ensures: `min_instances * slots <= deserved <= desired <= max_instances * slots`
  - **is_underused()**: Called by AllocateAction for each session
    - Returns true if `allocated < deserved`
    - This ensures sessions reach their guaranteed `min_instances`
    - And get fair share of resources beyond that
  - **is_allocatable()**: Checks if node has capacity for session's slots
  - **ssn_order_fn()**: Orders sessions by fairshare priority
  - Update `SSNInfo` struct to include `min_instances` and `max_instances`

### Data Structures

**RunnerContext (Updated):**

```python
@dataclass
class RunnerContext:
    execution_object: Any
    stateful: bool = False
    autoscale: bool = True
    # Computed fields
    min_instances: int = field(init=False, repr=False)
    max_instances: Optional[int] = field(init=False, repr=False)
    
    def __post_init__(self) -> None:
        # Compute min/max instances based on autoscale
        if self.autoscale:
            self.min_instances = 0
            self.max_instances = None  # Unlimited
        else:
            self.min_instances = 1
            self.max_instances = 1  # Single instance
        
        # Validation
        if self.stateful and inspect.isclass(self.execution_object):
            raise ValueError("Cannot set stateful=True for a class. Pass an instance instead.")
```

**FlameRunpyService State (Updated):**

```python
class FlameRunpyService(FlameService):
    def __init__(self):
        self._ssn_ctx: SessionContext = None
        self._execution_object: Any = None  # Cached execution object
        self._runner_context: RunnerContext = None  # Configuration
```

**Session Table Schema (Database):**

Current sessions table schema (from existing migrations):
```sql
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    application     TEXT NOT NULL,
    slots           INTEGER NOT NULL,
    common_data     BLOB,
    creation_time   INTEGER NOT NULL,
    completion_time INTEGER,
    state           INTEGER NOT NULL,
    version         INTEGER NOT NULL DEFAULT 1
);
```

**New Migration Script** (`migrations/sqlite/20260123000000_add_session_instances.sql`):

```sql
-- Add min_instances and max_instances columns to sessions table for RFE323
ALTER TABLE sessions 
ADD COLUMN min_instances INTEGER NOT NULL DEFAULT 0;

ALTER TABLE sessions 
ADD COLUMN max_instances INTEGER;  -- NULL means unlimited

-- Updated schema after migration:
-- sessions table will have:
--   id, application, slots, common_data, 
--   creation_time, completion_time, state, version,
--   min_instances, max_instances
```

**SessionDao Struct (Rust - storage layer):**

Current SessionDao in `session_manager/src/storage/engine/types.rs`:
```rust
#[derive(Clone, FromRow, Debug)]
pub struct SessionDao {
    pub id: SessionID,
    pub application: String,
    pub slots: i64,
    pub version: u32,
    pub common_data: Option<Vec<u8>>,
    pub creation_time: i64,
    pub completion_time: Option<i64>,
    pub state: i32,
}
```

**Updated SessionDao Struct:**
```rust
#[derive(Clone, FromRow, Debug)]
pub struct SessionDao {
    pub id: SessionID,
    pub application: String,
    pub slots: i64,
    pub version: u32,
    pub common_data: Option<Vec<u8>>,
    pub creation_time: i64,
    pub completion_time: Option<i64>,
    pub state: i32,
    pub min_instances: i64,          // New field
    pub max_instances: Option<i64>,  // New field, NULL means unlimited
}
```

**Updated TryFrom Implementation:**
```rust
impl TryFrom<&SessionDao> for Session {
    type Error = FlameError;

    fn try_from(ssn: &SessionDao) -> Result<Self, Self::Error> {
        Ok(Self {
            id: ssn.id.clone(),
            application: ssn.application.clone(),
            slots: ssn.slots as u32,
            version: ssn.version,
            common_data: ssn.common_data.clone().map(Bytes::from),
            creation_time: DateTime::<Utc>::from_timestamp(ssn.creation_time, 0)
                .ok_or(FlameError::Storage("invalid creation time".to_string()))?,
            completion_time: ssn.completion_time
                .map(|t| {
                    DateTime::<Utc>::from_timestamp(t, 0)
                        .ok_or(FlameError::Storage("invalid completion time".to_string()))
                })
                .transpose()?,
            tasks: HashMap::new(),
            tasks_index: HashMap::new(),
            status: SessionStatus {
                state: ssn.state.try_into()?,
            },
            events: vec![],
            min_instances: ssn.min_instances as u32,  // New field conversion
            max_instances: ssn.max_instances.map(|v| v as u32),  // New field conversion
        })
    }
}
```

**Internal Session Struct (Rust - runtime):**

Current Session in `common/src/apis.rs`:
```rust
pub struct Session {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,
    pub version: u32,
    pub common_data: Option<CommonData>,
    pub tasks: HashMap<TaskID, TaskPtr>,
    pub tasks_index: HashMap<TaskState, HashMap<TaskID, TaskPtr>>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub events: Vec<Event>,
    pub status: SessionStatus,
}
```

**Updated Internal Session Struct:**
```rust
pub struct Session {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,
    pub version: u32,
    pub common_data: Option<CommonData>,
    pub tasks: HashMap<TaskID, TaskPtr>,
    pub tasks_index: HashMap<TaskState, HashMap<TaskID, TaskPtr>>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub events: Vec<Event>,
    pub status: SessionStatus,
    pub min_instances: u32,          // New field
    pub max_instances: Option<u32>,  // New field, None means unlimited (but should be set via fallback)
}
```

**SessionInfo Struct (Scheduler Snapshot Model):**

Current SessionInfo in `session_manager/src/model/mod.rs`:
```rust
pub struct SessionInfo {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,
    pub tasks_status: HashMap<TaskState, i32>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub state: SessionState,
}
```

**Updated SessionInfo Struct:**
```rust
pub struct SessionInfo {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,
    pub tasks_status: HashMap<TaskState, i32>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub state: SessionState,
    pub min_instances: u32,          // New field
    pub max_instances: Option<u32>,  // New field
}
```

**Updated From<&Session> for SessionInfo:**
```rust
impl From<&Session> for SessionInfo {
    fn from(session: &Session) -> Self {
        // ... existing field mappings ...
        Self {
            id: session.id.clone(),
            application: session.application.clone(),
            slots: session.slots,
            tasks_status: calculate_tasks_status(&session.tasks_index),
            creation_time: session.creation_time,
            completion_time: session.completion_time,
            state: session.status.state,
            min_instances: session.min_instances,  // New field mapping
            max_instances: session.max_instances,  // New field mapping
        }
    }
}
```

**FairShare SSNInfo Struct:**

Current SSNInfo in `session_manager/src/scheduler/plugins/fairshare.rs`:
```rust
struct SSNInfo {
    pub id: SessionID,
    pub slots: u32,
    pub desired: f64,
    pub deserved: f64,
    pub allocated: f64,
}
```

**Updated SSNInfo Struct:**
```rust
struct SSNInfo {
    pub id: SessionID,
    pub slots: u32,
    pub desired: f64,
    pub deserved: f64,
    pub allocated: f64,
    pub min_instances: u32,          // New field
    pub max_instances: Option<u32>,  // New field (effective max after considering app limit)
}
```

**SessionSpec (RPC/Protobuf):**

The `SessionSpec` message in `rpc/protos/types.proto` needs to be updated to include instance configuration fields:

```protobuf
message SessionSpec {
  string application = 2;
  uint32 slots = 3;
  optional bytes common_data = 4;
  uint32 min_instances = 5;  // Minimum number of instances (default: 0)
  optional uint32 max_instances = 6;  // Maximum number of instances (null means unlimited)
}
```

This is part of the core RPC API interface and will be used by:
- Session manager to store and retrieve instance configuration
- SDK clients to create sessions with instance limits
- `flmctl` to display session configuration

**Session Manager Internal Structures:**

The session manager will use the protobuf-generated types directly, reading `min_instances` and `max_instances` from the `Session` message's `spec` field when loading sessions from the database and when creating new sessions.

**Session Creation with Validation (Rust):**

```rust
// Pseudo-code for session manager
fn create_session(request: CreateSessionRequest) -> Result<Session> {
    // Step 1: Read min_instances and max_instances from SessionSpec
    // These were already populated by the Python SDK from RunnerContext
    let min_instances = request.spec.min_instances as usize;
    let max_instances = request.spec.max_instances.map(|v| v as usize);
    
    // Step 2: Validate instance configuration
    if let Some(max) = max_instances {
        if min_instances > max {
            return Err(FlameError::InvalidArgument(
                format!("min_instances ({}) must be <= max_instances ({})", 
                    min_instances, max)
            ));
        }
    }
    
    // Note: No need to extract from RunnerContext in common_data - 
    // the SDK already populated SessionSpec with these values
    
    Ok(Session {
        metadata: generate_session_metadata(),
        spec: request.spec,
        status: SessionStatus::default(),
    })
}
```

### Algorithms

**Algorithm 1: Runner.service() with Default Determination**

```python
def service(self, execution_object, stateful=None, autoscale=None):
    # Step 1: Determine execution object type
    is_function = callable(execution_object) and not inspect.isclass(execution_object)
    is_class = inspect.isclass(execution_object)
    is_instance = not is_function and not is_class
    
    # Step 2: Apply defaults if not specified
    if stateful is None:
        stateful = False  # All types default to False
    
    if autoscale is None:
        if is_function:
            autoscale = True
        else:  # class or instance
            autoscale = False
    
    # Step 3: Validation
    if stateful and is_class:
        raise ValueError("Cannot set stateful=True for a class")
    
    # Step 4: Do NOT instantiate classes (keep as-is)
    # Old code: if inspect.isclass(execution_object): execution_object = execution_object()
    # New code: keep execution_object as class
    
    # Step 5: Create RunnerContext
    runner_context = RunnerContext(
        execution_object=execution_object,
        stateful=stateful,
        autoscale=autoscale
    )
    
    # Step 6: Serialize and store in cache
    serialized = cloudpickle.dumps(runner_context)
    object_ref = put_object(self._name, session_id, serialized)
    
    # Step 7: Create session and RunnerService
    return RunnerService(self._name, execution_object, stateful, autoscale)
```

**Algorithm 2: on_session_enter() with Object Loading**

```python
def on_session_enter(self, context: SessionContext) -> bool:
    # Step 1: Install package if needed
    if context.application.url:
        self._install_package_from_url(context.application.url)
    
    # Step 2: Load RunnerContext from common_data
    common_data_bytes = context.common_data()
    object_ref = ObjectRef.decode(common_data_bytes)
    serialized_ctx = get_object(object_ref)
    runner_context = cloudpickle.loads(serialized_ctx)
    
    # Step 3: Store configuration
    self._ssn_ctx = context
    self._runner_context = runner_context
    
    # Step 4: Load execution object
    execution_object = runner_context.execution_object
    
    # Step 5: If it's a class, instantiate it
    if inspect.isclass(execution_object):
        logger.debug(f"Instantiating class {execution_object.__name__}")
        execution_object = execution_object()  # Use default constructor
    
    # Step 6: Store execution object for reuse
    self._execution_object = execution_object
    
    logger.info("Session entered successfully, execution object loaded")
    return True
```

**Algorithm 3: on_task_invoke() with State Persistence**

```python
def on_task_invoke(self, context: TaskContext) -> Optional[TaskOutput]:
    # Step 1: Use cached execution object (not from common_data)
    execution_object = self._execution_object
    
    # Step 2: Deserialize RunnerRequest
    request = cloudpickle.loads(context.input)
    
    # Step 3: Resolve arguments
    invoke_args = tuple(self._resolve_object_ref(arg) for arg in request.args or ())
    invoke_kwargs = {k: self._resolve_object_ref(v) for k, v in (request.kwargs or {}).items()}
    
    # Step 4: Execute method or callable
    if request.method is None:
        result = execution_object(*invoke_args, **invoke_kwargs)
    else:
        method = getattr(execution_object, request.method)
        result = method(*invoke_args, **invoke_kwargs)
    
    # Step 5: Update execution object if stateful
    if self._runner_context.stateful:
        logger.debug("Persisting execution object state")
        updated_context = RunnerContext(
            execution_object=execution_object,  # Updated object
            stateful=self._runner_context.stateful,
            autoscale=self._runner_context.autoscale,
        )
        serialized = cloudpickle.dumps(updated_context)
        
        # Get original ObjectRef and update it
        common_data_bytes = self._ssn_ctx.common_data()
        object_ref = ObjectRef.decode(common_data_bytes)
        update_object(object_ref, serialized)
    
    # Step 6: Return result
    application_id = self._ssn_ctx.application.name
    result_ref = put_object(application_id, context.session_id, result)
    return TaskOutput(result_ref.encode())
```

**Algorithm 4: Session Creation Enhancement**

**Context**: The existing `create_session()` method in session manager needs to handle the new `min_instances` and `max_instances` fields.

**Changes Required**:

```rust
// In session_manager/src/manager.rs (or wherever create_session is implemented)
fn create_session(request: CreateSessionRequest) -> Result<Session> {
    // NEW: Step 1 - Read min_instances and max_instances from SessionSpec
    // These were populated by the Python SDK from RunnerContext
    let min_instances = request.spec.min_instances as usize;
    let max_instances = request.spec.max_instances.map(|v| v as usize);
    
    // NEW: Step 2 - Apply fallback logic for max_instances
    let application = get_application(&request.spec.application)?;
    let effective_max_instances = match max_instances {
        Some(max) => Some(max),
        None => {
            // Fallback to application's max_instances (if it exists)
            match application.spec.max_instances {
                Some(app_max) => {
                    tracing::info!(
                        "Session max_instances is None, using application max_instances: {}",
                        app_max
                    );
                    Some(app_max as usize)
                }
                None => {
                    tracing::info!(
                        "Both session and application max_instances are None, session is unlimited"
                    );
                    None
                }
            }
        }
    };
    
    // NEW: Step 3 - Validate min <= max
    if let Some(max) = effective_max_instances {
        if min_instances > max {
            return Err(FlameError::InvalidArgument(
                format!("min_instances ({}) must be <= max_instances ({})", 
                    min_instances, max)
            ));
        }
    }
    
    // NEW: Step 4 - Update SessionSpec with effective max_instances
    let mut session_spec = request.spec.clone();
    session_spec.max_instances = effective_max_instances.map(|v| v as u32);
    
    // MODIFIED: Step 5 - Update INSERT query to include new columns
    db.execute(
        "INSERT INTO sessions (id, application, slots, common_data, 
                               min_instances, max_instances, ...) 
         VALUES (?, ?, ?, ?, ?, ?, ...)",
        params![
            &session_id,
            &session_spec.application,
            session_spec.slots as i64,
            &session_spec.common_data,
            session_spec.min_instances as i64,  // NEW
            session_spec.max_instances.map(|v| v as i64),  // NEW
            // ... existing fields ...
        ]
    )?;
    
    // MODIFIED: Step 6 - Add new fields to Session struct
    let session = Session {
        // ... existing fields ...
        min_instances: session_spec.min_instances,  // NEW
        max_instances: session_spec.max_instances,  // NEW
        // ... existing fields ...
    };
    
    Ok(session)
    // Note: Executor allocation happens asynchronously in scheduler loop
}
```

**Algorithm 5: FairShare Plugin Enhancement**

**Context**: The existing FairShare plugin (`session_manager/src/scheduler/plugins/fairshare.rs`) calculates how many executors each session deserves. This shows the NEW/MODIFIED logic for min/max instances.

**Changes Required in `setup()` method**:

```rust
// In fairshare.rs setup() method
fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
    // ... existing code: load sessions, apps, calculate base desired from tasks ...
    
    for ssn in open_ssns.values() {
        // Existing: Calculate desired from pending/running tasks
        let mut desired = calculate_desired_from_tasks(ssn);
        
        if let Some(app) = apps.get(&ssn.application) {
            // NEW: Cap desired by session's max_instances
            // Note: ssn.max_instances already includes app limit (from session creation)
            if let Some(max_instances) = ssn.max_instances {
                desired = desired.min((max_instances * ssn.slots) as f64);
            }
            
            // NEW: Ensure desired is at least min_instances
            let min_allocation = (ssn.min_instances * ssn.slots) as f64;
            desired = desired.max(min_allocation);
            
            // MODIFIED: Store SSNInfo with new fields
            self.ssn_map.insert(
                ssn.id.clone(),
                SSNInfo {
                    id: ssn.id.clone(),
                    desired,
                    deserved: min_allocation,  // NEW: Initialize to min_instances
                    slots: ssn.slots,
                    min_instances: ssn.min_instances,  // NEW field
                    max_instances: ssn.max_instances,  // NEW field
                    allocated: 0.0,
                },
            );
        }
    }
    
    // ... existing code: calculate allocated from executors ...
    
    // NEW: Reserve slots for guaranteed minimums before fair distribution
    let mut remaining_slots = total_cluster_slots;
    for ssn in self.ssn_map.values() {
        let min_allocation = (ssn.min_instances * ssn.slots) as f64;
        remaining_slots -= min_allocation;
    }
    
    // Existing: Distribute remaining_slots fairly
    // (existing fair distribution loop works as-is, but now respects 
    //  deserved >= min_instances from initialization above)
    // ...
    
    Ok(())
}
```

**SSNInfo struct update** (in `fairshare.rs`):

```rust
struct SSNInfo {
    // ... existing fields ...
    min_instances: u32,      // NEW
    max_instances: Option<u32>,  // NEW
}
```

**Note**: The existing fair distribution loop works unchanged, because:
- `deserved` starts at `min_instances` (guaranteed)
- `desired` is capped by `max_instances`
- Fair distribution only gives more if `deserved < desired`

**Algorithm 6: AllocateAction Enhancement (Scheduler Loop)**

**Context**: The existing AllocateAction (`session_manager/src/scheduler/actions/allocate.rs`) iterates through open sessions, checks if they're underused via `ctx.is_underused()`, and allocates executors. This algorithm shows only the NEW logic needed.

**Change Required**: Add max_instances check after the `is_underused()` check (around line 65-67):

```rust
// Existing code (line ~65)
if !ctx.is_underused(&ssn)? {
    continue;
}

// NEW: Add this check before pipeline check
// Explicit max_instances check (safety guard)
// Note: The fairshare plugin caches allocated count from snapshot
// at the start of the cycle. Within a single cycle, if we allocate
// multiple executors, the cached count doesn't update. To prevent
// over-allocation, we count actual executors from the current snapshot.
if let Some(max_instances) = ssn.max_instances {
    let current_executors = ss.find_executors_by_session(&ssn.id)?;
    let current_count = current_executors.len();
    if current_count >= max_instances as usize {
        tracing::debug!(
            "Session <{}> has reached max_instances limit: {} >= {}",
            ssn.id, current_count, max_instances
        );
        continue;  // Already at max limit
    }
}

// Existing code continues (pipeline check, node iteration, etc.)
```

**Key Points:**
- Runs periodically in scheduler loop (every ~1 second)
- Uses fairshare plugin's `is_underused()` to determine if session needs more executors
- For sessions with `min_instances > 0`, `is_underused()` returns true until `allocated >= min_instances * slots`
- **Explicit max_instances check**: Counts actual executors from snapshot to prevent over-allocation (Step 5)
  - This is needed because fairshare's cached `allocated` count doesn't update within a single scheduler cycle
  - Prevents allocating beyond `max_instances` when creating multiple executors in one cycle
- Creates one executor per iteration, then re-evaluates
- Sessions are processed in fairshare priority order

### System Considerations

**Performance:**
- **Improved**: Loading execution object once per session (not per task) reduces overhead
- **Serialization**: Class pickling is more efficient than instance pickling
- **Cache Access**: Stateful services incur cache update cost after each task
- **Expected Impact**: 10-20% reduction in task invocation latency for stateful services

**Scalability:**
- **Autoscaling**: Services can scale from 0 to unlimited instances based on workload
- **Single Instance**: Services that require coordination can use autoscale=False
- **Executor Allocation**: Session manager dynamically allocates executors via fairshare plugin
- **Fair Sharing**: FairShare plugin ensures fair allocation across sessions while respecting individual session limits
- **Minimum Guarantee**: Sessions with `min_instances > 0` are guaranteed that many executors
- **Maximum Limit**: Session's `max_instances` serves as the effective limit (includes application's limit as fallback during creation)
- **Limitation**: No scale-down mechanism (min_instances remain allocated)

**Reliability:**
- **State Persistence**: Stateful services can recover state from cache
- **Failure Handling**: If executor fails, state persisted in cache can be loaded by new executor
- **Consistency**: No version checking; last-write-wins for state updates
- **Risk**: Concurrent updates to stateful services may cause race conditions

**Resource Usage:**
- **Memory**: Execution objects cached in memory per session
- **Disk**: Stateful services persist to cache storage
- **Network**: State updates require cache communication
- **Executors**: Autoscaling services may use more executors

**Security:**
- **Code Execution**: Execution objects are pickled and unpickled (existing risk)
- **State Tampering**: Cache objects could be modified externally (existing risk)
- **Validation**: No additional security measures in this RFE

**Observability:**
- **Logging**: Log when execution object is loaded, instantiated, and persisted
- **Metrics**: Track cache updates for stateful services
- **Debugging**: Log stateful/autoscale configuration when session is created

**Operational:**
- **Deployment**: Requires database migration to add `min_instances` and `max_instances` columns to session table
- **Database Migration**: 
  - Migration file: `session_manager/migrations/sqlite/20260123000000_add_session_instances.sql`
  - Add `min_instances INTEGER NOT NULL DEFAULT 0` column
  - Add `max_instances INTEGER` column (NULL for unlimited)
  - Existing sessions will automatically get default values (min=0, max=NULL) upon migration
  - Migration can be applied before deploying new session manager (forward-compatible)
  - No downtime required - SQLx handles schema evolution
- **Code Updates**:
  - Update `SessionDao` struct in `session_manager/src/storage/engine/types.rs`
  - Update `Session` struct in `common/src/apis.rs`
  - Update INSERT query in `session_manager/src/storage/engine/sqlite.rs`
  - Update `TryFrom<SessionDao>` implementation to map new fields
- **Configuration**: New parameters are optional with sensible defaults
- **Migration Path**: Existing code continues to work unchanged with defaults
- **Monitoring**: Monitor executor allocation for autoscaling services
- **Data Persistence**: Session configuration survives session manager restarts

### Dependencies

**External Dependencies:**
- `cloudpickle`: For serialization (existing)
- `inspect`: For type detection (existing)

**Internal Dependencies:**
- `rpc/protos/types.proto`: Core RPC API definition (SessionSpec message)
- `flamepy.core.cache`: For object persistence
- `flamepy.core.types`: For FlameContext
- Session manager: For executor allocation logic
- Protobuf compiler: For generating language bindings from proto files

**Version Requirements:**
- Python 3.8+: For type hints and dataclass features
- Rust 1.70+: For session manager updates

## 4. Use Cases

### Basic Use Cases

**Example 1: Stateless Function with Autoscaling**

```python
# Define a stateless function
def process_request(data):
    return {"result": data * 2}

with Runner("my-app") as runner:
    # Default behavior: stateful=False, autoscale=True
    service = runner.service(process_request)
    
    # Submit many tasks, they will autoscale
    futures = [service(i) for i in range(1000)]
    results = runner.get(futures)
```

- **Description**: A stateless request handler that benefits from autoscaling
- **Configuration**: `stateful=False` (default), `autoscale=True` (default)
- **Behavior**: Min 0 instances, max unlimited. Executors created as tasks arrive.
- **Expected outcome**: Efficient parallel processing with automatic scaling

**Example 2: Stateful Object without Autoscaling**

```python
# Define a stateful counter class
class Counter:
    def __init__(self):
        self.count = 0
    
    def increment(self):
        self.count += 1
        return self.count

with Runner("counter-app") as runner:
    # Explicitly configure: stateful=True, autoscale=False
    counter = Counter()
    service = runner.service(counter, stateful=True, autoscale=False)
    
    # All tasks go to the same instance, state is persisted
    service.increment()  # returns 1
    service.increment()  # returns 2
    service.increment()  # returns 3
```

- **Description**: A stateful counter that maintains state across tasks
- **Configuration**: `stateful=True`, `autoscale=False`
- **Behavior**: Min 1 instance, max 1 instance. State persisted after each task.
- **Expected outcome**: Sequential execution with persistent state

**Example 3: Class with Default Constructor**

```python
# Define a class
class DataProcessor:
    def __init__(self):
        self.processed_count = 0
    
    def process(self, data):
        self.processed_count += 1
        return f"Processed {data}, total: {self.processed_count}"

with Runner("processor-app") as runner:
    # Pass the class itself (not an instance)
    service = runner.service(DataProcessor, autoscale=False)
    
    # The class will be instantiated on the executor
    result1 = service.process("data1").get()  # "Processed data1, total: 1"
    result2 = service.process("data2").get()  # "Processed data2, total: 2"
```

- **Description**: A class is passed to service(), and instantiated on the executor
- **Configuration**: `stateful=False` (default), `autoscale=False`
- **Behavior**: Min 1 instance, max 1 instance. Class instantiated in on_session_enter.
- **Expected outcome**: Single instance, but state is not persisted (resets if executor fails)

### Advanced Use Cases

**Example 4: Database Connection Pool**

```python
class ConnectionPool:
    def __init__(self):
        self.pool = create_connection_pool(size=10)
    
    def execute_query(self, query):
        with self.pool.get_connection() as conn:
            return conn.execute(query)

with Runner("db-app") as runner:
    # Single instance, no autoscaling
    pool = ConnectionPool()
    service = runner.service(pool, stateful=False, autoscale=False)
    
    # All queries use the same connection pool
    result = service.execute_query("SELECT * FROM users")
```

- **Description**: A connection pool that should not be duplicated
- **Configuration**: `stateful=False`, `autoscale=False`
- **Behavior**: Min 1 instance, max 1 instance. Maintains connection pool.
- **Expected outcome**: Efficient resource usage with single pool

**Example 5: Distributed Counter with State Persistence**

```python
class DistributedCounter:
    def __init__(self):
        self.counts = {}
    
    def increment(self, key):
        self.counts[key] = self.counts.get(key, 0) + 1
        return self.counts[key]

with Runner("dist-counter") as runner:
    counter = DistributedCounter()
    service = runner.service(counter, stateful=True, autoscale=False)
    
    # State persisted after each task
    service.increment("user1")
    service.increment("user2")
    service.increment("user1")
    
    # If executor fails and restarts, state is recovered from cache
```

- **Description**: A distributed counter with persistent state
- **Configuration**: `stateful=True`, `autoscale=False`
- **Behavior**: Min 1 instance, max 1 instance. State persisted to cache.
- **Expected outcome**: State survives executor failures

**Example 6: Parallel Image Processing**

```python
def process_image(image_data):
    # CPU-intensive image processing
    return transformed_image

with Runner("image-processor") as runner:
    # Autoscale to handle large batch
    service = runner.service(process_image)  # stateful=False, autoscale=True
    
    # Submit 10,000 images
    futures = [service(img) for img in images]
    
    # Executors scale up to handle workload
    results = runner.get(futures)
```

- **Description**: Parallel image processing with autoscaling
- **Configuration**: `stateful=False` (default), `autoscale=True` (default)
- **Behavior**: Min 0 instances, max unlimited. Scales with workload.
- **Expected outcome**: Fast parallel processing with automatic resource allocation

## 5. References

### Related Documents
- RFE323 GitHub Issue: https://github.com/xflops/flame/issues/323
- RFE280: Initial Runner implementation
- RFE284: flmrun application
- RFE318: Apache Arrow-Based Object Cache

### External References
- Python `inspect` module documentation: https://docs.python.org/3/library/inspect.html
- `cloudpickle` documentation: https://github.com/cloudpipe/cloudpickle

### Implementation References

**RPC API:**
- RPC API definition: `rpc/protos/types.proto` (SessionSpec message)

**Python SDK:**
- Runner implementation: `sdk/python/src/flamepy/rl/runner.py`
- RunnerService: `sdk/python/src/flamepy/rl/runpy.py`
- RunnerContext and types: `sdk/python/src/flamepy/rl/types.py`
- Object cache: `sdk/python/src/flamepy/core/cache.py`
- Core client: `sdk/python/src/flamepy/core/client.py` (create_session)

**Session Manager:**
- Migration script: `session_manager/migrations/sqlite/20260123000000_add_session_instances.sql`
- SessionDao struct: `session_manager/src/storage/engine/types.rs`
- SQLite storage: `session_manager/src/storage/engine/sqlite.rs` (INSERT query around line 384)
- SessionInfo struct: `session_manager/src/model/mod.rs`
- Session manager: `session_manager/src/manager.rs`
- Scheduler: `session_manager/src/scheduler/` (main scheduler logic)
- AllocateAction: `session_manager/src/scheduler/actions/allocate.rs` (add max_instances check)
- FairShare plugin: `session_manager/src/scheduler/plugins/fairshare.rs`
- Plugin interface: `session_manager/src/scheduler/plugins/mod.rs`

**Common:**
- Internal Session struct: `common/src/apis.rs`

**Tools:**
- flmctl view command: `cmd/flmctl/` (session view implementation)

# Functional Spec: Node Information Management & Persistence

## 1. Motivation

**Background:**
Currently, `Node` and `Executor` states are maintained in-memory by the Session Manager. When the Session Manager restarts, it loses knowledge of registered nodes and running executors. Additionally, there is no mechanism for administrators to view the status of the cluster.

**Goals:**
1.  **Persistence**: Persist `Node` and `Executor` information to support recovery and consistency.
2.  **Observability**: Provide CLI commands (`flmctl node list/view`) and gRPC APIs to monitor cluster health.

## 2. API Design

### 2.1 gRPC Service Definition

The `Frontend` service will be extended with new methods for node operations.

```protobuf
service Frontend {
  // ... existing methods ...

  // ListNodes returns a list of all registered nodes.
  rpc ListNodes(ListNodesRequest) returns (NodeList);

  // GetNode returns detailed information for a specific node.
  rpc GetNode(GetNodeRequest) returns (GetNodeResponse);
}

message ListNodesRequest {
  // No pagination for now.
}

message GetNodeRequest {
  string name = 1;  // Node name (unique identifier)
}

message GetNodeResponse {
  Node node = 1;
}

// Node follows the standard Kubernetes-style object pattern.
// Aligns with Session/Application patterns in the codebase.
message Node {
  Metadata metadata = 1;
  NodeSpec spec = 2;
  NodeStatus status = 3;
}

// NodeSpec contains the static/desired attributes of a node.
message NodeSpec {
  string hostname = 1;
}

// ResourceRequirement represents compute resources.
message ResourceRequirement {
  uint64 cpu = 1;     // Number of CPU cores
  uint64 memory = 2;  // Memory in bytes
  int32 gpu = 3;      // Number of GPUs
}

// NodeInfo contains system information about the node.
message NodeInfo {
  string arch = 1;
  string os = 2;
}

// NodeAddress represents a network address for a node.
message NodeAddress {
  string type = 1;    // e.g., "InternalIP", "ExternalIP", "Hostname"
  string address = 2;
}

// NodeStatus contains the dynamic/observed state of a node.
message NodeStatus {
  NodeState state = 1;                    // e.g., Ready, NotReady, Unknown
  ResourceRequirement capacity = 2;       // Total resources on the node
  ResourceRequirement allocatable = 3;    // Resources available for scheduling
  NodeInfo info = 4;                      // System information
  repeated NodeAddress addresses = 5;     // Network addresses of the node
  int64 last_heartbeat_time = 6;          // Unix epoch seconds
}

// NodeList contains a list of nodes.
message NodeList {
  repeated Node nodes = 1;
}
```

### 2.2 CLI Design

#### `flmctl list --node`

**Usage:**
```bash
flmctl list --node
# or
flmctl list -n
```

**Output Format (Table):**
```text
NAME      HOSTNAME      STATUS   CPU  MEMORY  ARCH    OS
node-1    worker-01     Ready    8    32Gi    x86_64  linux
node-2    worker-02     Ready    8    32Gi    x86_64  linux
node-3    worker-03     NotReady 4    16Gi    x86_64  linux
```

#### `flmctl view --node <name>`

**Usage:**
```bash
flmctl view --node node-1
# or
flmctl view -n node-1
```

**Output Format (Text):**
```text
Name:           node-1
Hostname:       worker-01
Status:         Ready
Capacity:
  CPU:          8
  Memory:       32Gi
Info:
  Arch:         x86_64
  OS:           linux
```

## 3. Storage Design (Persistence)

### Storage Engine Interface

Update `session_manager/src/storage/engine/mod.rs` to include methods for Node and Executor persistence.

```rust
#[async_trait]
pub trait Engine: Send + Sync + 'static {
    // ... existing methods ...

    // Node Operations
    async fn register_node(&self, node: Node) -> Result<Node, FlameError>;
    async fn update_node(&self, node: Node) -> Result<Node, FlameError>;
    async fn find_node(&self, id: Option<String>) -> Result<Vec<Node>, FlameError>;
    async fn delete_node(&self, id: String) -> Result<(), FlameError>;

    // Executor Operations
    async fn register_executor(&self, executor: Executor) -> Result<Executor, FlameError>;
    async fn update_executor(&self, executor: Executor) -> Result<Executor, FlameError>;
    async fn find_executor(&self, id: Option<String>) -> Result<Vec<Executor>, FlameError>;
    async fn delete_executor(&self, id: String) -> Result<(), FlameError>;
}
```

### Data Structures

**Node Schema:**
- `name` (PK): String
- `uid`: String
- `hostname`: String
- `capacity`: JSON/Struct (CPU, Memory, GPU)
- `allocatable`: JSON/Struct (CPU, Memory, GPU)
- `state`: Enum (Ready, NotReady, Unknown)
- `addresses`: JSON/Array of NodeAddress
- `creation_time`: Timestamp (Unix epoch seconds)
- `last_heartbeat_time`: Timestamp (Unix epoch seconds)
- `labels`: Map<String, String>

**Executor Schema:**
- `id` (PK): String
- `node_name`: String (FK -> Node)
- `state`: Enum (Idle, Bound, Running, etc.)
- `session_id`: String (Nullable)
- `task_id`: String (Nullable)
- `application`: String
- `creation_time`: Timestamp (Unix epoch seconds)

### Implementation Detail

#### SQLite Engine

**Tables:**

```sql
CREATE TABLE IF NOT EXISTS nodes (
    name                TEXT PRIMARY KEY,
    state               INTEGER NOT NULL DEFAULT 0,
    
    -- Capacity resources (flattened, not JSON)
    capacity_cpu        INTEGER NOT NULL DEFAULT 0,
    capacity_memory     INTEGER NOT NULL DEFAULT 0,
    
    -- Allocatable resources (flattened, not JSON)
    allocatable_cpu     INTEGER NOT NULL DEFAULT 0,
    allocatable_memory  INTEGER NOT NULL DEFAULT 0,
    
    -- Node info (flattened, not JSON)
    info_arch           TEXT NOT NULL DEFAULT '',
    info_os             TEXT NOT NULL DEFAULT '',
    
    creation_time       INTEGER NOT NULL,
    last_heartbeat      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS executors (
    id                  TEXT PRIMARY KEY,
    node                TEXT NOT NULL,
    
    -- Resource requirements (flattened)
    resreq_cpu          INTEGER NOT NULL DEFAULT 0,
    resreq_memory       INTEGER NOT NULL DEFAULT 0,
    resreq_gpu          INTEGER NOT NULL DEFAULT 0,

    shim                INTEGER NOT NULL DEFAULT 0,
    
    task_id             INTEGER,
    ssn_id              TEXT,
    
    creation_time       INTEGER NOT NULL,
    state               INTEGER NOT NULL DEFAULT 0,
    
    FOREIGN KEY (node) REFERENCES nodes(name) ON DELETE CASCADE
);

-- Indexes for efficient lookups
CREATE INDEX IF NOT EXISTS idx_executors_node ON executors(node);
CREATE INDEX IF NOT EXISTS idx_executors_state ON executors(state);
CREATE INDEX IF NOT EXISTS idx_executors_ssn_id ON executors(ssn_id);
```

Note: The schema uses flattened columns instead of JSON for better relational database performance and type safety.

#### Filesystem Engine

**Directory Structure:**

```
<data_dir>/
  ├── nodes/
  │   └── <node_name>/
  │       ├── metadata          # Node metadata (JSON)
  │       └── executors/
  │           └── <executor_id>/
  │               └── metadata          # Executor metadata (JSON)
```

**Locking:**
- Use `lock_app!` (global lock) for node/executor registration to ensure consistency.
- Alternatively, introduce `lock_node!` if finer-grained concurrency is needed.

## 4. Component Interactions

1.  **User** runs `flmctl list nodes`.
2.  **flmctl** sends `ListNodes` gRPC request to **Frontend**.
3.  **Frontend** queries **Storage** (via `find_node`) for all nodes.
4.  **Storage** returns list of node objects.
5.  **Frontend** constructs `ListNodesResponse` and returns to **flmctl**.
6.  **flmctl** formats and displays the list.

## 5. Use Cases

**UC1: Node Registration**
- Node Agent starts up and calls `RegisterNode`.
- Session Manager calls `storage.register_node()`.
- Node is persisted.

**UC2: Executor State Change**
- Scheduler assigns task to executor.
- Session Manager updates executor state to `Bound`.
- Session Manager calls `storage.update_executor()`.
- State is persisted.

**UC3: Recovery**
- Session Manager restarts.
- Calls `storage.find_node(None)` and `storage.find_executor(None)`.
- Rebuilds in-memory `ClusterContext`.
- Mark Nodes/Executors as `Unknown` or `Recovering` until they send a heartbeat.

**UC4: List Nodes**
- Admin runs `flmctl list nodes`.
- System returns list of nodes with status (Ready/NotReady).

## 6. Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| **Stale Data** | On restart, persisted state might be stale (e.g., node crashed while SM was down). **Mitigation**: Nodes must re-register or heartbeat to confirm status. Prune nodes that don't report back within a timeout. |
| **Performance** | High frequency executor state changes (e.g., rapid tasks) could overload storage. **Mitigation**: Batch updates or only persist critical state transitions (e.g., Bound/Released). |
| **Eventual Consistency** | `ListNodes` reads from storage, which might lag slightly behind in-memory state. **Mitigation**: Acceptable for CLI listing; critical control plane logic should use in-memory state. |

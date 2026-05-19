# RFE318: Apache Arrow-Based Object Cache

## 1. Motivation

**Background:**
Previously, the object cache in Flame was implemented as a naive HTTP-based in-memory HashMap within the flame-executor-manager component. This implementation had several limitations:

1. **No Persistence**: Data stored in the cache was lost when the cache server restarted, leading to data loss and requiring clients to re-upload objects.
2. **Memory Limitations**: All cached objects were stored in memory, which limited the cache capacity and made it unsuitable for large-scale deployments.
3. **Performance**: The implementation lacked efficient serialization and storage mechanisms, which could become a bottleneck for large objects.
4. **No Standardization**: The implementation didn't leverage industry-standard formats, making it harder to integrate with other systems or tools.

**Note:** The naive cache implementation has been removed from flame-executor-manager as of this RFE. The object cache is now implemented as a library embedded in flame-executor-manager, running in a dedicated thread.

**Target:**
This design aims to improve the object cache implementation by leveraging Apache Arrow to achieve:

1. **Persistent Storage**: Implement disk-based persistence using Arrow IPC format to ensure data survives server restarts.
2. **Performance**: Utilize Arrow's efficient columnar format and zero-copy capabilities for better performance with large objects.
3. **Backward Compatibility**: Maintain compatibility with existing Python SDK clients while improving the underlying implementation.
4. **Scalability**: Support both local and remote cache access patterns to enable distributed caching scenarios.
5. **Standardization**: Use Arrow Flight as the communication protocol, which is a standard for high-performance data services.
6. **Incremental Updates**: Support `patch` operation for appending delta data to objects, enabling efficient distributed operations with multiple clients.
7. **Native Tabular Payloads**: Store DataSet/DataFrame-style payloads directly as Arrow data instead of wrapping them as pickled or opaque IPC bytes.

## 2. Function Specification

### Configuration

**FlameClusterContext for Cluster (flame-cluster.yaml):**
- Add a new `storage` field under the `cache` section to specify the local storage path for cache persistence.
- The storage path should be a directory path where Arrow IPC files will be stored.
- If not specified, the cache will operate in-memory only (backward compatible behavior).
- **Add a `capacity` field to specify the maximum storage usage (e.g., "10GB").**

Example configuration:
```yaml
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"  # New field
  capacity: "10GB"                 # New field: Max cache size (triggers LRU eviction)
```

**FlameContext for Python SDK (flame.yaml):**
- Change `cache` from a string endpoint to an object containing `endpoint` and `storage` fields.
- The `endpoint` field specifies the cache server endpoint (e.g., "grpc://127.0.0.1:9090").
- The `storage` field specifies a local storage path for client-side caching (optional).
- When `storage` is set, the Python SDK will write data directly to this storage path using Arrow IPC.
- The client will get flight info to construct an ObjectRef with the cache's remote endpoint.
- If `storage` is not set, the client will connect to the remote cache server via the endpoint.
- If the `endpoint` is not set, an exception will be raised.

Example Python SDK YAML configuration (`~/.flame/flame.yaml`):
```yaml
current-cluster: flame
clusters:
  - name: flame
    endpoint: "http://flame-session-manager:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
      storage: "/tmp/flame_cache"  # Optional: local storage path
    package:
      storage: "file:///opt/flame/packages"
      excludes:
        - "*.log"
        - "*.pkl"
        - "*.tmp"
```

Example Python SDK configuration (programmatic):
```python
context = FlameContext()
# Set cache configuration
context.cache = {
    "endpoint": "grpc://127.0.0.1:9090",
    "storage": "/tmp/flame_cache"  # Optional: local storage path
}
# Access cache fields
context.cache.endpoint  # "grpc://127.0.0.1:9090"
context.cache.storage   # "/tmp/flame_cache" (optional)
```

**Environment Variables:**
- `FLAME_CACHE_STORAGE`: Override cache storage path (for both cluster and client)
- `FLAME_CACHE_ENDPOINT`: Override cache endpoint (existing)
- `FLAME_CACHE_CAPACITY`: Override cache capacity

### API

**Arrow Flight Protocol:**
The cache server implements the Arrow Flight protocol with the following operations:

1. **do_put**: Upload an object to the cache
   - Request: Streaming FlightData containing RecordBatch with object data
   - Metadata: `session_id:{id}` in app_metadata
   - Response: PutResult with ObjectRef in app_metadata (BSON format)
   - Behavior: Persists data to disk using Arrow IPC, returns ObjectRef

2. **do_get**: Retrieve an object from the cache
   - Request: Ticket containing key (`app_name/session_id/object_id`) and optional cached version suffix
   - Response: Streaming FlightData containing RecordBatch with object data
   - Behavior: Reads base object and all deltas from disk, returns combined data

3. **get_flight_info**: Get metadata about a flight
   - Request: FlightDescriptor with path (`{app_name}/{session_id}/{object_id}`)
   - Response: FlightInfo with schema information

4. **list_flights**: List all cached objects
   - Request: Criteria (optional filtering)
   - Response: Streaming FlightInfo for all objects, aligned with key structure

5. **do_action**: Perform cache operations (PUT, UPDATE, DELETE, PATCH)
   - PUT: Put object (legacy support)
   - UPDATE: Update existing object (replaces base and all deltas)
   - DELETE: Delete objects by key prefix. Supports:
     - `{app}/{session}` - delete all objects in a specific session
     - `{app}/*` - delete all objects across all sessions of an application (wildcard)
   - **PATCH**: Append delta data to an existing object (new)

### Native DataSet/DataFrame Cache Path

Issue #318 item 3 scopes the next cache enhancement to: **For DataSet/DataFrame, put to cache directly**. In this design, "DataSet/DataFrame" means tabular Python payloads that can be represented losslessly as Arrow batches without cloudpickle:

- `pyarrow.Table`
- `pyarrow.RecordBatch`
- `pandas.DataFrame` when pandas is installed
- `polars.DataFrame` and collected `polars.LazyFrame` when polars is installed
- Dataset-like objects that expose an Arrow table through `to_arrow()`, `__arrow_c_stream__`, or an adapter registered by FlamePy

The current Python fast path avoids cloudpickle for PyArrow tables, but it still serializes the table into Arrow IPC bytes and stores those bytes in the opaque `{version, data}` cache row. The direct tabular path must avoid that wrapper. The cache should stream and persist the original Arrow schema and record batches as the cached object payload.

**Public API Behavior:**

- `put_object(key_prefix, obj)` remains the entry point. It classifies the payload before writing:
  - Tabular payloads use the native Arrow path.
  - All other objects use the existing opaque object path.
- `get_object(ref)` returns the original logical object type when the required optional dependency is installed:
  - PyArrow inputs return `pyarrow.Table` or `pyarrow.RecordBatch`.
  - pandas inputs return `pandas.DataFrame`.
  - polars inputs return `polars.DataFrame`.
  - Generic Dataset inputs return the registered adapter output; if no adapter is available, they return `pyarrow.Table`.
- If a consumer does not have the optional library needed to reconstruct the original type, FlamePy returns `pyarrow.Table` rather than failing for pandas/polars-compatible payloads. Adapter-backed Dataset types may raise `ImportError` if the adapter cannot be loaded.
- `ObjectRef` remains `{endpoint, key, version}`. Payload type is cache metadata, not part of the reference.
- `update_object(ref, new_obj)` supports the same payload classification as `put_object`; updating a direct tabular object rewrites the base Arrow object and clears deltas.
- `patch_object(ref, delta)` stays on the existing opaque delta path in the first implementation. Native tabular append/merge semantics are intentionally out of scope for this item.

**Payload Metadata:**

The cache reserves `flame.cache.*` schema metadata keys for native payloads:

| Key | Value |
|-----|-------|
| `flame.cache.format` | `opaque-v1` or `arrow-table-v1` |
| `flame.cache.version` | Current object version as decimal text |
| `flame.cache.logical_type` | `pyarrow.table`, `pyarrow.record_batch`, `pandas.dataframe`, `polars.dataframe`, or adapter name |
| `flame.cache.adapter` | Optional adapter identifier for Dataset-like objects |

User-provided Arrow schema metadata must be preserved. When user metadata collides with `flame.cache.*`, the cache-owned value wins for transport and persistence.

**Flight Protocol Behavior:**

- Native tabular `do_put` requests carry an Arrow schema with `flame.cache.format=arrow-table-v1` and stream the table's record batches directly.
- Opaque object `do_put` requests keep using the current wrapper schema with `version` and `data` fields.
- Native tabular `do_get` responses stream the stored Arrow schema and batches directly. They do not wrap rows in the opaque response schema.
- Opaque object `do_get` responses keep using the response schema with `version`, `kind`, and `data` fields so existing patch and incremental-read behavior remains unchanged.
- `get_flight_info` should return the native table schema for tabular objects when the object is known. For opaque objects it may keep returning an empty schema for backward compatibility.

**Storage Behavior:**

- Opaque objects remain compatible with existing files that use the `version/data` Arrow IPC schema.
- Native tabular objects are stored as Arrow IPC files using the original table schema plus the reserved cache metadata.
- Storage paths and object keys do not change:
  - `{storage_path}/{app_name}/{session_id}/{object_id}.arrow`
  - ObjectRef key: `{app_name}/{session_id}/{object_id}`
- Cache loading must detect both formats:
  - `flame.cache.format=arrow-table-v1` means native tabular object.
  - Existing `version/data` files without metadata are legacy opaque objects.
- Size accounting for eviction should use stored IPC file size when available, falling back to Arrow buffer sizes for in-memory-only storage.

**Implementation Shape:**

Rust cache objects should model payload type explicitly instead of assuming every object is a byte vector:

```rust
pub enum ObjectPayload {
    Opaque(Vec<u8>),
    ArrowTable {
        schema: Arc<Schema>,
        batches: Vec<RecordBatch>,
        logical_type: Option<String>,
        adapter: Option<String>,
    },
}

pub struct Object {
    pub version: u64,
    pub payload: ObjectPayload,
    pub deltas: Vec<Object>,
}
```

The existing `Object { version, data, deltas }` representation remains the legacy opaque representation during migration. New server code should convert legacy data into `ObjectPayload::Opaque` at load boundaries so the rest of the cache can dispatch by payload kind.

**Python Payload Classification:**

FlamePy should classify tabular payloads without adding mandatory pandas, polars, or datasets dependencies:

1. Check exact PyArrow types first (`pa.Table`, `pa.RecordBatch`, `pa.RecordBatchReader`).
2. Check optional pandas/polars types only when those modules are importable.
3. Check adapter registry entries for Dataset-like objects.
4. Check Arrow protocol methods such as `__arrow_c_stream__` or `to_arrow()`.
5. Fall back to the existing opaque object serializer.

The classifier should be conservative: if conversion to `pyarrow.Table` is lossy or ambiguous, use the opaque path.

**Out of Scope for Item 3:**

- Designing row-level tabular patch/merge semantics.
- Distributed Dataset partition placement or cache-side query execution.
- Changing `ObjectRef`.
- Making pandas, polars, or datasets required dependencies.
- Migrating existing opaque objects that contain pickled DataFrames.


### Patch Operation Semantics

The `patch` operation enables incremental updates to cached objects by appending delta data. This is particularly useful for:
- **Log aggregation**: Multiple clients appending log entries to a shared object
- **Distributed data collection**: Aggregating results from multiple workers
- **Streaming data**: Building up data incrementally over time

**Semantic Model:**
An object in the cache consists of:
- **Base object**: The initial object data (created via `put_object`)
- **Delta list**: An ordered list of delta data (appended via `patch_object`)

```
Object = Base + [Delta_0, Delta_1, Delta_2, ...]
```

**Key Behaviors:**

| Operation | Behavior |
|-----------|----------|
| `put_object(key, obj)` | Creates new object with `obj` as base, clears any existing deltas |
| `patch_object(key, delta)` | Appends `delta` to the object's delta list |
| `get_object(key)` | Returns `{base: <base_data>, deltas: [<delta_0>, <delta_1>, ...]}` |
| `update_object(key, obj)` | Replaces entire object (base + deltas) with new `obj` as base |

**LRU Interaction:**
The `patch` operation is treated as an **active access** to the object.
- **Recency Update**: Patching an object updates its LRU (Least Recently Used) recency, marking it as the most recently used item.
- **Eviction Protection**: This ensures that objects receiving frequent updates (like active logs) remain in the cache and are not evicted, even if they are not being read via `get_object`.
- **Size Accounting**: The size of the delta is added to the total cache usage, potentially triggering eviction of *other* (older) objects if the capacity is exceeded.

**Why Append-Only (Not Random Access):**
1. **Simplicity**: Append-only semantics are simpler to implement and reason about
2. **Concurrency**: Multiple clients can safely append without coordination
3. **Arrow Flight Alignment**: Maps naturally to Arrow Flight's streaming model
4. **Use Case Fit**: Primary use cases (logs, aggregation) are append-oriented

**ObjectRef Structure:**
The ObjectRef structure is updated to include:
```python
@dataclass
class ObjectRef:
    endpoint: str      # The endpoint of cache server (e.g., "grpc://127.0.0.1:9090")
    key: str          # The key of object (e.g., "app/session/object")
    version: int      # Server-managed version; 0 forces a fresh read
```

**Error Handling:**
- If cache storage is not set and endpoint is not set: raise `FlameError(INVALID_CONFIG, "Cache endpoint not configured")`
- If object not found: return `Status::not_found`
- If storage path is invalid: return `Status::invalid_argument`
- If Arrow IPC operations fail: return `Status::internal` with error details
- **PATCH on non-existent object**: return `Status::not_found` (must `put` first)

### Cache Eviction Policy

To manage disk usage, the cache implements a Least Recently Used (LRU) eviction policy.

**Configuration:**
- `capacity`: Maximum storage size (e.g., "10GB", "500MB"). Defined in `flame-cluster.yaml`.
- If `capacity` is not set or 0, eviction is disabled (unbounded growth).

**Eviction Logic:**
1. **Trigger**: Eviction is triggered when a write operation (`put`, `patch`, `update`) would cause the total storage usage to exceed the configured `capacity`.
2. **Selection**: The cache identifies the Least Recently Used (LRU) object.
3. **Removal**: The LRU object (base file + all delta files) is deleted from disk and removed from the in-memory index.
4. **Repeat**: Steps 2-3 are repeated until there is sufficient space for the new data.

**Interaction with Operations:**
- **`get_object`**: Updates access time. Object becomes MRU (Most Recently Used).
- **`put_object`**: Updates access time. Object becomes MRU.
- **`patch_object`**: Updates access time. Object becomes MRU.
- **`list_flights`**: Does NOT update access time.

### Scope

**In Scope:**
- Implementation of flame-cache library in Rust, integrated into executor-manager
- Arrow Flight server implementation
- Arrow IPC persistence to disk
- Python SDK integration with Arrow Flight client
- Native Arrow storage and transport for DataSet/DataFrame-style payloads
- Local and remote cache access patterns
- ObjectRef structure updates
- Key-based storage organization (`app_name/session_id/object_id`)
- Support for both public IP and localhost binding
- Implementation updates for common data in both RL and service modules

**Out of Scope:**
- Client-side version conflict resolution
- Distributed cache coordination
- Cache replication
- Authentication and authorization
- Cache size limits and quotas
- Cache statistics and monitoring (beyond basic logging)
- Native tabular patch/merge semantics
- Requiring pandas, polars, or datasets as core SDK dependencies

**Limitations:**
- Object versions are server-managed; clients do not perform conflict resolution
- No automatic cache cleanup or eviction
- Single-node cache server (no distributed coordination)
- No authentication/authorization mechanisms
- Storage path must be accessible and writable
- Objects are stored per session; no cross-session sharing

### Feature Interaction

**Related Features:**
- **Session Management**: Cache keys are organized by session ID
- **RL Module**: Uses cache for storing RunnerContext and common data
- **Agent Module**: Uses cache for storing common data

**Updates Required:**
1. **FlameClusterContext (Rust)**: Add `storage` field to `FlameCache` struct and `FlameCacheYaml`
2. **FlameContext (Python)**: Change `cache` from string to object with `endpoint` and `storage` fields
3. **ObjectRef (Python)**: Update structure to include `endpoint`, `key`, and `version`
4. **Python SDK cache.py**: Replace HTTP-based implementation with Arrow Flight client
5. **RL Module**: Update to use new ObjectRef structure
6. **Agent Module**: Update to use new ObjectRef structure
7. **Python SDK tabular payload classifier**: Detect PyArrow, pandas, polars, and Dataset-like objects that can be represented as Arrow tables.
8. **Rust cache payload model**: Distinguish opaque and native Arrow table payloads in storage, Flight responses, and metadata.

**Integration Points:**
- Arrow Flight server integrates with gRPC/tonic
- Arrow IPC files stored in filesystem
- Python SDK uses pyarrow for Arrow Flight client
- Cache server binds to both public IP (from network_interface) and localhost (127.0.0.1)

**Compatibility:**
- Backward compatible: If storage path is not set, cache operates in-memory (though this may be deprecated)
- ObjectRef structure change requires updates to all clients
- Arrow Flight protocol is standard and well-supported

**Breaking Changes:**
- ObjectRef structure change: `url` field replaced with `endpoint` and `key` fields
- Cache protocol change: HTTP REST API replaced with Arrow Flight
- Python SDK clients must be updated to use new ObjectRef structure

## 3. Implementation Detail

### Architecture

The flame-object-cache component is a standalone Rust service that implements an Arrow Flight server. It persists data to disk using Arrow IPC format and serves cached objects to clients via Arrow Flight protocol.

```
┌─────────────────┐
│  Python SDK     │
│  (Arrow Flight  │
│   Client)       │
└────────┬────────┘
         │ Arrow Flight (gRPC)
         │
         ▼
┌─────────────────┐
│ flame-object-   │
│ cache Server    │
│ (Arrow Flight)  │
└────────┬────────┘
         │
         ├──► In-Memory Index
         │    (HashMap<key, metadata>)
         │
         └──► Disk Storage
              (Arrow IPC files)
              /storage_path/
                └── app_name/
                    └── session_id/
                        └── object_id.arrow
```

### Components

**1. flame-cache (Rust Library in executor-manager)**
- **Location**: `object_cache/` directory in workspace
- **Responsibilities**:
  - Implement Arrow Flight server
  - Manage object storage and retrieval
  - Persist data using Arrow IPC
  - Handle both public IP and localhost bindings
  - Maintain in-memory index for fast lookups

**2. FlightCacheServer**
- **Responsibilities**:
  - Implement FlightService trait
  - Handle do_put, do_get, get_flight_info, list_flights operations
  - Convert between Arrow formats and internal Object representation
  - Return ObjectRef in BSON format

**3. ObjectCache**
- **Responsibilities**:
  - Manage object storage (in-memory index + disk persistence)
  - Generate unique object IDs
  - Organize storage by session ID
  - Handle Arrow IPC read/write operations
  - Store cache service's public endpoint (obtained during construction via network_interface)

**4. Python SDK Cache Module**
- **Location**: `sdk/python/src/flamepy/core/cache.py`
- **Responsibilities**:
  - Implement Arrow Flight client
  - Handle local vs remote cache logic
  - Manage local cache storage (if configured)
  - Convert between Python objects and Arrow formats

### Data Structures

**Object (Rust):**
```rust
pub enum ObjectPayload {
    Opaque(Vec<u8>),
    ArrowTable {
        schema: Arc<Schema>,
        batches: Vec<RecordBatch>,
        logical_type: Option<String>,
        adapter: Option<String>,
    },
}

pub struct Object {
    pub version: u64,
    pub payload: ObjectPayload,
    pub deltas: Vec<Object>,
}
```

**ObjectKey (Rust/Python):**
```rust
/// Constant for wildcard session
pub const WILDCARD_SESSION: &str = "*";

/// Parsed object key: `<app_name>/<session_id>/<object_id>`
/// session_id can be "*" for wildcard (all sessions), requires object_id to be None
pub struct ObjectKey {
    pub app_name: String,
    pub session_id: String,      // Can be WILDCARD_SESSION ("*") for all sessions
    pub object_id: Option<String>,  // Must be None when session_id is wildcard
}
```

```python
WILDCARD_SESSION = "*"

@dataclass
class ObjectKey:
    app_name: str
    session_id: str      # Can be WILDCARD_SESSION ("*") for all sessions
    object_id: Optional[str] = None  # Must be None when session_id is wildcard
    
    @classmethod
    def for_all_sessions(cls, app_name: str) -> "ObjectKey":
        """Create wildcard key for all sessions: '<app>/*'."""
        return cls(app_name=app_name, session_id=WILDCARD_SESSION, object_id=None)
    
    def is_all_sessions(self) -> bool:
        """Return True if this key represents all sessions."""
        return self.session_id == WILDCARD_SESSION
```

**ObjectRef (Python):**
```python
@dataclass
class ObjectRef:
    endpoint: str    # Cache server endpoint
    key: str        # "app/session/object"
    version: int    # Server-managed version; 0 forces a fresh read
```

**ObjectMetadata (Rust):**
```rust
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,        // "app/session/object"
    pub version: u64,
    pub size: u64,
}
```

**Storage Organization:**
- Root: `{storage_path}/`
- App directory: `{storage_path}/{app_name}/`
- Session directory: `{storage_path}/{app_name}/{session_id}/`
- Object file: `{storage_path}/{app_name}/{session_id}/{object_id}.arrow`
- Key format: `{app_name}/{session_id}/{object_id}`
- Wildcard key format: `{app_name}/*` (for delete operations across all sessions)

**Arrow IPC File Format:**
- Opaque objects are stored as a single RecordBatch in Arrow IPC file
- Opaque schema: `{version: UInt64, data: Binary}`
- Native tabular objects are stored as Arrow IPC files with their original Arrow schema and reserved `flame.cache.*` schema metadata
- File naming: `{object_id}.arrow`


### Algorithms

**Cache Server Initialization:**
1. Load configuration from flame-cluster.yaml
2. Get network interface name from `cache.network_interface`
3. Look up public IP address from the network interface
4. Construct cache service's public endpoint using public IP and port from configuration
5. Store public endpoint in ObjectCache instance
6. Bind server to both public IP and localhost (127.0.0.1)

**do_put Algorithm:**
1. Receive FlightData stream with RecordBatch
2. Extract key prefix and optional object ID from the Flight descriptor
3. Inspect schema metadata:
   - `flame.cache.format=arrow-table-v1`: collect batches as native tabular payload
   - No native metadata: decode the legacy opaque `{version, data}` payload
4. Generate unique object_id (UUID) when the descriptor only contains a prefix
5. Construct key: `{app_name}/{session_id}/{object_id}`
6. Create session directory if it doesn't exist
7. Write payload to Arrow IPC file:
   - Opaque: `{version, data}` wrapper schema
   - Native tabular: original schema and record batches
8. Update in-memory index: `HashMap<key, ObjectMetadata>`
9. Construct ObjectRef: `{endpoint, key, version}` (using public endpoint from ObjectCache)
10. Serialize ObjectRef to BSON
11. Return PutResult with ObjectRef in app_metadata

**do_get Algorithm:**
1. Extract key from Ticket
2. Parse key to get session_id and object_id
3. Check in-memory index for key
4. If found, read Arrow IPC file: `{storage_path}/{app_name}/{session_id}/{object_id}.arrow`
5. Detect stored payload kind
6. Opaque payload:
   - Deserialize object wrapper and optional deltas
   - Convert response rows to FlightData using `{version, kind, data}`
7. Native tabular payload:
   - Stream original schema and record batches directly
   - Preserve user metadata and reserved cache metadata
8. Stream FlightData to client

**list_flights Algorithm:**
1. Get cache service's public endpoint from ObjectCache (obtained during server construction from flame-cluster.yaml)
2. Iterate through all session directories in storage_path
3. For each session directory:
   - List all `.arrow` files
   - For each file:
     - Extract object_id from filename
     - Construct key: `{app_name}/{session_id}/{object_id}`
     - Create FlightInfo with key as ticket and cache service's public endpoint
     - Stream FlightInfo to client

**Python SDK put_object Algorithm:**
1. Classify the payload:
   - Native tabular: convert to Arrow schema and record batches with `flame.cache.format=arrow-table-v1`
   - Opaque: serialize to RecordBatch with `{version, data}`
2. Check if `cache.storage` is set
3. If set:
   - Write payload to local Arrow IPC storage using the selected format
   - Generate object_id (UUID)
   - Write to local storage: `{cache.storage}/{app_name}/{session_id}/{object_id}.arrow`
   - Connect to cache server using `cache.endpoint`
   - Get flight info using FlightDescriptor with path `{app_name}/{session_id}/{object_id}`
   - Construct ObjectRef with cache server's endpoint from FlightInfo, key `{app_name}/{session_id}/{object_id}`, and server version from cache metadata
4. If not set:
   - Check if `cache.endpoint` is set, else raise exception
   - Connect to remote cache server via endpoint
   - Call do_put to upload the selected payload format
   - Extract ObjectRef from PutResult app_metadata
5. Return ObjectRef

**Key Construction:**
- Format: `{app_name}/{session_id}/{object_id}`
- Wildcard session: `{app_name}/*` - matches all sessions of an application (object_id must be None)
- Session directory creation: Automatically created when first object is stored
- Object ID generation: UUID v4

**Wildcard Session Support:**
The special session_id value `*` (constant: `WILDCARD_SESSION`) indicates all sessions of an application:
- Used primarily for bulk delete operations (e.g., `delete_objects("myapp/*")`)
- When session_id is `*`, object_id must be None (wildcard keys cannot reference specific objects)
- The `ObjectKey.for_all_sessions(app_name)` helper creates wildcard keys
- `ObjectKey.is_wildcard()` returns True when session_id is `*`

### System Considerations

**Performance:**
- Arrow IPC provides efficient serialization with zero-copy capabilities
- In-memory index enables O(1) lookups
- Disk I/O is asynchronous to avoid blocking
- RecordBatch format allows efficient handling of large objects
- Expected latency: <10ms for in-memory operations, <50ms for disk operations
- Throughput: Supports concurrent operations with async/await

**Scalability:**
- Horizontal scaling: Each cache server instance is independent
- Vertical scaling: Limited by disk space and memory for index
- Capacity: Limited by available disk space
- Concurrent operations: Supported via async Rust runtime (Tokio)
- No built-in distributed coordination (single-node design)

**Reliability:**
- Data persistence: Objects survive server restarts
- Atomic writes: Arrow IPC writes are atomic at file level
- Error recovery: Failed operations return appropriate error status
- Failure modes:
  - Disk full: Returns error, doesn't corrupt existing data
  - Network failure: Client receives connection error
  - Server crash: Data persisted to disk is recoverable on restart

**Resource Usage:**
- Memory: In-memory index (HashMap) + Arrow buffers during operations
- Disk: One Arrow IPC file per object
- CPU: Arrow serialization/deserialization overhead
- Network: Arrow Flight over gRPC (efficient binary protocol)

**Security:**
- No authentication/authorization in this version
- File permissions: Cache files should be readable/writable by cache server process only
- Network: Server binds to both public IP and localhost (consider firewall rules)
- Input validation: Validate session_id and object_id formats

**Observability:**
- Logging: Use tracing for structured logging
  - Object put/get operations
  - Storage operations
  - Error conditions
- Metrics: (Future) Consider adding metrics for cache hit/miss, storage usage
- Tracing: Arrow Flight operations can be traced via gRPC interceptors

**Operational:**
- Deployment: Standalone binary, can be deployed as systemd service or container
- Configuration: Via flame-cluster.yaml
- Storage management: Manual cleanup required (no automatic eviction)
- Backup: Standard filesystem backup strategies apply
- Disaster recovery: Restore from filesystem backup

### Dependencies

**External Dependencies:**
- `arrow` (Rust): Apache Arrow Rust implementation
- `arrow-flight` (Rust): Arrow Flight protocol implementation
- `tonic` (Rust): gRPC framework for Rust
- `tokio` (Rust): Async runtime
- `serde` (Rust): Serialization framework
- `bson` (Rust): BSON serialization
- `pyarrow` (Python): Apache Arrow Python bindings (for SDK)

**Internal Dependencies:**
- `common`: FlameClusterContext, FlameError, SessionID types
- `sdk/python`: Python SDK integration

**Version Requirements:**
- Arrow: Latest stable version compatible with arrow-flight
- Python: 3.8+ (for pyarrow compatibility)

## 4. Use Cases

### Basic Use Cases

**Example 1: Python SDK Client Uploading Object to Remote Cache**
- Description: A Python SDK client uploads an object to a remote cache server
- Step-by-step workflow:
  1. Client calls `put_object("app/sess123", my_data)`
  2. SDK checks `cache.storage` - not set
  3. SDK checks `cache.endpoint` - set to "grpc://cache.example.com:9090"
  4. SDK classifies the payload and serializes it as opaque or native tabular Arrow data
  5. SDK connects to cache server via Arrow Flight
  6. SDK calls do_put with FlightDescriptor path `app/sess123`
  7. Cache server generates object_id, creates session directory, writes Arrow IPC file under `app/sess123`
  8. Cache server returns ObjectRef in PutResult
  9. SDK deserializes ObjectRef and returns to client
- Expected outcome: Object is stored on cache server, client receives ObjectRef

**Example 2: Python SDK Client Uploading Object to Local Cache**
- Description: A Python SDK client uploads an object using local storage
- Step-by-step workflow:
  1. Client calls `put_object("app/sess123", my_data)`
  2. SDK checks `cache.storage` - set to "/tmp/flame_cache"
  3. SDK classifies the payload and prepares opaque or native tabular Arrow data
  4. SDK generates object_id (UUID)
  5. SDK writes RecordBatch to `/tmp/flame_cache/app/sess123/{object_id}.arrow`
  6. SDK connects to cache server using `cache.endpoint`
  7. SDK gets flight info using FlightDescriptor with path `app/sess123/{object_id}`
  8. SDK constructs ObjectRef with cache server's endpoint from FlightInfo, key `app/sess123/{object_id}`, and server version from cache metadata
  9. SDK returns ObjectRef to client
- Expected outcome: Object is stored locally, client receives ObjectRef with remote endpoint from FlightInfo

**Example 3: Retrieving Cached Object**
- Description: A client retrieves a previously cached object
- Step-by-step workflow:
  1. Client has ObjectRef: `{endpoint: "grpc://cache.example.com:9090", key: "app/sess123/obj456", version: 1}`
  2. Client calls `get_object(ref)`
  3. SDK connects to cache server using ref.endpoint
  4. SDK calls do_get with ticket = ref.key
  5. Cache server looks up key in index
  6. Cache server reads Arrow IPC file from disk
  7. Cache server streams RecordBatch to client
  8. SDK deserializes RecordBatch to Python object
  9. SDK returns object to client
- Expected outcome: Object is retrieved and deserialized

**Example 4: Updating Cached Object**
- Description: A client updates an existing cached object
- Step-by-step workflow:
  1. Client has ObjectRef and new object data
  2. Client calls `update_object(ref, new_obj)`
  3. SDK serializes new object to RecordBatch
  4. SDK calls do_put with same key (overwrites existing)
  5. Cache server writes new data to existing Arrow IPC file
  6. Cache server updates metadata and increments the object version
  7. Cache server returns updated ObjectRef
  8. SDK returns ObjectRef to client
- Expected outcome: Object is updated, same ObjectRef returned

**Example 5: Listing All Cached Objects**
- Description: Administrator lists all objects in cache
- Step-by-step workflow:
  1. Client calls list_flights()
  2. Cache server retrieves public endpoint from ObjectCache (obtained during server construction)
  3. Cache server iterates through storage_path directories
  4. For each session directory, lists all .arrow files
  5. For each file, creates FlightInfo with key as ticket and cache service's public endpoint
  6. Cache server streams FlightInfo to client
  7. Client receives list of all cached objects with their keys and endpoints
- Expected outcome: Complete list of all cached objects with their keys and cache service endpoints

**Example 6: Native DataFrame Upload and Retrieval**
- Description: A Python SDK client stores a DataFrame without wrapping it in pickled bytes
- Step-by-step workflow:
  1. Client calls `put_object("app/sess123", dataframe)`
  2. SDK recognizes the DataFrame as a tabular payload and converts it to `pyarrow.Table`
  3. SDK adds reserved schema metadata such as `flame.cache.format=arrow-table-v1` and `flame.cache.logical_type=pandas.dataframe`
  4. SDK streams the table batches directly with Arrow Flight `do_put`
  5. Cache server persists the original Arrow schema and batches in `{storage_path}/app/sess123/{object_id}.arrow`
  6. Client calls `get_object(ref)`
  7. Cache server streams the table schema and batches directly with Arrow Flight `do_get`
  8. SDK reconstructs the original DataFrame type when the dependency is available, otherwise returns `pyarrow.Table`
- Expected outcome: Tabular data avoids cloudpickle and avoids the opaque `{version, data}` wrapper.

### Advanced Use Cases

**Example 7: RL Module Using Cache for RunnerContext**
- Description: RL module stores RunnerContext in cache for remote execution
- Step-by-step workflow:
  1. RL module creates RunnerService with execution object
  2. RunnerService serializes RunnerContext using cloudpickle
  3. RunnerService calls put_object with session_id and serialized context
  4. Cache stores context and returns ObjectRef
  5. RunnerService encodes ObjectRef to bytes for core API
  6. Core API stores ObjectRef bytes as common_data
  7. Remote executor retrieves common_data, decodes ObjectRef
  8. Remote executor calls get_object to retrieve RunnerContext
  9. Remote executor deserializes and uses RunnerContext
- Expected outcome: RunnerContext is cached and accessible to remote executors

**Example 8: Cache Server Restart Recovery**
- Description: Cache server restarts and recovers persisted objects
- Step-by-step workflow:
  1. Cache server starts up
  2. Server scans storage_path for all session directories
  3. For each .arrow file found, server reads metadata
  4. Server rebuilds in-memory index from disk files
  5. Server is ready to serve cached objects
- Expected outcome: All previously cached objects are available after restart

## 5. References

### Related Documents
- RFE318 GitHub Issue: https://github.com/xflops/flame/issues/318
- Apache Arrow Documentation: https://arrow.apache.org/docs/
- Arrow Flight Documentation: https://arrow.apache.org/docs/format/Flight.html

### External References
- Apache Arrow IPC Format: https://arrow.apache.org/docs/format/Columnar.html#ipc-format
- Arrow Flight Protocol: https://arrow.apache.org/docs/format/Flight.html
- PyArrow Documentation: https://arrow.apache.org/docs/python/

### Implementation References
- Code location: `object_cache/` directory
- Python SDK cache: `sdk/python/src/flamepy/core/cache.py`
- FlameClusterContext: `common/src/ctx.rs`
- ObjectRef definition: `sdk/python/src/flamepy/core/cache.py`
- RL module usage: `sdk/python/src/flamepy/rl/runner.py`
- Service module usage: `sdk/python/src/flamepy/service/instance.py`

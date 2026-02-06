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

## 2. Function Specification

### Configuration

**FlameClusterContext for Cluster (flame-cluster.yaml):**
- Add a new `storage` field under the `cache` section to specify the local storage path for cache persistence.
- The storage path should be a directory path where Arrow IPC files will be stored.
- If not specified, the cache will operate in-memory only (backward compatible behavior).

Example configuration:
```yaml
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"  # New field
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

### API

**Arrow Flight Protocol:**
The cache server implements the Arrow Flight protocol with the following operations:

1. **do_put**: Upload an object to the cache
   - Request: Streaming FlightData containing RecordBatch with object data
   - Metadata: `session_id:{id}` in app_metadata
   - Response: PutResult with ObjectRef in app_metadata (BSON format)
   - Behavior: Persists data to disk using Arrow IPC, returns ObjectRef

2. **do_get**: Retrieve an object from the cache
   - Request: Ticket containing key (`ssn_id/object_id`)
   - Response: Streaming FlightData containing RecordBatch with object data
   - Behavior: Reads data from disk using Arrow IPC

3. **get_flight_info**: Get metadata about a flight
   - Request: FlightDescriptor with path (`{application_id}/{session_id}/{object_id}`)
   - Response: FlightInfo with schema information

4. **list_flights**: List all cached objects
   - Request: Criteria (optional filtering)
   - Response: Streaming FlightInfo for all objects, aligned with key structure

5. **do_action**: Perform cache operations (PUT, UPDATE, DELETE)
   - PUT: Put object (legacy support)
   - UPDATE: Update existing object
   - DELETE: Delete session and all its objects

**ObjectRef Structure:**
The ObjectRef structure is updated to include:
```python
@dataclass
class ObjectRef:
    endpoint: str      # The endpoint of cache server (e.g., "grpc://127.0.0.1:9090")
    key: str          # The key of object (e.g., "ssn_id/object_id")
    version: int      # Version number (always 0 for now)
```

**Error Handling:**
- If cache storage is not set and endpoint is not set: raise `FlameError(INVALID_CONFIG, "Cache endpoint not configured")`
- If object not found: return `Status::not_found`
- If storage path is invalid: return `Status::invalid_argument`
- If Arrow IPC operations fail: return `Status::internal` with error details

### CLI

**flame-object-cache library:**
- Embedded in `flame-executor-manager`
- Runs as a dedicated thread
- Options:
  - `-c, --config <PATH>`: Path to flame cluster configuration file (default: `~/.flame/flame-cluster.yaml`)
- Exit codes:
  - `0`: Success
  - `1`: Configuration error
  - `2`: Server startup error
  - `3`: Runtime error

### Other Interfaces

**Python SDK Interface:**

1. **put_object(application_id: str, session_id: str, obj: Any) -> ObjectRef**
   - `application_id`: Required application ID for organizing objects (first parameter)
   - `session_id`: The session ID for the object
   - `obj`: The object to cache (will be pickled)
   - If `cache.storage` is set:
     - Write data to local storage using Arrow IPC
     - Get flight info to construct an ObjectRef with cache's remote endpoint
   - If `cache.storage` is not set:
     - Connect to remote cache server via endpoint
     - Use Arrow Flight do_put to upload object
   - If endpoint is not set: raise exception
   - Returns ObjectRef with key format "application_id/session_id/object_id"

2. **get_object(ref: ObjectRef) -> Any**
   - Connect to cache server using ref.endpoint
   - Use Arrow Flight do_get with ref.key as ticket
   - Deserialize and return object

3. **update_object(ref: ObjectRef, new_obj: Any) -> ObjectRef**
   - Re-use do_put to overwrite old data
   - No version check for now (version always 0)
   - Returns updated ObjectRef

### Scope

**In Scope:**
- Implementation of flame-cache library in Rust, integrated into executor-manager
- Arrow Flight server implementation
- Arrow IPC persistence to disk
- Python SDK integration with Arrow Flight client
- Local and remote cache access patterns
- ObjectRef structure updates
- Key-based storage organization (`ssn_id/object_id`)
- Support for both public IP and localhost binding
- Implementation updates for common data in both RL and agent modules

**Out of Scope:**
- Version checking and conflict resolution (version always 0 for now)
- Cache eviction policies
- Distributed cache coordination
- Cache replication
- Authentication and authorization
- Cache size limits and quotas
- Cache statistics and monitoring (beyond basic logging)

**Limitations:**
- Version is always 0; no version conflict detection
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
                └── ssn_id/
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
pub struct Object {
    pub version: u64,
    pub data: Vec<u8>,
}
```

**ObjectRef (Python):**
```python
@dataclass
class ObjectRef:
    endpoint: str    # Cache server endpoint
    key: str        # "ssn_id/object_id"
    version: int    # Always 0 for now
```

**ObjectMetadata (Rust):**
```rust
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,        // New field: "ssn_id/object_id"
    pub version: u64,
    pub size: u64,
}
```

**Storage Organization:**
- Root: `{storage_path}/`
- Application directory: `{storage_path}/{application_id}/`
- Session directory: `{storage_path}/{application_id}/{session_id}/`
- Object file: `{storage_path}/{application_id}/{session_id}/{object_id}.arrow`
- Key format: `{application_id}/{session_id}/{object_id}`

**Arrow IPC File Format:**
- Each object stored as a single RecordBatch in Arrow IPC file
- Schema: `{version: UInt64, data: Binary}`
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
2. Extract application_id, session_id, and object_id from FlightDescriptor path or app_metadata
3. Generate unique object_id (UUID) if not provided
4. Construct key: `{application_id}/{session_id}/{object_id}`
5. Create application/session directory structure if it doesn't exist
6. Write RecordBatch to Arrow IPC file: `{storage_path}/{application_id}/{session_id}/{object_id}.arrow`
7. Update in-memory index: `HashMap<key, ObjectMetadata>`
8. Construct ObjectRef: `{endpoint, key, version: 0}` (using public endpoint from ObjectCache)
9. Serialize ObjectRef to BSON
10. Return PutResult with ObjectRef in app_metadata

**do_get Algorithm:**
1. Extract key from Ticket
2. Parse key to get application_id, session_id, and object_id
3. Check in-memory index for key
4. If found, read Arrow IPC file: `{storage_path}/{application_id}/{session_id}/{object_id}.arrow`
5. Deserialize RecordBatch from file
6. Convert RecordBatch to FlightData
7. Stream FlightData to client

**list_flights Algorithm:**
1. Get cache service's public endpoint from ObjectCache (obtained during server construction from flame-cluster.yaml)
2. Iterate through all session directories in storage_path
3. For each session directory:
   - List all `.arrow` files
   - For each file:
     - Extract application_id, session_id, and object_id from directory structure and filename
     - Construct key: `{application_id}/{session_id}/{object_id}`
     - Create FlightInfo with key as ticket and cache service's public endpoint
     - Stream FlightInfo to client

**Python SDK put_object Algorithm:**
1. Check if `cache.storage` is set
2. If set:
   - Serialize object to RecordBatch
   - Generate object_id (UUID)
   - Write to local storage: `{cache.storage}/{application_id}/{session_id}/{object_id}.arrow`
   - Connect to cache server using `cache.endpoint`
   - Get flight info using FlightDescriptor with path `{application_id}/{session_id}/{object_id}`
   - Construct ObjectRef with cache server's endpoint from FlightInfo, key `{application_id}/{session_id}/{object_id}`, and version 0
3. If not set:
   - Check if `cache.endpoint` is set, else raise exception
   - Connect to remote cache server via endpoint
   - Call do_put with path `{application_id}/{session_id}/{object_id}` to upload object
   - Extract ObjectRef from PutResult app_metadata
4. Return ObjectRef

**Key Construction:**
- Format: `{application_id}/{session_id}/{object_id}`
- Session directory creation: Automatically created when first object is stored
- Object ID generation: UUID v4

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
- Input validation: Validate application_id, session_id, and object_id formats

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
  1. Client calls `put_object(application_id="my-app", session_id="sess123", obj=my_data)`
  2. SDK checks `cache.storage` - not set
  3. SDK checks `cache.endpoint` - set to "grpc://cache.example.com:9090"
  4. SDK serializes object to RecordBatch
  5. SDK connects to cache server via Arrow Flight
  6. SDK calls do_put with RecordBatch and path "my-app/sess123/{object_id}"
  7. Cache server generates object_id, creates application/session directory, writes Arrow IPC file
  8. Cache server returns ObjectRef in PutResult with key "my-app/sess123/{object_id}"
  9. SDK deserializes ObjectRef and returns to client
- Expected outcome: Object is stored on cache server, client receives ObjectRef

**Example 2: Python SDK Client Uploading Object to Local Cache**
- Description: A Python SDK client uploads an object using local storage
- Step-by-step workflow:
  1. Client calls `put_object(application_id="my-app", session_id="sess123", obj=my_data)`
  2. SDK checks `cache.storage` - set to "/tmp/flame_cache"
  3. SDK serializes object to RecordBatch
  4. SDK generates object_id (UUID)
  5. SDK writes RecordBatch to `/tmp/flame_cache/my-app/sess123/{object_id}.arrow`
  6. SDK connects to cache server using `cache.endpoint`
  7. SDK gets flight info using FlightDescriptor with path `my-app/sess123/{object_id}`
  8. SDK constructs ObjectRef with cache server's endpoint from FlightInfo, key `my-app/sess123/{object_id}`, and version 0
  9. SDK returns ObjectRef to client
- Expected outcome: Object is stored locally, client receives ObjectRef with remote endpoint from FlightInfo

**Example 3: Retrieving Cached Object**
- Description: A client retrieves a previously cached object
- Step-by-step workflow:
  1. Client has ObjectRef: `{endpoint: "grpc://cache.example.com:9090", key: "sess123/obj456", version: 0}`
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
  6. Cache server updates metadata (version remains 0)
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

### Advanced Use Cases

**Example 6: RL Module Using Cache for RunnerContext**
- Description: RL module stores RunnerContext in cache for remote execution
- Step-by-step workflow:
  1. RL module creates RunnerService with execution object
  2. RunnerService serializes RunnerContext using cloudpickle
  3. RunnerService calls put_object with application_id, session_id, and serialized context
  4. Cache stores context and returns ObjectRef with key "application_id/session_id/object_id"
  5. RunnerService encodes ObjectRef to bytes for core API
  6. Core API stores ObjectRef bytes as common_data
  7. Remote executor retrieves common_data, decodes ObjectRef
  8. Remote executor calls get_object to retrieve RunnerContext
  9. Remote executor deserializes and uses RunnerContext
- Expected outcome: RunnerContext is cached and accessible to remote executors

**Example 7: Cache Server Restart Recovery**
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
- Agent module usage: `sdk/python/src/flamepy/agent/instance.py`

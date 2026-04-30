# RFE426: Object Versioning for Client-Side Cache

## 1. Motivation

**Background:**

Currently, when a Python SDK client calls `get_object`, it always downloads the full object data from the cache server, even if the data has not changed since the last retrieval. This creates several inefficiencies:

1. **Unnecessary Network Traffic**: Large objects are transferred over the network repeatedly when unchanged.
2. **Increased Latency**: Each `get_object` call incurs full download latency, regardless of whether data changed.
3. **Resource Waste**: Both client and server spend CPU cycles serializing/deserializing unchanged data.
4. **Memory Pressure**: The object cache runs embedded within `flame-executor-manager`, causing OOM issues when caching large objects.

**Target:**

This design addresses both problems with a two-part solution:

1. **Dedicated Binary (`flame-object-cache`)**: Extract the object cache into a standalone process to isolate memory usage from executor-manager.

2. **Object Versioning & Client-Side Caching**: Avoid unnecessary downloads by tracking object versions and caching on the client side:
   - Each object maintains a monotonically increasing version number
   - Python SDK maintains a local cache of downloaded objects
   - Conditional get allows clients to check if data changed before downloading
   - Minimal API changes preserve backward compatibility

## 2. Function Specification

### Configuration

**No configuration changes required.**

The client-side cache operates in-memory within the Python process. Future enhancements may add configuration for:
- Maximum cache size
- Cache eviction policy
- Persistence to disk

### API

#### Object and ObjectMetadata Changes

**ObjectMetadata (Rust):**
```rust
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,      // Now incremented on each mutation
    pub size: u64,
    pub delta_count: u64,
}
```

**ObjectRef (Python):**
```python
@dataclass
class ObjectRef:
    endpoint: str
    key: str
    version: int  # Now meaningful - tracks object version
```

#### Version Semantics

| Operation | Version Behavior |
|-----------|-----------------|
| `put_object(key, obj)` | Server creates object with `version=1` |
| `update_object(ref, obj)` | Server increments: `version += 1` |
| `patch_object(ref, delta)` | Server increments: `version += 1` |
| `get_object(ref)` | Returns object with current server version |

**Key Points:**
- Version is **server-controlled** - client cannot set version on writes
- Version always increments monotonically on mutations
- Returned `ObjectRef` contains the new version after write operations

**Reserved Values:**
- `version=0`: Unconditional get (bypass client cache)
- `version>=1`: Normal versioned object

#### Conditional Get Protocol

**New Flight Action: `GET`**

Following the existing action pattern (`PUT`, `UPDATE`, `PATCH`, `DELETE`), we add a `GET` action for conditional retrieval.

Request body format: `{key}:{client_version}`

Response (JSON):
- If `server_version == client_version`: `{"status": "not_modified", "version": <version>}`
- If `server_version != client_version`: `{"status": "modified", "version": <version>, "data": <base64>, "deltas": [<base64>, ...]}`

When modified, the response includes the full object data and deltas encoded as base64, avoiding a second round-trip to fetch the data.

#### Python SDK API

**No API changes to `get_object`:**
```python
def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    """Get an object from the cache.
    
    Internally uses client-side caching and conditional get to avoid
    unnecessary downloads when the object hasn't changed.
    
    To force a fresh download (bypass cache), set ref.version = 0.
    """
```

#### Version 0 Semantics

`version=0` has special meaning: **unconditional operation**.

| Operation | `version=0` Behavior | Server Response |
|-----------|---------------------|-----------------|
| `get_object` | Always download from server (bypass client cache) | Returns object with current version |

**Note:** For `put_object`, `update_object`, and `patch_object`, version is **server-controlled** and always increments by 1. The client's `ref.version` is not used for writes - the server determines the new version.

This provides a clean way to bypass client-side caching:

```python
# Normal get - uses cache if version matches
obj = get_object(ref)

# Force refresh - bypass cache, always download
ref.version = 0
obj = get_object(ref)  # Always downloads fresh data
```

**Server behavior:**
- `GET_IF_MODIFIED` with `client_version=0`: Always returns full object (never "not_modified")
- New objects via `put_object`: Always assigned `version=1`
- `update_object` / `patch_object`: Always increments `version += 1`
- Legacy objects with `version=0` are treated as unversioned (always transfer)

### Client-Side Cache

**Cache Structure:**
```python
@dataclass
class Object:
    version: int           # Version when object was cached
    data: Any             # Deserialized base object
    deltas: List[Any]     # Deserialized delta objects

# Global cache, keyed by (endpoint, key)
_object_cache: Dict[Tuple[str, str], Object] = {}
```

**Cache Behavior:**

1. On `get_object(ref)`:
   - **Always** call `GET_IF_MODIFIED` with cached version (or 0 if not cached / `ref.version == 0`)
   - If `not_modified`: return cached object
   - If `modified`: deserialize data/deltas from response, update cache, return object

2. On `put_object`/`update_object`/`patch_object`:
   - Server returns new `ObjectRef` with incremented version
   - Invalidate local cache entry (will be refreshed on next get)

### Scope

**In Scope:**
- Version field tracking in `Object` and `ObjectMetadata`
- Version increment on mutations (`put`, `update`, `patch`)
- `GET_IF_MODIFIED` Flight action
- Python SDK client-side object cache
- Conditional get logic in `get_object`
- Dedicated `flame-object-cache` binary (separate from executor-manager)

**Out of Scope:**
- Client-side cache persistence to disk
- Cache size limits / eviction policy (use simple dict for now)
- Cache invalidation across processes
- Distributed cache coherence
- Version conflict detection / optimistic locking

**Limitations:**
- Client-side cache is per-process (no cross-process sharing)
- No TTL-based invalidation (rely on version check)
- No maximum cache size (potential memory growth)
- Version check requires round-trip even when cache hit

### Feature Interaction

**Related Features:**
- **RFE318 (Object Cache)**: This builds on the existing cache architecture
- **RFE366 (LRU Policy)**: Server-side eviction unrelated to client cache
- **Session Management**: Objects are organized by session

**Updates Required:**
1. **object_cache/src/cache.rs**: 
   - Increment version on `put`, `update`, `patch`
   - Add `GET_IF_MODIFIED` action handler
   - Initialize version to 1 on new objects

2. **sdk/python/src/flamepy/core/cache.py**:
   - Add client-side cache dictionary
   - Modify `get_object` to always check with server
   - Add `GET_IF_MODIFIED` action call
   - Update `put_object`/`update_object`/`patch_object` to invalidate cache

3. **New binary: `flame-object-cache`**:
   - Extract cache server from executor-manager into standalone binary
   - Add systemd service file
   - Update deployment configurations

**Compatibility:**
- Backward compatible: Existing `get_object` calls work unchanged
- Legacy objects with `version=0` treated as "always modified"
- `GET_IF_MODIFIED` is optional; clients can still use `do_get` directly

### Dedicated Binary: flame-object-cache

**Motivation:**

Currently, the object cache runs as an embedded library within `flame-executor-manager`. This creates memory pressure issues:
- Large cached objects consume executor-manager memory
- OOM in cache can crash the entire executor-manager
- Cache memory cannot be independently tuned or monitored

**Solution:**

Extract the object cache into a dedicated binary `flame-object-cache` that runs as a separate process.

**Benefits:**
- **Memory isolation**: Cache OOM doesn't affect executor-manager
- **Independent scaling**: Cache resources can be tuned separately
- **Survivability**: Cache survives executor-manager restarts
- **Monitoring**: Easier to monitor cache-specific metrics

**Binary Structure:**

```
flame-object-cache/
├── Cargo.toml
├── src/
│   └── main.rs          # Entry point, config loading, server startup
└── README.md
```

**Configuration (flame-cluster.yaml):**

```yaml
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"
  eviction:
    policy: "lru"
    max_memory: 10737418240  # 10GB
    max_objects: 10000
```

**Deployment Options:**

1. **Systemd service:**
```ini
[Unit]
Description=Flame Object Cache
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/flame-object-cache --config /etc/flame/flame-cluster.yaml
Restart=always
MemoryMax=12G

[Install]
WantedBy=multi-user.target
```

2. **Docker Compose:**
```yaml
services:
  flame-object-cache:
    image: ghcr.io/xflops/flame-object-cache:latest
    ports:
      - "9090:9090"
    volumes:
      - ./flame-cluster.yaml:/etc/flame/flame-cluster.yaml
      - cache-data:/var/lib/flame/cache
    deploy:
      resources:
        limits:
          memory: 12G
```

3. **flmadm install:**

`flmadm` will be updated to install `flame-object-cache` as a systemd service with a new `--cache` profile:

```bash
# Install all flame components including object cache
sudo flmadm install --all

# Install only the object cache
sudo flmadm install --cache

# Install cache with worker (typical deployment)
sudo flmadm install --worker --cache --enable

# Install and enable services
sudo flmadm install --cache --enable

# Manage services via systemctl
sudo systemctl start flame-object-cache
sudo systemctl stop flame-object-cache
sudo systemctl status flame-object-cache

# View logs
sudo journalctl -u flame-object-cache -f
```

**Installation Profiles:**

| Profile | Components |
|---------|------------|
| `--control-plane` | flame-session-manager, flmctl, flmadm |
| `--worker` | flame-executor-manager, flmping-service, flmexec-service, flamepy |
| `--cache` | flame-object-cache |
| `--client` | flmping, flmexec, flamepy |
| `--all` | All of the above |

**Migration Path:**

1. **Phase 1**: Release `flame-object-cache` binary alongside embedded cache
   - Both modes supported via config flag
   - Default: embedded (backward compatible)

2. **Phase 2**: Deprecate embedded mode
   - Log warning when using embedded mode
   - Documentation updates

3. **Phase 3**: Remove embedded mode
   - `flame-object-cache` becomes required component

## 3. Implementation Detail

### Architecture

```
┌─────────────────────────────────────────────────────┐
│                    Python SDK                       │
├─────────────────────────────────────────────────────┤
│  get_object(ref)                                    │
│       │                                             │
│       ▼                                             │
│  ┌─────────────────┐                                │
│  │ Client Cache    │ ◄── Dict[(endpoint,key), Obj]  │
│  │ (in-memory)     │                                │
│  └────────┬────────┘                                │
│           │                                         │
│           │ Always call GET_IF_MODIFIED             │
│           ▼                                         │
│  ┌─────────────────┐                                │
│  │ GET_IF_MODIFIED │                                │
│  │ Action          │                                │
│  └────────┬────────┘                                │
│           │                                         │
│           ├── not_modified ──► return cached object │
│           │                                         │
│           └── modified ──► deserialize data/deltas  │
│                            from response, update    │
│                            cache, return object     │
└───────────┼─────────────────────────────────────────┘
            │
            │ Arrow Flight (gRPC)
            ▼
┌─────────────────────────────────────────────────────┐
│         flame-object-cache (Dedicated Binary)       │
├─────────────────────────────────────────────────────┤
│  ┌─────────────────┐    ┌─────────────────┐         │
│  │ Version Tracker │    │ Object Storage  │         │
│  │ (per object)    │    │ (Arrow IPC)     │         │
│  └─────────────────┘    └─────────────────┘         │
│                                                     │
│  GET_IF_MODIFIED:                                   │
│  - not_modified: return {status, version}           │
│  - modified: return {status, version, data, deltas} │
└─────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────┐
│              flame-executor-manager                 │
│  (No longer contains object cache - memory saved)   │
└─────────────────────────────────────────────────────┘
```

### Components

**1. flame-object-cache Binary (New)**

Location: `flame-object-cache/` directory in workspace

Responsibilities:
- Run as standalone process
- Implement Arrow Flight server
- Manage object storage and retrieval
- Handle version tracking
- Persist data using Arrow IPC

**2. Version Tracker (Rust - object_cache/src/cache.rs)**

Responsibilities:
- Initialize version to 1 on new objects
- Increment version on mutations
- Return version in ObjectMetadata
- Handle `GET_IF_MODIFIED` action

**3. Client Cache (Python - sdk/python/src/flamepy/core/cache.py)**

Responsibilities:
- Maintain in-memory cache of deserialized objects
- Always check with server on `get_object` calls
- Call `GET_IF_MODIFIED` for version validation
- Invalidate cache entries on mutations

### Data Structures

**Server-Side (Rust):**

```rust
// Object struct already has version field
pub struct Object {
    pub version: u64,     // Now meaningful: incremented on mutation
    pub data: Vec<u8>,
    pub deltas: Vec<Object>,
}

// ObjectMetadata already has version field
pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,     // Now meaningful: tracks object version
    pub size: u64,
    pub delta_count: u64,
}
```

**Client-Side (Python):**

```python
@dataclass
class Object:
    """Cached object with version information."""
    version: int
    data: Any  # Deserialized base object
    deltas: List[Any]  # Deserialized delta objects

# Module-level cache
_object_cache: Dict[Tuple[str, str], Object] = {}
_cache_lock = threading.Lock()  # Thread-safe access
```

### Algorithms

**Version Initialization (put):**
```rust
async fn put(&self, session_id: SessionID, object: Object) -> Result<ObjectMetadata> {
    // Create object with version 1
    let versioned_object = Object::new(1, object.data);
    // ... store and return metadata with version = 1
}
```

**Version Increment (update/patch):**
```rust
async fn update(&self, key: String, new_object: Object) -> Result<ObjectMetadata> {
    // Get current version
    let current = self.get(key.clone()).await?;
    let new_version = current.version + 1;
    
    // Create new object with incremented version
    let versioned = Object::new(new_version, new_object.data);
    // ... store and return metadata
}

async fn patch(&self, key: String, delta: Object) -> Result<ObjectMetadata> {
    // Increment version on patch
    let current_meta = self.metadata.get(&key)?;
    let new_version = current_meta.version + 1;
    // ... append delta, update metadata with new version
}
```

**GET_IF_MODIFIED Action Handler:**
```rust
async fn handle_get_if_modified(&self, action_body: &str) -> Result<String> {
    // Parse "key:version"
    let (key, client_version) = parse_action_body(action_body)?;
    let client_version: u64 = client_version.parse()?;
    
    // Get current metadata
    let metadata = self.metadata.get(&key)?;
    
    // version=0 means "always fetch" (force refresh)
    if client_version != 0 && metadata.version == client_version {
        // Not modified - return status only
        Ok(json!({
            "status": "not_modified",
            "version": metadata.version
        }).to_string())
    } else {
        // Modified or force refresh - return full object with data
        let object = self.get(key).await?;
        let deltas_b64: Vec<String> = object.deltas.iter()
            .map(|d| base64::encode(&d.data))
            .collect();
        
        Ok(json!({
            "status": "modified",
            "version": metadata.version,
            "data": base64::encode(&object.data),
            "deltas": deltas_b64
        }).to_string())
    }
}
```

**Client-Side Cache Get:**
```python
def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    cache_key = (ref.endpoint, ref.key)
    
    # version=0 means force refresh - skip cache check
    if ref.version == 0:
        cached_version = 0
    else:
        # Get cached version (0 if not cached)
        with _cache_lock:
            cached = _object_cache.get(cache_key)
        cached_version = cached.version if cached else 0
    
    # Always check with server (passes cached_version, or 0 if not cached/force refresh)
    response = _check_with_server(ref, cached_version)
    
    if response.status == "not_modified" and cached is not None:
        # Server confirmed not modified - return cached data
        if deserializer:
            return deserializer(cached.data, cached.deltas)
        return cached.data
    
    # Modified - server returned data inline
    data = cloudpickle.loads(base64.b64decode(response.data))
    deltas = [cloudpickle.loads(base64.b64decode(d)) for d in response.deltas]
    
    # Update cache
    with _cache_lock:
        _object_cache[cache_key] = Object(
            version=response.version,
            data=data,
            deltas=deltas,
        )
    
    if deserializer:
        return deserializer(data, deltas)
    return data

def _check_with_server(ref: ObjectRef, cached_version: int) -> GetIfModifiedResponse:
    """Check with server if object was modified.
    
    Args:
        ref: Object reference
        cached_version: Version in local cache (0 if not cached or force refresh)
    
    Returns:
        Response with status, version, and optionally data/deltas if modified
    """
    tls_config = _get_cache_tls_config()
    client = _get_flight_client(ref.endpoint, tls_config)
    
    action_body = f"{ref.key}:{cached_version}"
    action = flight.Action("GET_IF_MODIFIED", action_body.encode())
    
    results = list(client.do_action(action))
    if not results:
        raise ValueError("No response from GET_IF_MODIFIED")
    
    return GetIfModifiedResponse.from_json(results[0].body.to_pybytes())
```

### System Considerations

**Performance:**
- Cache hit with version match: ~1-5ms (single action call)
- Cache miss: Same as current implementation
- Memory overhead: Proportional to cached objects
- Expected speedup: 10-100x for repeated reads of unchanged objects

**Scalability:**
- Client cache is per-process; no coordination overhead
- Server-side version tracking is O(1) per operation
- No additional storage requirements

**Reliability:**
- Cache is optional; failures fall back to full download
- Version mismatch always results in fresh data
- No data consistency issues (version is authoritative)

**Resource Usage:**
- Memory: Client cache grows with unique objects accessed
- Network: Reduced for repeated reads of unchanged objects
- CPU: Minimal overhead for version comparison

**Security:**
- No new security considerations
- Version is not sensitive information

**Observability:**
- Log cache hits/misses at DEBUG level
- Potential metric: cache hit ratio

### Dependencies

**No new dependencies required.**

Existing dependencies:
- `pyarrow`: Already used for Arrow Flight
- `threading`: Standard library
- `json`: Standard library

## 4. Use Cases

### Basic Use Cases

**Example 1: Repeated Read of Unchanged Object**

Description: Client reads the same object multiple times without modifications.

```python
# First read - downloads full object
ref = put_object("app/session", large_data)
obj1 = get_object(ref)  # Downloads ~100MB

# Second read - cache hit, no download
obj2 = get_object(ref)  # Returns cached, ~5ms

# Third read - still cached
obj3 = get_object(ref)  # Returns cached, ~5ms
```

Expected outcome: First call downloads full object; subsequent calls return cached data after quick version check.

**Example 2: Read After Update**

Description: Client reads object, another client updates it, first client reads again.

```python
# Client A puts object
ref_a = put_object("app/session", data_v1)  # version=1

# Client A caches object
obj = get_object(ref_a)  # Downloads, caches version 1

# Client B updates object (has the same ref)
ref_b = update_object(ref_a, data_v2)  # Server returns version=2

# Client A reads again using its original ref
# Client A's ref still has version=1, but that's OK:
# - Client checks local cache: found, cached_version=1
# - Client calls GET_IF_MODIFIED with cached_version=1
# - Server returns {"status": "modified", "version": 2, "data": ..., "deltas": [...]}
# - Client deserializes data from response (no second round-trip needed)
# - Client updates cache with version=2
obj = get_object(ref_a)  # Detects modification, gets new data inline
```

Expected outcome: Even though Client A has an old `ObjectRef`, the version check with the server detects the modification and returns the new data in a single round-trip.

**Example 3: Force Refresh with version=0**

Description: Client explicitly bypasses cache using version=0.

```python
ref = put_object("app/session", data)
obj = get_object(ref)  # Cached

# Force fresh download by setting version=0
ref.version = 0
obj = get_object(ref)  # Bypasses cache, always downloads
```

Expected outcome: Full download occurs regardless of cache state.

### Advanced Use Cases

**Example 4: Multi-Process Consistency**

Description: Multiple processes sharing ObjectRef.

```python
# Process A puts object
ref = put_object("app/session", shared_data)

# Process A serializes ref to shared storage
save_ref_to_file(ref)

# Process B loads ref and reads
ref = load_ref_from_file()
obj = get_object(ref)  # Downloads (Process B has empty cache)

# Process A modifies
ref = update_object(ref, new_data)

# Process B reads again
obj = get_object(ref)  # Version check fails, downloads new version
```

Expected outcome: Each process maintains its own cache; version checks ensure consistency.

**Example 5: RL Runner Context Caching**

Description: Executor repeatedly fetching RunnerContext.

```python
# Controller creates context
context_ref = put_object("app/session", runner_context)

# Task 1 on executor
context = get_object(context_ref)  # Downloads, caches

# Task 2 on same executor (common data unchanged)
context = get_object(context_ref)  # Cache hit, fast

# Task 3 after context update
context = get_object(context_ref)  # Version changed, downloads new
```

Expected outcome: Executors benefit from caching common data across tasks in the same session.

## 5. References

### Related Documents
- [RFE426 GitHub Issue](https://github.com/xflops/flame/issues/426)
- [RFE318 Object Cache Design](../RFE318-cache/FS.md)
- [RFE366 LRU Policy](../RFE366-lru-policy/)

### Implementation References
- Server cache: `object_cache/src/cache.rs`
- Python SDK cache: `sdk/python/src/flamepy/core/cache.py`
- Object struct: `object_cache/src/cache.rs:Object`
- ObjectRef: `sdk/python/src/flamepy/core/cache.py:ObjectRef`

### External References
- [Apache Arrow Flight](https://arrow.apache.org/docs/format/Flight.html)
- [HTTP ETag semantics](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/ETag) (similar concept)

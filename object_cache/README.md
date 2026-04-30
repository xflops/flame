# Flame Object Cache

Apache Arrow-based object cache service for Flame distributed system.

## Overview

The `flame-object-cache` is a standalone binary that provides persistent object storage using Apache Arrow Flight protocol and Arrow IPC format for efficient serialization. It runs as a dedicated service, enabling centralized caching with version tracking for efficient client-side caching.

## Features

- **Standalone Service**: Runs as a dedicated process/container for centralized caching
- **Version Tracking**: Each object has a version number, incremented on mutations
- **Conditional Get**: Clients can check if their cached copy is still valid (RFE426)
- **Client-Side Caching**: Python SDK caches objects locally, reducing network round-trips
- **Persistent Storage**: Objects are stored on disk using Arrow IPC format
- **Arrow Flight Protocol**: High-performance gRPC-based protocol for data transfer
- **Delta Support**: Append-only patches without rewriting the base object
- **Eviction Policies**: LRU eviction with configurable memory limits

## Configuration

### Server Configuration (`flame-cluster.yaml`)

```yaml
cache:
  endpoint: "grpc://0.0.0.0:9090"
  network_interface: "eth0"
  storage: "fs:///var/lib/flame/cache"
  eviction:
    policy: lru
    max_memory: "8G"
```

### Client Configuration (`flame.yaml`)

```yaml
clusters:
  - name: flame
    endpoint: "http://flame-session-manager:8080"
    cache:
      endpoint: "grpc://flame-object-cache:9090"
      storage: "/tmp/flame_cache"  # Optional: local storage path
```

### Environment Variables

- `FLAME_CACHE_STORAGE`: Override cache storage path
- `FLAME_HOME`: Flame installation directory

## Running

### Standalone Binary

```bash
# Run with default config location
flame-object-cache --config /etc/flame/flame-cluster.yaml

# Run with custom config
flame-object-cache --config ./my-config.yaml
```

### Using flmadm

```bash
# Install cache component only
sudo flmadm install --cache --enable

# Install as part of full deployment
sudo flmadm install --all --enable
```

### Docker

```bash
# Build the image
docker build -f docker/Dockerfile.foc -t xflops/flame-object-cache .

# Run container
docker run -d \
  -v ./flame-cluster.yaml:/root/.flame/flame-cluster.yaml \
  -v flame-cache:/var/lib/flame/cache \
  -p 9090:9090 \
  xflops/flame-object-cache
```

### Docker Compose

```bash
# Start all services including object cache
docker compose up -d

# View cache logs
docker compose logs flame-object-cache

# Stop services
docker compose down
```

## Python SDK Usage

```python
from flamepy.core.cache import put_object, get_object, update_object, patch_object, ObjectRef

# Put an object (returns ObjectRef with version=1)
ref = put_object("app/session", my_data)
print(f"Stored at: {ref.key}, version: {ref.version}")

# Get an object (uses client-side cache if version matches)
data = get_object(ref)

# Force fresh fetch (bypass cache)
ref.version = 0
fresh_data = get_object(ref)

# Update replaces the object (increments version)
new_ref = update_object(ref, new_data)
print(f"New version: {new_ref.version}")  # version=2

# Patch appends delta without replacing base
patched_ref = patch_object(ref, delta_data)

# Get with custom deserializer to combine base + deltas
def merge_lists(base, deltas):
    result = base.copy()
    for delta in deltas:
        result.extend(delta)
    return result

merged = get_object(ref, deserializer=merge_lists)
```

## Version Tracking (RFE426)

Objects have version numbers that enable efficient client-side caching:

1. **PUT**: Creates object with `version=1`
2. **UPDATE**: Replaces object, increments version
3. **PATCH**: Appends delta, increments version
4. **GET**: Conditional get - returns `not_modified` if client has current version

Client-side caching workflow:
```
Client                              Server
  |                                    |
  |-- do_get(key:0) ------------------>|  (version=0 means "give me latest")
  |<-- [Arrow RecordBatch with data] --|  (client caches object, version=1)
  |                                    |
  |-- do_get(key:1) ------------------>|  (client has version 1)
  |<-- [empty stream] -----------------|  (not modified, no data transfer!)
```

## Storage Structure

```
/var/lib/flame/cache/
└── app_name/
    └── session_id/
        ├── object1.arrow       # Base object
        ├── object1.delta.001   # Delta 1
        ├── object1.delta.002   # Delta 2
        └── object2.arrow
```

Each object is stored as an Arrow IPC file with schema: `{version: UInt64, data: Binary}`

## API

The cache server implements the Arrow Flight protocol:

| Operation | Description |
|-----------|-------------|
| `do_put` | Upload or update an object (returns ObjectRef with version) |
| `do_put` (PATCH cmd) | Append delta to existing object (command: `PATCH:{key}`) |
| `do_get` | Retrieve object with conditional version check (ticket: `{key}:{version}`) |
| `get_flight_info` | Get metadata about an object |
| `list_flights` | List all cached objects |
| `do_action(DELETE)` | Delete objects by key prefix (supports `{app}/*` wildcard) |

### Wire Protocol Details

**Conditional GET (`do_get`)**:
- Ticket format: `{key}:{client_version}` (e.g., `app/session/obj1:5`)
- If `client_version == 0`: Always returns full object (force refresh)
- If `client_version == server_version`: Returns empty stream (not modified)
- If versions differ: Returns full object as Arrow RecordBatch

**PUT/UPDATE (`do_put`)**:
- Uses `FlightDescriptor.for_path(key)` to specify object key
- New objects are created with `version=1`
- Existing objects are overwritten (version incremented server-side)

**PATCH (`do_put` with command)**:
- Uses `FlightDescriptor.for_command("PATCH:{key}")` 
- Appends delta to existing object, increments version
- Returns error if base object doesn't exist

## Building

```bash
# Build the standalone binary
cargo build --package flame-object-cache --release

# Run tests
cargo test --package flame-object-cache
```

## Architecture

```
                    ┌──────────────────┐
                    │ flame-object-    │
                    │ cache            │
                    │ (standalone)     │
                    └────────┬─────────┘
                             │ Arrow Flight
         ┌───────────────────┼───────────────────┐
         │                   │                   │
┌────────┴───────┐  ┌────────┴───────┐  ┌────────┴───────┐
│ executor-mgr-1 │  │ executor-mgr-2 │  │ executor-mgr-3 │
│ (worker)       │  │ (worker)       │  │ (worker)       │
└────────────────┘  └────────────────┘  └────────────────┘
```

Benefits of standalone architecture:
- **Centralized caching**: Single cache for all workers
- **Independent scaling**: Scale cache separately from workers
- **Version consistency**: All workers see same object versions
- **Simpler configuration**: Workers just point to cache endpoint

## Systemd Service

When installed with `--cache --enable`, flmadm creates a systemd service:

```bash
# Check status
sudo systemctl status flame-object-cache

# View logs
sudo journalctl -u flame-object-cache -f
tail -f /usr/local/flame/logs/foc.log

# Restart
sudo systemctl restart flame-object-cache
```

## See Also

- Design Document: `docs/designs/RFE318-cache/FS.md`
- Version Tracking: `docs/designs/RFE426-cache-versioning/FS.md`
- Python SDK Cache Module: `sdk/python/src/flamepy/core/cache.py`
- flmadm Documentation: `flmadm/README.md`

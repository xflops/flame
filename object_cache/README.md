# Flame Object Cache

Object cache service for Flame distributed system.

## Overview

The `flame-object-cache` is a standalone service that stores opaque client-encoded bytes. It uses a streaming gRPC API, keeps hot payloads in memory, and persists them as binary files. Clients choose their own value encoding, including Arrow IPC for table values, and provide a data type such as `raw.zstd` when they compress the bytes.

## Features

- **Standalone Service**: Runs as a dedicated process/container for centralized caching
- **Version Tracking**: Each object has a version number, incremented on mutations
- **Conditional Get**: Clients can check if their cached copy is still valid (RFE426)
- **Client-Side Caching**: Python SDK caches objects locally, reducing network round-trips
- **Persistent Storage**: Opaque objects and patches are stored in binary files
- **Streaming gRPC API**: Chunked transfer without server-side value encoding
- **Client-Side Compression**: The Python SDK compresses structured data and tensors; the cache stores their bytes unchanged
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
  gc:
    interval: 60s
```

Stale application-data garbage collection is always enabled. The `gc` section
only overrides its 60-second default interval.

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
from flamepy.core.cache import (
    ObjectRef,
    get_object,
    patch_object,
    put_object,
    update_object,
)

# Put an object (returns ObjectRef with version=1)
ref = put_object("app/session", my_data)
print(f"Stored at: {ref.key}, version: {ref.version}")

# The SDK uses ZSTD for arrays, tables, data frames, and tensors regardless
# of payload size; their data type gains a .zstd suffix. Arbitrary pickled
# objects and raw file uploads, including .tar.gz packages, remain uncompressed.

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
  |-- Get(key, version=0) ------------>|  (version=0 means "give me latest")
  |<-- [header, bytes] ---------------|  (client caches object, version=1)
  |                                    |
  |-- Get(key, version=1) ------------>|  (client has version 1)
  |<-- [NOT_MODIFIED header] ----------|  (no payload transfer)
```

## Storage Structure

```
/var/lib/flame/cache/
└── app_name/
    └── session_id/
        ├── object1.bin          # Opaque base payload with cache header
        ├── object1.deltas/
        │   ├── 0.bin            # Opaque patch
        │   └── 1.bin            # Opaque patch
        └── object2.bin          # Another opaque base payload
```

Opaque payloads are stored directly in binary files with a small cache header
for version, creation time, and client-provided data type. All patches have
the same type as the base. Clients encode Arrow tables to bytes and optionally
compress them before uploading. The cache does not decompress hot or persisted
objects.

## API

The cache server implements `ObjectCacheService` in `cache.proto`:

| Operation | Description |
|-----------|-------------|
| `Put` | Stream a new or replacement object; return its metadata |
| `Patch` | Stream a delta for an existing object; return updated metadata |
| `Get` | Stream a full object, later patches, or a not-modified header |
| `GetMetadata` | Get metadata for one object |
| `List` | Stream metadata for all objects |
| `Delete` | Delete a key or prefix, including `{app}/*` |

### Wire Protocol Details

**Conditional GET (`Get`)**:
- Request: full key and `client_version`
- If `client_version == 0`: Always returns full object (force refresh)
- If `client_version == server_version`: Returns `NOT_MODIFIED` without payload chunks
- If a contiguous patch suffix is available: Returns only later patches
- Otherwise: Returns the base object and all patches
- Chunks for one base or patch share a kind and version. A new part or the end
  of the stream completes that part. Empty parts have one empty chunk.
- The response header carries the shared data type for the base and patches.
- The Python SDK interprets types such as `arrow.table.zstd` and decompresses
  each base or patch before decoding its value. Other clients receive the type
  and bytes unchanged. The cache does not parse the type.

**PUT/UPDATE (`Put`)**:
- Starts with a key or prefix and client-provided data type, followed by
  byte chunks of at most 1 MiB
- New objects are created with `version=1`
- Existing objects are overwritten (version incremented server-side)

**PATCH (`Patch`)**:
- Starts with a full object key and the same data type as the base, followed
  by byte chunks
- Appends delta to existing object, increments version
- Returns error if base object doesn't exist

## Building

```bash
# Build the standalone binary
cargo build --package flame-object-cache --release

# Run tests
cargo test --package flame-object-cache
```

### Performance benchmark

Run the opt-in microbenchmarks with optimized code and one test thread:

```bash
cargo test -p flame-object-cache --release cache_benchmarks -- --ignored --nocapture --test-threads=1
```

The opaque-object benchmark compares copying an 8 MiB payload with sharing its
cached snapshot. The disk benchmarks measure 8 MiB opaque writes and object reloads, plus
sequential 1 KiB patch appends. Reloads bypass the in-memory object cache but
may be served by the operating system's page cache; these timings are not
physical cold-disk measurements.

## Architecture

```
                    ┌──────────────────┐
                    │ flame-object-    │
                    │ cache            │
                    │ (standalone)     │
                    └────────┬─────────┘
                             │ gRPC bytes
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

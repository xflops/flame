# Flame Object Cache

Apache Arrow-based object cache service for Flame distributed system.

## Overview

The flame-object-cache is an embedded library that provides persistent object storage using Apache Arrow Flight protocol and Arrow IPC format for efficient serialization. It runs as a dedicated thread within the flame-executor-manager service.

## Features

- **Persistent Storage**: Objects are stored on disk using Arrow IPC format and survive server restarts
- **Arrow Flight Protocol**: High-performance gRPC-based protocol for data transfer
- **Key-based Organization**: Objects organized by application and session ID (`application_id/session_id/object_id`)
- **In-memory Index**: Fast O(1) lookups with disk-backed persistence
- **Zero-copy Operations**: Leverages Arrow's efficient columnar format

## Configuration

### Server Configuration (`flame-cluster.yaml`)

```yaml
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"  # Optional: disk storage path
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
- `FLAME_CACHE_ENDPOINT`: Override cache endpoint
- `RUST_LOG`: Control log level (e.g., `debug`, `info`, `warn`, `error`)
  - Set to `debug` to see detailed debug logs: `RUST_LOG=flame_cache=debug`
  - Set to `trace` for even more verbose output: `RUST_LOG=flame_cache=trace`
  - Set globally: `RUST_LOG=debug` (shows debug logs from all modules)

## Usage

### Using the Cache

The cache server is automatically started when the executor-manager starts. No separate startup command is needed.

### Python SDK

```python
from flamepy import put_object, get_object, update_object, ObjectRef

# Put an object (application_id is required and is the first parameter)
ref = put_object("my-app", "session123", my_data)
print(f"Stored at: {ref.endpoint}/{ref.key}")

# Get an object
data = get_object(ref)

# Update an object
new_ref = update_object(ref, new_data)
```

## Storage Structure

```
/var/lib/flame/cache/
└── application_id/
    └── session_id/
        ├── object1.arrow
        ├── object2.arrow
        └── object3.arrow
```

Each object is stored as an Arrow IPC file with schema: `{version: UInt64, data: Binary}`

## API

The cache server implements the Arrow Flight protocol:

- **do_put**: Upload an object (returns ObjectRef in BSON format)
- **do_get**: Retrieve an object by key
- **get_flight_info**: Get metadata about an object
- **list_flights**: List all cached objects
- **do_action**: Perform cache operations (PUT, UPDATE, DELETE)

## Building

The cache is built as part of the executor-manager:

```bash
# Build the cache library (part of executor-manager build)
cargo build --package object_cache --release

# Or build the full executor-manager
cargo build --package executor_manager --release
```

## Logging Configuration

The object cache uses the `tracing` crate for logging. Log levels are controlled via the `RUST_LOG` environment variable.

### Enable Debug Logs

To see debug logs from object_cache:

```bash
# Enable debug logs for object_cache only
export RUST_LOG=flame_cache=debug

# Or enable debug logs for all modules
export RUST_LOG=debug

# For even more verbose output (trace level)
export RUST_LOG=flame_cache=trace

# Output ALL logs from all modules (maximum verbosity)
export RUST_LOG=trace

# Output all logs including third-party libraries (override default filters)
export RUST_LOG=trace,h2=trace,hyper_util=trace,tower=trace
```

### Log Level Examples

```bash
# Debug level (recommended for development)
RUST_LOG=flame_cache=debug flame-object-cache --endpoint grpc://127.0.0.1:9090

# Info level (default)
RUST_LOG=flame_cache=info flame-object-cache --endpoint grpc://127.0.0.1:9090

# Trace level (very verbose, for deep debugging)
RUST_LOG=flame_cache=trace flame-object-cache --endpoint grpc://127.0.0.1:9090

# Output ALL logs from all modules (maximum verbosity)
RUST_LOG=trace flame-object-cache --endpoint grpc://127.0.0.1:9090

# Output all logs including network libraries (override default filters)
RUST_LOG=trace,h2=trace,hyper_util=trace,tower=trace flame-object-cache --endpoint grpc://127.0.0.1:9090
```

### Available Log Levels

- `error`: Only error messages
- `warn`: Warnings and errors
- `info`: Informational messages (default)
- `debug`: Debug information including object operations
- `trace`: Very detailed trace information

### Debug Log Output

When debug logging is enabled, you'll see logs for:
- Object put/get/update/delete operations
- Disk I/O operations
- Object loading from disk
- Key format parsing
- Flight protocol operations

### Output All Logs

To output **all logs** including third-party libraries:

```bash
# Maximum verbosity - all modules at trace level
RUST_LOG=trace flame-object-cache --endpoint grpc://127.0.0.1:9090

# Override default filters to see network library logs too
RUST_LOG=trace,h2=trace,hyper_util=trace,tower=trace,sqlx=trace flame-object-cache --endpoint grpc://127.0.0.1:9090

# Or set globally before running
export RUST_LOG=trace
flame-object-cache --endpoint grpc://127.0.0.1:9090
```

**Note**: By default, some third-party libraries (h2, hyper_util, tower, sqlx) have their log levels restricted to reduce noise. To see their logs, explicitly set them to `trace` as shown above.

## Running with Docker Compose

```bash
# Start all services (cache runs embedded in executor-manager)
docker compose up -d

# View cache logs with debug level
RUST_LOG=flame_cache=debug docker compose up

# View cache logs (part of executor-manager logs)
docker compose logs flame-executor-manager | grep cache

# Stop services
docker compose down
```

## Implementation Details

- **Language**: Rust
- **Protocol**: Arrow Flight (gRPC-based)
- **Storage Format**: Arrow IPC
- **Async Runtime**: Tokio
- **Arrow Version**: 53 (compatible with tonic 0.12)

## Architecture

The object cache runs as a dedicated thread within the executor-manager process, providing:

- **Simplified deployment**: No separate service to manage
- **Better locality**: Cache runs alongside executors on the same node
- **Shared configuration**: Uses the same config file as executor-manager
- **Resource efficiency**: Lower overhead than separate service

## Limitations

- Version is always 0 (no version conflict detection)
- No automatic cache cleanup or eviction
- Single-node cache (no distributed coordination)
- No authentication/authorization
- Objects are per-session (no cross-session sharing)

## See Also

- Design Document: `docs/designs/RFE318-cache/FS.md`
- Python SDK Cache Module: `sdk/python/src/flamepy/core/cache.py`

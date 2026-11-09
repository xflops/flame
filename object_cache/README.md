# Flame Object Cache

Object cache service for Flame distributed system.

## Overview

The `flame-object-cache` is a standalone service that stores opaque client-encoded bytes. It uses a streaming gRPC API; persistent PUT and GET process payloads in bounded chunks, while PATCH buffers its uploaded part before appending it. The open files of recently used objects of a persistent store are kept in memory; `storage: none` has no store at all, only the in-memory cache. Clients choose their own value encoding, including Arrow IPC for table values, and provide a data type such as `raw.zstd` when they compress the bytes.

## Features

- **Standalone Service**: Runs as a dedicated process/container for centralized caching
- **Version Tracking**: The version of an object is its number of data parts: 1 when created, incremented by each append
- **Conditional Get**: Clients can check if their cached copy is still valid (RFE426)
- **Client-Side Caching**: Python SDK caches objects locally, reducing network round-trips
- **Payload Integrity**: Persistent stores write a SHA-256 checksum for each payload part and verify it while streaming reads; a checksum mismatch fails the stream
- **Persistent Storage**: An object is a directory of `metadata.json`, `index.bin` and `data.bin` files, or with `storage: none` not at all (the in-memory cache is then the only copy, and nothing survives a restart)
- **Streaming gRPC API**: Chunked transfer without server-side value encoding; persistent PUT and GET use bounded chunk buffers, while PATCH buffers its uploaded part
- **Metadata-Only Startup**: Startup scans object metadata; payloads are never loaded at startup
- **Client-Side Compression**: The Python SDK compresses structured data and tensors; the cache stores their bytes unchanged
- **Create-Only Objects**: An object is created once and then only grows by appended data, like the layers of an overlay filesystem; it is never replaced
- **Eviction Policies**: LRU eviction with `max_memory` and `max_objects` limits for the resident (in-memory) data. With a disk store the resident data is only the handle of an object, its key, data type, number of parts and open `index.bin` and `data.bin`, so reads need no open or close and never hold payload or index bytes: a read seeks to the part's fixed-size record in `index.bin`, then to its payload in `data.bin`. Data is appended through the same handle, so the handle stays valid; an evicted handle is opened again. Under `storage: none` there is no store to read back from: every object is resident with all its parts whatever its size, eviction applies, and an evicted object is lost. A `none` cache cannot stream objects larger than memory, because a write is assembled in memory (bounded by `max_memory`)

## Configuration

### Server Configuration (`flame-cluster.yaml`)

```yaml
cache:
  endpoint: "grpc://0.0.0.0:9090"
  advertise: "eth0"
  storage: "fs:///var/lib/flame/cache"
  eviction:
    policy: lru
    max_memory: "8G"
  gc:
    interval: 60s
```

Object references carry the address the cache advertises. Clients read, patch,
and delete through that address, so each reference identifies the instance
holding its object. The advertised address is selected in this order:

1. `cache.advertise`, when nonempty;
2. the `FLAME_CACHE_ADVERTISE` environment variable, when nonempty;
3. the first private IPv4 address of a non-loopback interface;
4. `cache.endpoint` as configured.

An explicit IP address or interface name/regex selects both the listener IP
and the address in object references. For example, `"eth0"` or `"^eth0$"` uses
the first IPv4 address on the first matching interface in name order;
`"127.0.0.1"` selects loopback. Binding resolves the raw effective advertise
value as an IP or interface selector, without parsing a URL or extracting a
host. The listener port always comes from `cache.endpoint`.

Full URLs, `IP:port` values, and DNS addresses remain valid advertisements but
keep the listener on `0.0.0.0` when they do not match an interface selector.
This includes URLs with numeric IP hosts. They can name a proxy without
restricting the listener. A full URL supplies its own scheme and advertised
port; bare hosts or `host:port` inherit missing values from `cache.endpoint`.
An advertised port override changes only object references. If neither advertise setting is supplied, the listener also
stays on `0.0.0.0`, even when references use an automatically selected private
address. Startup logs show the bind address and advertised endpoint together.

A client using `127.0.0.1` cannot reach a listener bound only to a non-loopback
IP. Point local clients at the selected IP, use a loopback IP/interface for
local-only access, or unset both advertise settings to keep wildcard listening.
The configuration value takes precedence over the environment for both
binding and advertisement.

Stale application-data garbage collection is always enabled. The `gc` section
only overrides its 60-second default interval.

Disk reads and writes use positional file I/O on blocking threads. An object is
created in a temporary directory, synced and renamed into place, so an aborted
or failed upload leaves nothing behind. An append adds its payload to
`data.bin` and is committed by its record in `index.bin`. A `Get` holds the object's
read lock until its response ends, so a slow client delays writers and
deleters of that object: put, patch and delete take the object's write lock,
and a delete of a prefix takes the lock of every object it matches.

Disk storage removes empty session and application directories after deletion.
Startup recovery also removes empty directories left by earlier versions.
Nonempty directories, the cache root, and the filesystem's `lost+found`
directory are preserved.

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

### Using the install script

The object cache runs on each worker, so a worker install includes it:

```bash
# A worker: the executor manager and the object cache, joined to the head
curl -fsSL https://xflops.io/flame/setup.sh | sudo FLAME_ENDPOINT=http://HEAD:8080 sh -s -- --join
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

# Get an object (uses client-side cache if version matches). The value is the
# newest data part, so an object with appended data returns the newest.
data = get_object(ref)

# Force fresh fetch (bypass cache)
ref.version = 0
fresh_data = get_object(ref)

# Patch appends data to the object
patched_ref = patch_object(ref, delta_data)

# Get with a deserializer that makes a value of all the data parts, in the
# order they were written (the first is the data the object was created with)
def merge_lists(parts):
    result = []
    for part in parts:
        result.extend(part)
    return result

merged = get_object(ref, deserializer=merge_lists)
```

## Version Tracking (RFE426)

Objects have version numbers that enable efficient client-side caching:

1. **PUT**: Creates object with `version=1`; an existing key is an error
2. **PATCH**: Appends data, increments version
3. **GET**: Get the data after a version: nothing if the client has the current version

Client-side caching workflow:
```
Client                              Server
  |                                    |
  |-- Get(key, version=0) ------------>|  (version=0 means "give me latest")
  |<-- [header, bytes] ---------------|  (client caches object, version=1)
  |                                    |
  |-- Get(key, version=1) ------------>|  (client has version 1)
  |<-- [header] -----------------------|  (no data: nothing after version 1)
```

## Storage Structure

```text
/var/lib/flame/cache/
└── app_name/
    └── session_id/
        └── object1/
            ├── metadata.json   # shared data type
            ├── index.bin       # one 56-byte record per committed part
            └── data.bin        # append-only payload bytes
```

`metadata.json` contains the client-defined `data_type`.
Every `index.bin` record contains these
fields in order, without a file header:

| Field | Encoding | Bytes |
|-------|----------|-------|
| Payload offset | Little-endian unsigned integer | 8 |
| Payload length | Little-endian unsigned integer | 8 |
| Creation time | Little-endian signed Unix milliseconds | 8 |
| Payload checksum | SHA-256 digest bytes | 32 |

An object's version is its number of committed parts; the first PUT creates
part 1 and every PATCH appends another part. The checksum covers the exact
encoded payload bytes of that part, including an empty payload. Clients encode
and optionally compress their values before upload; the cache checks those
opaque bytes without decoding or decompressing them.

PUT computes its digest incrementally as chunks arrive, syncs the payload,
index and metadata in a temporary directory, and renames the directory into
place only when complete. PATCH buffers its uploaded part, computes its digest,
then writes and syncs that payload before appending and syncing the index record
containing its digest. A complete index record
commits the part. Bytes beyond committed payload ranges can remain after an
interrupted append and are ignored. Opening an object removes an incomplete
trailing index record using its 56-byte record width.

### Integrity and streaming failures

Reads calculate SHA-256 incrementally and compare each delivered part against
its persisted expected checksum. Memory use is bounded by the stream's chunks,
not the size of the object. A modified payload fails even when its length is
unchanged or the object's file handle was already cached.

Earlier chunks may reach a reader before the checksum comparison fails. A
non-OK stream termination invalidates the whole read: clients must discard all
partially delivered data and must not publish a value or mark it cached until
the stream completes successfully. A suffix read verifies the parts it returns;
a conditional GET that returns no payload does not reread or verify bytes the
client already holds. Listing objects and reading metadata do not verify
payload integrity.

These checks detect payload corruption relative to its persisted digest. They
are not authentication: someone able to replace both a payload and its checksum
can make them agree. The digest does not authenticate the data type, timestamps,
or other index fields.

Storage uses `DiskStorage` for persistent files and `ResidentStorage` to cache
open object handles. Persistent PUT and GET stream in bounded chunks; PATCH
buffers its uploaded part before appending it.
`storage: none` instead assembles complete payloads in memory, including PATCH
payloads, and has no persisted integrity record; its contents do not survive a
restart.

## API

The cache server implements `ObjectCacheService` in `cache.proto`:

| Operation | Description |
|-----------|-------------|
| `Put` | Stream a new object; return its metadata |
| `Patch` | Stream a delta for an existing object; return updated metadata |
| `Get` | Stream an object's data after a version: all of it for 0 |
| `GetMetadata` | Get metadata for one object |
| `List` | Stream metadata for all objects |
| `Delete` | Delete a key or prefix, including `{app}/*` |

### Wire Protocol Details

**GET (`Get`)**:
- Request: full key and `version`
- If `version == 0`: Returns all the data of the object (force refresh)
- If `0 < version < server_version`: Returns only the data after `version`
- If `version == server_version`: Returns the header only, no chunks
- If `version` is ahead of the server's: Fails with `INVALID_ARGUMENT` (invalid version): the client has another object of the same key, which it must discard and get again from version 0
- The header carries the current version and the data type shared by all data.
- A chunk carries the version of the data it belongs to: its position in the
  object, starting at 1. A client tells data from the start (version 1) from later
  data by the version of the first chunk. A new version or the end of the stream completes
  a part. Empty parts have one empty chunk.
- The Python SDK interprets types such as `arrow.table.zstd` and decompresses
  each part before decoding its value. Other clients receive the type and bytes
  unchanged. The cache does not parse the type.

**PUT (`Put`)**:
- Starts with a key or prefix and client-provided data type, followed by
  byte chunks of at most 1 MiB
- New objects are created with `version=1`
- An existing object is not replaced: the request fails with `AlreadyExist`

**PATCH (`Patch`)**:
- Starts with a full object key and the same data type as the object, followed
  by byte chunks
- Appends the data to the object, increments version
- Returns an error if the object doesn't exist

**gRPC server settings**: the server uses 16 MiB HTTP/2 stream windows and a
64 MiB connection window (the 64 KiB default caps the upload rate of one stream
at about window / round-trip time) and enables TCP no-delay. Chunks stay at
1 MiB.

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

The disk benchmarks measure 8 MiB opaque writes and object reloads, plus
sequential 1 KiB appends. Reloads bypass the resident cache but
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

A worker install (`setup.sh --join`) creates a systemd service for the cache:

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
- Install scripts: `scripts/README.md`

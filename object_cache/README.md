# Flame Object Storage Service

`flame-object-cache` stores opaque client-encoded object bytes in the filesystem.
Despite its existing service name, it has no server-side payload cache, LRU,
object proxies, memory budget, or `none` storage backend. PUT, PATCH, and GET
stream bounded chunks directly through filesystem part writers and readers.

Clients choose the encoding, such as Arrow IPC, cloudpickle, or compressed raw
bytes. Existing Python/Rust SDK client caches are separate from server storage.

## Objects, parts, and chunks

- **Object:** UUID, key, encoding, committed part count, size, and the part
  descriptors selected for a captured request.
- **Part:** A zero-based index, size, creation timestamp, and SHA-256 checksum.
  Part descriptors contain no payload buffers.
- **Chunk:** Internal transfer bytes with their owning part index and
  part-relative offset. A part may span many chunks, including an empty chunk
  for an empty part.

The service exposes PUT, PATCH, and GET. Metadata listing is internal; deletion
is internal to GC.
PUT and PATCH create an object if missing; otherwise they append a part.
Each committed part increments its version. Readers select Committed parts in logical commit order and skip
Writing and Stale slots. Deletion and
recreation produce a new UUID. Payloads persist across server restart.

## Modules

| Module | Responsibility |
| --- | --- |
| [`model/`](src/model/mod.rs) | Object/Part descriptors, metadata/filter/key types, and internal Chunk. |
| [`storage/`](src/storage/mod.rs) | Direct filesystem implementation, checksummed files, part reader/writer contracts, metadata catalog, recovery, and deletion. |
| [`controller/`](src/controller/mod.rs) | Direct storage operations, advertised endpoint, and server startup. |
| [`storage/dao.rs`](src/storage/dao.rs) | Filesystem object, metadata, and part DAO records and domain conversions. |
| [`storage/object_reader.rs`](src/storage/object_reader.rs) | Select and stream captured filesystem part readers. |
| [`storage/object_writer.rs`](src/storage/object_writer.rs) | Incremental hashing and filesystem part writes without catalog access. |
| [`storage/part_reader.rs`](src/storage/part_reader.rs) | Part reader contract, positional filesystem reads, and checksum validation. |
| [`storage/part_writer.rs`](src/storage/part_writer.rs) | Physical part allocation, parallel positional writes, and durability operations. |
| [`controller/grpc.rs`](src/controller/grpc.rs) | RPC framing and the three GET handlers. |
| [`controller/gc.rs`](src/controller/gc.rs) | Garbage collection through internal storage deletion. |

## Configuration

```yaml
cluster:
  name: flame
  endpoint: http://127.0.0.1:8080
cache:
  endpoint: grpc://0.0.0.0:9090
  advertise: eth0
  storage: fs://cache
  gc:
    interval: 60s
```

Storage defaults to `fs://cache` when omitted. Relative fs/file URLs resolve under
`FLAME_HOME` (default `/var/lib/flame`); plain paths and absolute filesystem URLs
are supported. `FLAME_CACHE_STORAGE` overrides the storage path. Empty paths,
`none`, and non-filesystem URL schemes are rejected. Eviction configuration is
removed.

`cache.advertise` accepts an interface selector, IP, hostname, host:port, or URL.
It takes precedence over `FLAME_CACHE_ADVERTISE`. Explicit IP/interface selectors
also choose the listener address. DNS advertisements retain wildcard binding;
the listener port comes from `cache.endpoint`. With neither setting, references
use a discovered private interface address or the configured endpoint.

GC is enabled with a default 60-second interval; `cache.gc.interval` overrides
that interval. Its FSM client uses the cluster endpoint/TLS configuration.
Existing `cache.tls` and `cache.pprof` settings remain available. TLS uses a
`grpcs://` endpoint; returned references use the client-compatible `grpc+tls://`
scheme.

## Filesystem layout

```text
<storage-root>/
├── .uploads/
└── <application>/<session>/<object>/
    ├── metadata.json
    ├── index.bin
    └── data.bin
```

Filesystem records use `ObjectDao`, `ObjectMetadataDao`, and `PartDao`, kept
private to storage. ObjectDao combines the stored metadata and all physical part states;
readers and writers exchange the domain types instead. The metadata JSON retains UUID/type fields; each 72-byte index entry also
persists its Writing, Committed, or Stale state and logical commit index.

Metadata persists the UUID and encoding. The index records each part's physical
offset, length, creation time, and SHA-256 checksum. Chunks use part-relative
offsets; physical extents remain private to storage.

Storage creates missing object files before allocating a
block under a short catalog lock. It owns the input stream through commit or
cleanup, so writers need no catalog handles or mutation callbacks.

Each PUT/PATCH header declares the exact encoded payload size. Storage reserves
a physical ordinal and a disjoint sparse data-file block under a short lock, persists its
Writing descriptor, then streams bounded chunks directly into that block outside
the lock. Extra bytes and incomplete bodies are rejected; zero declares an empty
part. Both SDKs derive sizes from encoded bytes or the already-opened file.

Commit makes payload and index state durable and assigns a dense logical part
index. Payload writes and syncs run in parallel; a short metadata lock orders
commit indexes. A completed part never waits for an earlier allocated upload.
For example, if the second patch finishes first, it receives the next logical
index; the first patch receives the following index when it commits. Persisted
commit indexes keep historical prefixes stable across later writes and restart.
Storage publishes Committed state and the cached version from the successful
durability result. Object and part writers perform filesystem I/O without
accessing the catalog.
Abort marks only its physical slot Stale. Retried writes append new physical
slots; delayed I/O cannot overwrite them.

Startup calls explicit object recovery. Ordinary descriptor opening only loads
and validates complete records; it never trims indexes or rewrites part state.

Storage initialization changes interrupted Writing parts to Stale, retains the
dense logical Committed prefix, and trims torn index records without loading
payloads. A partially persisted commit tail beyond a missing logical index is
marked Stale without renumbering; acknowledged prefixes remain unchanged.
Objects with no committed data are removed; stale payload blocks are ignored. Callers use `list(filter)` for discovery; listing does
not perform recovery. Valid dot-prefixed object IDs are preserved.

There is no payload or file-handle cache. Storage's small metadata catalog tracks
committed files and is updated on commit/deletion. Handles belong to active
operations and are released when their readers/writers finish or are dropped.

## GET modes

| Mode | Result |
| --- | --- |
| `METADATA` | One current metadata header and EOF; no payload reads. |
| `OBJECT` | Parts before the requested version; version zero selects latest. |
| `PATCHES` | Parts from the requested cursor to the captured current end. |

OBJECT prefix metadata reports that prefix's count and size. PATCHES metadata
reports the captured full object. An unchanged suffix returns its header and
EOF. Nonzero PATCHES cursors require UUID. UUID mismatch returns
`FAILED_PRECONDITION`; an ahead version returns `INVALID_ARGUMENT`; missing
objects return `NOT_FOUND`.

The controller queries `list(ObjectFilter)`, then calls storage
`read_object(&mut metadata)` or `read_patch(&mut metadata)`, returning a
`ChunkStream` of `Result<Chunk, FlameError>`. Object reads use version as the
exclusive end; patch reads use it as the starting cursor. Storage replaces
metadata with the captured snapshot before returning the stream. Storage validates
identity/range and opens only the selected part readers. `ObjectFilter::AllObject`
selects all current metadata for internal enumeration. `ByKey { key, version }`
selects an exact key or namespace; `version=None` selects current metadata and
`Some(N)` projects `[0, N)`, including an empty prefix for `Some(0)`.
The wire OBJECT value zero instead means latest.

Capturing a chunk stream and emitting its header do not load payload data. A private ObjectReader
traverses the captured Part descriptors; each PartReader streams bounded Chunk
values and verifies integrity through `next_chunk`; EOF releases that reader
before advancing.
The gRPC adapter maps chunks directly to responses. Corruption fails with
`DATA_LOSS`; SDKs reject the entire failed result even if earlier chunks arrived.

## Writes, snapshots, and deletion

PUT and PATCH both stream request chunks directly. There is no whole-part PATCH
buffer in the server. Writers incrementally hash bytes and commit only after
successful input EOF. A source error marks only that unfinished part Stale;
other concurrent commits remain readable.

Parallel writers share `ObjectDao.parts` and its `PartDao` state; there is no
separate allocation registry or duplicate parts list. A short lock protects block
allocation and index/catalog updates. Streaming and fsync happen outside it. Readers and GC have no admission guards.

Reads validate UUID and requested version against the current Committed prefix before
opening payload handles. Historical OBJECT prefixes and older PATCHES cursors are
supported; versions beyond the Committed prefix are rejected immediately. Reads never
wait for Writing parts. Writing and Stale slots are skipped. The DAO caches the
version derived from its committed logical parts and rebuilds it on recovery;
there is no separately persisted object version. An initial create with no committed part
is NOT_FOUND. Captured file handles survive unlink.

GC unlinks vanished-session data without waiting for readers or writers. An
in-flight write may fail; it cannot republish collected metadata. A late upload
for an absent session is collected in a later cycle. Storage write tasks retain ownership
independently of the client waiting for them.

GC lists object metadata and validates current application/session snapshots from
FSM. Session objects remain while their session exists, including closed sessions.
After the session is absent and a short creation grace has passed, GC confirms
absence with a fresh snapshot and removes the object. The reserved `pkg` namespace
belongs to its application instead. Failed or malformed frontend responses cannot
authorize collection. Collection proceeds regardless of active readers or
writers. Appending to an object whose session is gone does not retain it.

There is no delete RPC or SDK helper, and no `delete_if_unchanged` API. Package
uploads never delete or replace an existing name. Filesystem metadata is retired
on internal deletion, with no memory payload cache to invalidate.

## Validation

The package tests cover bounded direct streaming, all GET modes, empty/multiple
parts, corruption, failed input/commit, UUID races, captured-inode snapshots,
namespace deletion, GC revalidation, and filesystem recovery. Helm templates and
CI configurations use filesystem storage. See the [design](../docs/designs/RFE703-object-cache-refactor/FS.md)
for the contracts and verification matrix.

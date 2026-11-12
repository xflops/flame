# RFE703: Filesystem objects, parts, and chunks

Status: Implemented.

## Goal

Keep a simple filesystem-backed object service. Objects persist only in the
filesystem. The original refactor removed the object payload cache, proxies,
loading state, LRU, memory budgets, and `none` backend. A later enhancement adds
the bounded storage-owned part cache described below, without restoring object
proxies or changing filesystem authority.
The service name remains unchanged; its public RPCs are Put, Patch, and Get.

Keep the `Object -> Part -> Chunk` hierarchy and UUID-aware GET modes. Client SDK
caches are independent of this server-side change and retain their existing
behavior. There is no filesystem-format compatibility layer or migration.

## Data model

```rust
pub struct Object {
    metadata: ObjectMetadata,
    parts: Vec<Part>,
}

pub struct Part {
    part: u64,
    size: u64,
    creation_time: i64,
    checksum: [u8; 32],
}

pub(crate) struct Chunk {
    part: u64,
    offset: u64,
    data: Bytes,
}
```

`Object` captures metadata and the immutable descriptors selected for a request.
An object-prefix view contains parts `[0, end)`; a patch-suffix view contains
`[start, end)`. Suffix parts keep their original indexes. `Part` contains no
payload buffer, reader, writer, or physical file extent. Only `Chunk` owns bytes
being transferred, and its offset is relative to its owning part.

Object UUID identifies an incarnation. Deleting and recreating a key produces a
new UUID. Object version is the number of committed parts; it is the exclusive
readable end. Part indexes start at zero and have no independent version.

PUT and PATCH share create-if-missing behavior. Every write adds one part,
including an empty part, without replacing prior payloads.
PartDao records persist Writing/Committed/Stale state. The readable sequence
skips Writing and Stale slots. Committed parts have stable dense logical indexes
assigned in commit order; object version is their cached derived count. State
lives in the storage DAO, with no object payload cache or loading proxy. The
optional part cache holds verified bytes independently of domain descriptors.
Captured descriptors are not changed by later appends or deletion.

## Module ownership

| Module | Responsibility |
| --- | --- |
| [`model/`](../../../object_cache/src/model/mod.rs) | Object/Part descriptors, metadata/filter/key types, and internal Chunk transfers. |
| [`storage/`](../../../object_cache/src/storage/mod.rs) | Direct filesystem implementation, part reader/writer contracts, private index records, metadata catalog, recovery, and deletion. |
| [`controller/`](../../../object_cache/src/controller/mod.rs) | Direct filesystem operations, endpoint stamping, metadata selection, and startup. |
| [`storage/dao.rs`](../../../object_cache/src/storage/dao.rs) | Filesystem object, metadata, and part DAO records and domain conversions. |
| [`storage/object_reader.rs`](../../../object_cache/src/storage/object_reader.rs) | Capture requested Object/Part views and forward storage chunks. |
| [`storage/object_writer.rs`](../../../object_cache/src/storage/object_writer.rs) | Hash chunks and perform filesystem writes without catalog access. |
| [`storage/part_reader.rs`](../../../object_cache/src/storage/part_reader.rs) | Part reader contract, positional filesystem reads, and checksum validation. |
| [`storage/part_cache.rs`](../../../object_cache/src/storage/part_cache.rs) | Byte-bounded part LRU, memory-reader construction, resident-byte accounting, and incarnation invalidation. |
| [`storage/part_writer.rs`](../../../object_cache/src/storage/part_writer.rs) | Physical part allocation, positional writes, and durability operations. |
| [`controller/grpc.rs`](../../../object_cache/src/controller/grpc.rs) | Wire framing and the metadata/object/patch GET handlers. |
| [`controller/gc.rs`](../../../object_cache/src/controller/gc.rs) | Classify vanished-session data and stale application packages for internal deletion. |

There is no `cache/` module. The controller accesses storage directly. Filesystem
`ObjectDao`, `PartDao`, and `ObjectMetadataDao` remain private backend types. Physical offsets and index
layout do not cross the storage boundary. Its metadata catalog is bookkeeping
for committed files, not a payload or file-handle cache.

## Filesystem DAOs

Storage uses three private persistence representations:

- `ObjectDao` combines an `ObjectMetadataDao` and ordered `PartDao` entries, including their Writing, Committed, or Stale states.
- `ObjectMetadataDao` persists the UUID and data type in `metadata.json`.
- `PartDao` encodes a part's physical offset, length, creation time, and SHA-256
  checksum, Writing/Committed/Stale state, and logical commit index as a 72-byte
  index entry.

`ObjectDao` caches the version derived from committed parts, rebuilds it on
recovery, derives size and creation time, and
converts them to domain `ObjectMetadata`. DAOs do not own payload buffers or cross
the storage boundary. Metadata JSON still stores UUID and data type; the index now includes state
and logical commit index in a 72-byte record, without a backward-format compatibility layer;
`Object`, `Part`, and `Chunk` remain the domain and transfer types.

## Storage API

```rust
trait PartReader {
    fn part(&self) -> Part;
    async fn next_chunk(&mut self) -> Result<Option<Chunk>, FlameError>;
}

trait PartWriter {
    async fn write_chunk(&mut self, chunk: Chunk) -> Result<(), FlameError>;
    async fn sync_payload(&mut self, part: Part) -> Result<(), FlameError>;
    fn prepare(&mut self, part: Part) -> Result<(), FlameError>;
    async fn commit(&mut self) -> Result<(), FlameError>;
    fn abort(&mut self) -> Result<(), FlameError>;
}
```

A part reader owns its captured descriptor, file resource, byte cursor, length
checks, and checksum state. `next_chunk` emits bounded chunks and verifies the
part before successful EOF. ObjectReader releases each part reader at EOF;
direct reader resources are released when ownership ends. There is no reader
finalization call or assembled payload buffer.

A part writer accepts sequential chunks belonging to its assigned physical part.
It validates offsets and exact length before payload sync, persists the logical
descriptor assigned by storage, and returns the index durability result. It has
no catalog access and does not publish object or part status. Storage retires
unprepared failed uploads; other concurrent commits can still advance version.
Neither ObjectReader nor the gRPC adapter duplicates backend byte-cursor or
checksum validation.

Storage exposes `read_object(&mut metadata)` and `read_patch(&mut metadata)`,
returning `ChunkStream<Item = Result<Chunk, FlameError>>`, plus streaming
`write_object(key, data_type, size, chunks)` / `write_patch(...)` operations.
Storage privately retains the selected Object descriptor and open PartReaders
in the returned chunk stream. Filesystem writers own incremental hashing, bounded positional writes, and durability I/O.
FsStorage owns each accepted write task from allocation through source consumption,
commit-result publication, and cleanup. Writer factories are private to storage;
writers carry no catalog handles. No intermediate metadata/handle carrier,
mutation callback, reservation wrapper, or worker queue is needed.

The controller forwards byte streams and adds the advertised endpoint to returned
metadata. Storage does not depend on RPC request types. Storage also provides
internal `list(filter)` and GC-internal `delete_objects`; neither is a public RPC. Selected readers share one
data-file descriptor across parts; there is no long-lived file-handle pool.

`ObjectFilter::AllObject` explicitly selects all current object metadata.
`ObjectFilter::ByKey { key, version }` selects an exact key, session, or
application. `version=None` returns latest metadata. `Some(N)` projects a prefix `[0, N)` and
returns its total size; `Some(0)` is an empty prefix preserving identity/type.
Counts ahead of the committed index are invalid. Latest metadata comes from the
backend DAO catalog; prefix sizes are derived from visible committed descriptors.

## Filesystem layout and recovery

```text
<root>/
└── <application>/
    └── <session>/
        └── <object>/
            ├── metadata.json
            ├── index.bin
            └── data.bin
```

The metadata file persists UUID and encoding information. Each fixed-size index
record contains the part's offset, length, creation timestamp, and SHA-256 digest.
Parts occupy successive regions of the data file. Record offsets are physical
backend details; Chunk offsets are part-relative.

Storage creates missing object files before allocating a
block under a short catalog lock. It owns the input stream through commit or
cleanup; writers need no catalog handles or mutation callbacks.

The write header includes `size`, the exact encoded byte count for this part.
Both SDKs derive it from their encoded byte buffer or opened file. Zero means an
empty part, never an unknown size. Storage rejects short bodies and extra bytes.

A short allocation lock selects the next part index, extends the data file with
a sparse void block of the declared size, and writes its Writing descriptor.
Each part writer awaits bounded positional writes at its physical offsets
outside the lock. Idle uploads use async tasks rather than
occupying the blocking I/O thread pool. Concurrent parts have disjoint extents, including when earlier uploads are
still pending. Write payloads are never assembled in server memory.

Payload writes and syncs run independently. A short metadata update assigns a
dense logical commit index; index durability is established outside the lock
before publication and acknowledgement. Commit returns authoritative metadata
for a durable prefix containing its own part. It never waits for an earlier
allocated Writing slot. Public indexes follow commit order, so a patch finishing
first receives the next logical index. Persisting that index preserves historical
OBJECT prefixes on restart, even when physical allocation order differs.

FsStorage coordinates these steps and publishes DAO status and cached version
from the successful durability result. ObjectWriter and the physical part writer
own files and perform I/O without accessing the shared catalog.

Abort or failure marks only an unprepared Writing slot Stale. Once a logical
index has been prepared, its record remains available for a later sync fence or
recovery, including after cancellation. Later committed parts remain valid. Physical ordinals and extents are never reused while delayed I/O may still
target them; Writing and Stale parts consume no public logical index.
`ObjectDao.parts` and `PartDao` hold all write state, without allocation registries,
reservation wrappers, worker queues, or earlier-writer notifications.

Startup calls explicit object recovery. Ordinary descriptor opening only loads
and validates complete records; it never trims indexes or rewrites part state.

Recovery changes Writing slots to Stale and retains the dense logical Committed
prefix. A crash can persist an unacknowledged later descriptor before an earlier
one; records beyond that first missing logical index become Stale without
renumbering. Acknowledged prefixes and cached PATCHES cursors remain stable.
Readers skip stale entries using captured physical descriptors. A torn final
record is trimmed; objects with no committed payloads are removed. Recovery and
listing never preload payload bytes.

The filesystem is the authoritative persistent copy. A restart retains committed
objects and UUIDs. Service deletion is internal to GC; there is no Delete RPC or public SDK deletion
helper. Objects persist until their owning session is gone. External filesystem
actions can also remove files. Part-cache eviction releases memory only; it never
deletes persistent objects or changes committed metadata.

## Reads and GET modes

GET supports three explicit modes:

| Mode | Selection |
| --- | --- |
| `METADATA` | One current metadata header; no payload reader or chunks. |
| `OBJECT` | Parts before the requested version; wire version zero selects latest. |
| `PATCHES` | Parts from the requested cursor through captured current end. |

`OBJECT` prefix metadata describes its selected prefix. `PATCHES` metadata
describes the captured full object. An empty suffix returns its header and EOF.
A nonzero PATCHES cursor requires UUID. A UUID mismatch returns
`FAILED_PRECONDITION` before considering an ahead cursor; an ahead version returns
`INVALID_ARGUMENT`. Missing objects return `NOT_FOUND`.

The controller selects metadata with `list(filter)`, then calls storage
`read_object(&mut metadata)` or `read_patch(&mut metadata)`. Read operations
return only chunk streams; private readers remain inside storage. The object
read uses metadata.version as its exclusive end, while the patch read uses it
as its starting cursor. Storage updates metadata to the captured prefix or latest
committed snapshot before returning the stream, so the header and chunks agree.
Shared capture takes an internal start and optional exclusive end; absent end
means the current committed end. Storage validates UUID/range and opens selected immutable filesystem handles.
It rechecks the file incarnation after opening so a GC/recreation race cannot mix
object identities. No read registration or deletion guard is retained. Metadata
queries do not read payload bytes.

The private ObjectReader's part position traverses its captured Object.parts descriptors.
Each part reader emits Chunk values and validates integrity in `next_chunk`.
The object reader releases it at EOF and advances to the next part. gRPC maps chunks directly to wire messages, with no
intermediate response state machine or admission candidate.

Captured file handles and index descriptors preserve the selected view while
PATCH appends new parts or GC unlinks the object. A read interrupted during file
opening may fail; an already captured reader can finish from its original inode.
Corruption produces `DATA_LOSS`; earlier chunks may have been delivered, so SDKs
must reject the entire failed GET rather than publish a partial client result.

## Writes and operation coordination

Both PUT and PATCH parse a header and forward the remaining byte stream to
storage. PATCH is not buffered as a whole part. Storage owns allocation, commit,
and catalog publication; its filesystem writer hashes input incrementally and
returns I/O results. Storage returns metadata only after durable commit and the
corresponding catalog update. The accepted write task lives in storage, with no
second task or status mutation in the writer or gRPC adapter.

A short lock protects physical block allocation and DAO/index metadata updates.
Per-part writes and fsync occur outside it. Writers own
their captured files directly; there are no allocation registries, reservation
guards or worker queues. An interrupted borrowed chunk-write future poisons its
writer, so a detached pwrite cannot race a retried payload into a valid commit.

Readers never await write completion. They validate UUID, then reject requested
versions beyond the current visible committed count before opening data files.
The sequence skips Writing and Stale slots; public part indexes are dense and
follow persisted commit order. Historical OBJECT prefixes and older PATCHES cursors remain valid.
Physical descriptors are captured directly from the DAO, and file handles remain
valid across concurrent GC unlink. Object version is cached from committed
parts and rebuilt on recovery, without an independent persistent version field.

Commit acknowledgement does not wait for earlier Writing slots. GC unlinks
without waiting for any writer or reader. Writes racing collection fail rather
than resurrecting data.

## Garbage collection

GC lists committed object metadata and obtains validated application and session
snapshots from FSM. Normal `<app>/<session>/<object>` data is collectible only
when its `(application, session ID)` is absent, after the existing ten-second
creation grace. Open and closed sessions both retain data. Before deletion, GC
refreshes the validated liveness snapshot; any RPC error or malformed response
skips destructive work. An empty successful session listing confirms absence.

The reserved `<app>/pkg/<filename>` and `<app>/shared/<object>` namespaces are
application-owned. Shared data written by `app.put()` is available across sessions.
Both namespaces retain the existing missing-application and
prior-application-incarnation cleanup rules, rather than treating `pkg` or
`shared` as a missing session. Uploading the same package name does not delete
or replace prior parts. Reusing a package name appends another
part; package downloads require a single-part object, so distinct uploads need
distinct package names.

Deletion is internal to GC and uses `delete_objects` to unlink paths and retire
metadata directly, regardless of active readers or writers. Session disappearance is
the lifetime decision: a newly committed patch does not make a vanished session's
object live again. There is no `delete_if_unchanged` API or object-version-based
retention rule. No external delete RPC, SDK object/package delete helper or
pre-upload delete exists. Client cache integrity still uses UUID and part counts.

## Storage part cache

Storage owns a byte-bounded LRU of immutable, committed parts. Entries are keyed
by object UUID and logical part number, so parallel allocation slots and recreated
object keys cannot alias cached payloads. The controller still selects metadata
and GET modes through storage; there is no separate controller cache or public
RPC change.

A read uses a ready cached part when available. On a miss, storage reads the
captured filesystem part with the existing bounded-chunk and checksum behavior.
`PartCache::patch_readers(object_id)` creates readers for all cached parts of
that object using each cached part's own metadata. `FsStorage::part_readers` calls `open_readers(object_id, parts)` for misses and
merges cached and disk reader maps in captured order. The filesystem opener
returns a map keyed by PartDao.part; selected readers are independent clones
sharing immutable file handles, preserving repeated selections. The opener resolves the storage key
under its storage root and uses each PartDao's existing logical part ID; no
duplicate index is passed. Snapshot capture rechecks UUID after opening handles.
Fully cached reads validate UUID without
opening the payload file. The only concrete part readers are MemoryReader and
FsReader; neither needs a cache-population wrapper.
`FsStorage::read_stream` observes complete, checksum-verified miss payloads and
inserts them into `self.part_cache` after checking the current catalog UUID.
Cached-only, disabled-cache, and oversized-only streams use the direct reader
stream. Failure or cancellation releases incomplete buffers without insertion.
`FsStorage::delete_committed` uses the same owned fields to remove files, catalog
metadata, and cached parts under the established lock order.
Only a complete read that verifies the entire part is eligible for insertion;
partial, canceled, or corrupt reads never publish an entry. Writes still commit
durably to the filesystem and do not populate the part cache in this iteration.
A restart begins with an empty part cache and recovered committed descriptors.

`entries: HashMap<String, HashMap<u64, CachedPart>>` groups ready parts first
by object ID and then by logical part ID. Each cached
part holds its full `Part` descriptor, `object_id`, latest access clock, and an `Arc`
of immutable chunks. The descriptor supplies part ID, size, creation time, and
checksum for memory readers and pool accounting.
Nested hash lookups provide constant-time part access. The cache `object_id` is
the object incarnation UUID. `PartCache::patch_readers(object_id)` returns all
cached parts for that object using stored descriptors. FsStorage selects only
parts in the captured OBJECT prefix or PATCHES suffix, so unused cached parts
are not streamed or marked recently used. A separate
`order: BTreeMap<u64, PartKey>` sorts parts by their access clocks.
`State` owns the pool and all lookup, insertion, and removal operations;
`PartCache` acquires the mutex and delegates to those operations.
On a hit, storage removes the previous clock from the ordered map, assigns a new
clock to CachedPart, and reinserts its key as most recently used. This updates
recency in logarithmic time without scanning entries or retaining stale keys.
Eviction removes the first ordered key and its cached part. GC removes both the
cached payload and its exact order key. Active readers share immutable chunks
through `Arc`. On insertion of a verified ready part, storage evicts least
recently used entries until it fits.
The pool counts only payload bytes resident in this LRU. Checking eligibility or
buffering an in-flight read does not reserve space or evict ready parts. Empty
parts and parts larger than the pool are not cached.

Eviction releases resident capacity immediately and never interrupts a captured
reader. Readers and returned chunks can retain shared immutable bytes after
eviction. These retained bytes and buffers collected by in-flight reads are
outside the resident pool. Concurrent misses may independently read and buffer
the same part; inserting an already resident identity keeps one copy in the LRU.

GC invalidates all cached parts for the collected object incarnation. A read
already holding a captured inode can finish, but cannot repopulate a deleted
incarnation. Catalog identity and requested versions are checked before cache
access; cached bytes cannot make a collected object visible or bypass UUID checks.
Append commits retain existing immutable parts, including historical OBJECT
prefixes and PATCHES cursors.

## Configuration

```yaml
cache:
  endpoint: grpc://0.0.0.0:9090
  advertise: eth0
  storage: fs://cache
  pool:
    size: 256M
    policy: lru
  gc:
    interval: 60s
```

An omitted storage setting uses `fs://cache`. Relative fs/file paths resolve
under `FLAME_HOME`, defaulting to `/var/lib/flame`; absolute paths and plain
filesystem paths are supported. `FLAME_CACHE_STORAGE` can override the backend
path. `none`, empty backend URLs, and non-filesystem URL schemes are rejected.
`cache.pool.size` accepts the existing memory-size notation (for example, `256M`,
`1Gi`, or a byte count). It defaults to 256 MiB; `"0"` disables part caching.
`cache.pool.policy` defaults to `lru`, the only supported replacement policy.
An omitted pool, `{}`, or an omitted property uses the corresponding default.
Unknown properties and unsupported policies are rejected; the pool is a mapping,
not a scalar. Helm exposes `objectCache.pool.size` and `objectCache.pool.policy`.
The size limits only ready payload bytes resident in the part LRU. It does not bound in-flight
read buffers or evicted bytes retained by readers and returned chunks. Metadata,
allocator overhead, filesystem-stream buffers, and the operating-system page
cache are also separate; the pool is not a process RSS limit.
Helm and CI templates use filesystem storage. Endpoint advertisement, interface
binding, TLS, GC interval, and profiling behavior remain supported.

## Validation

- Domain descriptors retain identity/size/checksum without owning payloads.
- Opening a reader and emitting its header do not read payload data; each next
  operation reads a bounded chunk, including objects much larger than a chunk.
- Three GET modes, prefix/suffix counts, empty parts, UUID recreation, and cursor
  errors retain their protocol behavior.
- Captured snapshots retain their original data across concurrent append,
  internal GC unlink, and key recreation.
- Source failures and abort before Commit never publish the failed part;
  unrelated concurrent commits can still advance the readable version.
  Invalid writes and corruption cannot publish successful partial results.
  An already accepted Commit or deletion completes despite RPC waiter cancellation.
- GC retains existing open/closed sessions and removes gone-session data without waiting for readers/writers.
- A stalled earlier upload does not delay a later commit. Out-of-order completion
  preserves historical prefixes before and after recovery; Writing and Stale
  slots never appear as readable parts.
- Parallel block allocation/recovery, index integrity, catalog projections, and storage
  URL validation retain direct backend regression coverage.
- Run package/common tests, strict workspace lint, private Rustdoc, Helm lint and
  rendering, and a rebuilt-server filesystem E2E suite.

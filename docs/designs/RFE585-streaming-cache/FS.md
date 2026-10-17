# RFE585: Streaming object cache storage

## Motivation

The object cache held complete objects in memory at every step:

- A `Put` collected all chunks into one `Vec` before writing.
- A `Get` of an object that was not resident read all of its data from disk, kept a copy in memory, and then built the response.
- Startup loaded every payload of the storage directory into memory.
- A write read the old object only to learn its version.

With large objects, or many concurrent clients, the process was OOM-killed.
Memory use must depend on the chunk size, not on the object size.

## Design

### Storage API

`StorageEngine` (`object_cache/src/storage/mod.rs`) is a streaming interface:

| Method | Purpose |
|--------|---------|
| `write_stream(key, meta: ObjectMetadata, chunks) -> ObjectMetadata` | Create an object, its first data, from a stream of chunks, with the data type and creation time of `meta`, and return `meta` with the size written; nothing is stored unless the stream ends successfully; an existing object is an `AlreadyExist` error |
| `read_range(key, part, offset, len)` | The only way payload bytes are read for serving; `part` 0 is the first data |
| `open(key)` | The open files of an object (`ObjectFiles`) for engines that have files |
| `stat(key)` | `ObjectStat` (data type, creation time, the size of each part) without reading payloads |
| `patch_object(key, data)` | Append an `ObjectData` |
| `scan()` | Metadata of all objects, for startup |
| `touch(key)` | Recency hint for engines that evict |

An object is a list of `ObjectData` (creation time, data) like the layers of an
overlay filesystem. They are all alike: the first is just the one written when the
object was created. The version of an object is its number of `ObjectData`, so a
new object is version 1. `Object { version, data_type, data }` holds all the data
for version 0 and, above 0, only what was written after that version. Objects are
created once and then only appended to; there is no update. An `ObjectData` has no
data type of its own: the type belongs to the object. At most
`MAX_APPENDS_PER_OBJECT` pieces of data can be appended to what the object was
created with. Writes to one object are serialized by the caller (the cache's
per-key lock).

### Disk layout

`DiskStorage` stores an object as a directory, like the session manager's
filesystem engine stores a session:

```text
<root>/<app>/<session>/<object>/
├── metadata.json   # data type
├── index.bin       # one 24-byte record per part: offset, length, creation time
└── data.bin        # payloads, concatenated, append-only
```

No version is stored: the version of a part is its index + 1. `ObjectFiles::open`
opens `index.bin` and `data.bin` for append (and read), reads `metadata.json`, and
derives the number of parts from the length of `index.bin`; nothing else is
kept in memory. The records are fixed-size, so reading part `i` is a positional
read of its record at `i * 24` of `index.bin` followed by a positional read of
the payload at the record's offset in `data.bin`. `stat` reads all the records
in one read. `append` writes through the same two files and bumps the count, so
the handle never goes stale.

- Create: build the directory under a temporary name (payload streamed to
  `data.bin` through a buffered writer, then index and metadata), fsync, and
  rename it into place. An aborted or failed stream leaves nothing; startup scan
  removes leftover temporary directories.
- Patch: both files are open for append and the records are fixed-size, so a
  append is two writes: the payload to `data.bin` (fsync), then its record
  (offset = the end of `data.bin`, length, creation time) to `index.bin`
  (fsync). The record commits the append; payload bytes without a record (an
  interrupted append) are ignored, and a partial trailing record is cut off when
  the object is opened so later records stay aligned.
- Everything runs in `spawn_blocking`.

### Resident decorator

`ResidentStorage` wraps an engine and keeps, for each recently used object, only
its key, its metadata and the open `index.bin` and `data.bin` files (an
`Arc<ObjectFiles>`), so serving an object does not open and close files per read
and holds no payload bytes. A handle is opened by the first read (`open`) and
is bounded by `max_memory` / `max_objects` with the existing eviction policies; an
evicted handle is opened again. An append goes through the handle and updates it; a delete drops it.
`read_range`, `stat` and the rest of the engine's reads go through the handle.

### `storage: none`

`none` means no store. The engine is `NoStorage`, which discards writes and
finds nothing, so `ResidentStorage` alone holds the objects:

- every object is resident with all its parts, whatever its size (handles
  only apply when there is an engine with files);
- `max_memory` / `max_objects` eviction applies, and an evicted object is lost,
  as before this change.

`storage::connect(url, eviction)` builds the stack, so the cache does not
know the engine behind it: `none` (or an empty url) gives
`ResidentStorage(NoStorage)` (cache only), a path gives
`ResidentStorage(DiskStorage)` (cache of open files over the disk engine).

### ObjectCache

`ObjectCache` keeps only the metadata map and the per-key locks. `put_stream`
passes chunks straight to storage and creates the object at version 1; a put to
an existing key fails with `AlreadyExist`. Startup uses `scan` and loads
metadata only.

### gRPC

The server sets 16 MiB HTTP/2 stream windows, a 64 MiB connection window and TCP
no-delay: with the 64 KiB default window, one upload stream can send at most one
window per round trip, which caps its rate at window / round-trip time on a
cluster network. Chunks stay at 1 MiB.

`Put` forwards chunks to `write_stream`, so a write holds one chunk in memory.
`Get` is a state machine in `cache/response.rs`: `ResponseStage` is `Header`,
`Next`, `Reading` or `Done`, with one handler per stage. Every chunk is read
with `storage.read_range`. The response owns the key's read guard until it
ends. The wire protocol has no mode or part kind: the request carries a
`version` (0 for everything), the response header the current version and data
type, and each chunk the version of its `ObjectData`; the chunk size stays 1 MiB.

## Behaviour changes and limitations

- Every object has a read/write lock in the cache, like the entries of the
  session manager's filesystem engine. A `Get` holds the read lock until its
  response ends, so a slow client delays writers and deleters of that object
  (previously the lock covered only the snapshot); put, patch and delete take
  the write lock, and a delete of a prefix takes the lock of every object it
  matches, in key order.
- A `Get` with a version ahead of the object's fails with `INVALID_ARGUMENT` (the
  `InvalidVersion` error): the client has another object of the same key (deleted and created again).
- Under `none`, a streamed write is assembled in memory as the resident object,
  so a `none` cache cannot stream objects larger than memory. Memory per write
  is one chunk only with a disk store.
- Under `none`, eviction removes the object; metadata may outlive it and a `Get`
  of such an object returns not found.
- Startup is metadata only; the first read of an object opens its files.
- Objects cannot be updated: a `Put` to an existing key fails. The SDKs have no
  `update_object`; `Instance.update_context` became `patch_context`, and the
  newest data part is the context.
- Patch requests are still buffered in memory; the data of a patch is small by
  construction.

## Testing

- Storage engines: layout, create-only (`AlreadyExist`), abort and error leave
  nothing, ranges across parts, the append limit, uncommitted appended bytes and
  torn index records, corrupt files, scan and cleanup of leftover directories
  (`disk.rs`, `none.rs`).
- Resident decorator: an object opened once for many reads, an append updates
  the open handle and a delete drops it, the handle limit, create-only, and the
  cache-only mode where objects stay resident with all their parts and eviction
  loses them (`resident.rs`).
- `ObjectCache`: metadata-only startup, a put to an existing key is rejected,
  appending to a large object, eviction under `none`.
- gRPC: all the data, only the data after a version, and nothing; a large
  object streams back in chunks through open files, and under `none` from its
  only resident copy; response stage tests (`response.rs`).

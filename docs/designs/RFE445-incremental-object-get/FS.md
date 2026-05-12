# RFE445: Incremental Object Retrieval by Patch Version

## 1. Motivation

**Background:**

Flame object cache already supports versioned objects and append-only patches. Today, `get_object(ref)` passes a client-side version to the cache server. If the server version matches, the server returns an empty response and flamepy serves the local cached object. If the server version differs, the server returns the full base object plus all patches.

That behavior is correct but expensive for patch-heavy objects. A replay buffer is the clearest example: collectors append transition batches with `patch_object`, while a buffer service repeatedly calls `get_object` for state and sampling. Once the buffer service has version `N`, the next read after versions `N+1..M` only needs those new patches if the base object has not changed. Returning the base object and all historical patches every time makes network transfer and deserialization grow with total buffer history instead of with new work since the previous read.

**Target:**

Add an incremental retrieval path for versioned `get_object`:

- If the client version equals the current server version, return not-modified.
- If the client version is older but the server base snapshot is still valid, return only patches after the client version.
- If the server base object version is newer than the client version, or the patch history cannot bridge from the client version, return the full base object plus patches.

The Python SDK should hide this behind the existing `get_object(ref, deserializer=None)` API and update its local cache so callers do not need a new method.

Success is measured by replay-buffer read-path improvements: fewer bytes downloaded, lower `get_object` latency for state/sample calls, and better or unchanged end-to-end transitions per second.

## 2. Function Specification

### Configuration

No configuration changes are required.

Incremental retrieval is controlled only by the client version sent to `get_object`:

| Request Version | Behavior |
|-----------------|----------|
| `0` | Return the full base object plus all patches. |
| `> 0` | Return not-modified, patches after that version, or a full response when patch-only retrieval is not safe. |

### API

No public Python API change:

```python
def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    ...
```

`ObjectRef.version` remains the latest server object version known at the time the ref was produced. `version=0` keeps its current force-refresh behavior.

For normal reads, flamepy should choose the effective request version this way:

- Send `0` when `ref.version == 0`.
- Send `0` when there is no local cached object, even if `ref.version > 0`.
- Send the local cached object version when a cached object exists and `ref.version > 0`.

The caller does not need to mutate an `ObjectRef` after every read.

### CLI

No production CLI changes are required for incremental object retrieval.

Replay-buffer performance evaluation should add example-only flags to `examples/rl/replay_buffer/main.py`:

- `--metrics-json <path>`: write per-iteration performance metrics.
- `--merge-every <n>`: override compaction cadence.
- `--no-merge`: disable compaction, including forced-full baseline runs.
- `--force-full-get`: force replay-buffer reads to send request version `0` for baseline measurement.

The replay-buffer example should use mode-aware merge defaults: forced-full baseline runs keep merge enabled every 5 iterations, while normal incremental runs disable merge so the benchmark measures patch-only get plus incremental local materialization.

These flags should exit with the existing process status semantics: nonzero on uncaught workload failure, zero when all configured iterations complete.

### Other Interfaces

#### Version Model

The design keeps one monotonically increasing object version. There is no new public version field and no new object API.

That single version is used consistently:

| Location | Meaning |
|----------|---------|
| `ObjectRef.version` | Current object version returned to Python callers. |
| `ObjectMetadata.version` | Current object version tracked by the cache server. |
| Base row `Object.version` | Version when the current base snapshot was written. |
| Patch row `Object.version` | Version assigned when that patch was appended. |

Mutation rules:

| Operation | Version Behavior |
|-----------|------------------|
| Existing `put_object` for a new key | Current version becomes `1`; base row version is `1`. |
| Existing `update_object` / replacing put | Current version increments, base row is written with that new version, patches are cleared. |
| Existing `patch_object` | Current version increments, patch row is written with that new version, base row is unchanged. |

The object cache must persist patch versions. On storage reload, metadata can be reconstructed from the base row and ordered patch rows:

```text
current_version = max(base.version, last_patch.version)
delta_count = number of patches
```

#### Retrieval Protocol

Object retrieval stays on Arrow Flight `do_get`. The cache server currently lists only `DELETE` as a Flight action, so this design does not add a `GET` action or an alternate action.

The existing ticket format remains the protocol:

```text
<key>:<client_version>
```

Keys are already restricted to `<app>/<session>/<object_id>`, so `:` is safe as a ticket separator.

The `do_get` response uses a versioned Arrow schema that lets flamepy distinguish full responses from patch-only responses. This schema is for the read response stream; persistent storage can keep its existing `version,data` row shape.

```text
version: uint64
kind: utf8      # "base" or "patch"
data: binary
```

Implementation should centralize these schema field names and row-kind values as constants or enums on both the Rust server and Python client.

Response framing:

- Not modified: empty stream with this schema and zero rows.
- Full response: exactly one `kind="base"` row first, followed by zero or more `kind="patch"` rows in increasing version order.
- Patch-only response: one or more `kind="patch"` rows in increasing version order and no base row.

Response modes:

| Server State | Response |
|--------------|----------|
| `client_version == current_version` and `client_version != 0` | Empty stream with schema: not modified. |
| `base.version <= client_version < current_version` and all needed patch versions exist | Patch rows where `patch.version > client_version`. |
| `client_version == 0` | Full response: base row plus all patch rows. |
| `client_version < base.version` | Full response: base row plus all patch rows. |
| Patch history is missing, compacted, or non-contiguous | Full response: base row plus all patch rows. |
| `client_version > current_version` | Full response and warning log. Treat the client cache as suspect instead of returning not-modified. |

The client computes the returned current version as the maximum row version. For a full response, the base row and patch rows replace the cached entry. For a patch-only response, the returned patch rows advance the cached current version.

Patch-only responses require a valid local cached base. flamepy enforces this by sending request version `0` whenever no local cache entry exists.

### Scope

**In Scope:**

- Single monotonically increasing object version across base writes and patches.
- Assigning server versions to persisted patch rows.
- Version-driven incremental behavior for the existing `do_get` ticket format.
- Full-object retrieval when the request version is `0`.
- flamepy cache changes to store base data, patch data, and materialized results.
- Unit tests and focused E2E coverage for full, patch-only, and not-modified paths.
- Replay buffer performance evaluation plan.

**Out of Scope:**

- Public Python API changes.
- Cross-process client cache sharing.
- Client cache persistence to disk.
- Arbitrary patch compaction policy changes beyond returning a full response when history cannot bridge.
- Public incremental deserializer/reducer APIs. Existing deserializers still receive base plus the full cached patch list.
- Optimistic write conflict detection.

**Limitations:**

- Patch-only retrieval only helps clients that already have a valid local cache entry.
- If the base object is replaced or compacted past the client version, the server must send a full response.
- Existing deserializers still define how base plus patches become user data. Incremental fetch reduces network and repeated deserialization of old patch payloads; it does not automatically make every deserializer incrementally composable. The replay-buffer example uses a stateful deserializer to apply only newly seen patch batches to its service-local materialized buffer.

### Feature Interaction

**Related Features:**

- RFE318 object cache and patch semantics.
- RFE426 object versioning and client-side cache.
- RFE423 app/session cache key validation.
- `examples/rl/replay_buffer`, which is the target workload for performance evaluation.

**Updates Required:**

- `object_cache/src/cache.rs`: update existing `do_get` version handling with current-version and base-row-version decisions, plus patch-only response streaming.
- `object_cache/src/storage/disk.rs`: persist server-assigned patch versions and reconstruct current version from base plus patch history.
- `sdk/python/src/flamepy/core/cache.py`: cache base, versioned patches, and materialized outputs; send request version `0` for full fetches and cached nonzero current versions for incremental reads.
- `sdk/python/tests/test_cache.py` and E2E cache tests: add full, patch-only, not-modified, and update-after-cache coverage.
- `examples/rl/replay_buffer/replay_buffer.py`: use a stateful deserializer so `state()` and `sample()` merge only newly seen patch batches into the service-local materialized buffer.
- `examples/rl/replay_buffer/main.py`: add benchmark flags and metrics export for the performance evaluation.

**Integration Points:**

- Arrow Flight `do_get` remains the transport for object reads.
- `do_put` with path descriptors remains the transport for put/update writes.
- `do_put` with `PATCH:<key>` command descriptors remains the transport for patch writes.
- `ObjectRef.version` continues to expose the server object version to Python callers.
- Replay-buffer `state()`, `sample()`, and `merge()` continue to call `get_object(..., deserializer=...)`.

**Compatibility:**

- No backward compatibility with old flamepy/cache-server wire behavior is required.
- The public Python `get_object` API remains unchanged.
- `version=0` always requests a full object.
- `version>0` always enables the version-aware behavior described in this document.
- flamepy sends `0` when it has no local cache entry, so first reads and forced refreshes remain full-object downloads.

**Breaking Changes:**

The existing `do_get` response format can change as needed so updated flamepy can distinguish full responses from patch-only responses.

## 3. Implementation Detail

### Architecture

```text
flamepy get_object
  |
  | local cached entry?
  |   no  -> key:0       -> full base + patches
  |   yes -> key:version -> not_modified, patch rows, or full rows
  v
object cache
  |
  | compares client_version with base row version and current version
  v
returns minimal safe stream
```

The important invariant is that patch-only rows are returned only when the client's cached base is still the same base snapshot the server is extending.

### Components

**`object_cache/src/cache.rs`**

- Track `ObjectMetadata.version` as the current object version.
- Use the existing base object's `version` field to detect whether the client's cached base is still valid.
- Set patch object version to the new current version before appending.
- Load metadata from storage before comparing versions in `do_get`, so process restarts do not make version checks look like `0`.
- Parse the existing `<key>:<client_version>` tickets.
- Return full, patch-only, or empty streams according to the retrieval protocol.
- Produce a consistent per-object read snapshot. Implementation should hold the per-key lock while choosing the current version and loading base/patch rows, or verify after loading that the selected current version still matches the loaded row set.

**`object_cache/src/storage/disk.rs`**

- Persist patch row versions by writing the server-assigned patch object, not the client-uploaded version `0`.
- Read patches in order and preserve their versions.
- Reconstruct current version and `delta_count` after reload.
- Treat old stored patch rows with version `0` as pre-RFE445 rows. On load, synthesize contiguous patch versions in filename order from `base.version + 1` through `base.version + delta_count`; future writes persist server-assigned versions.

**`sdk/python/src/flamepy/core/cache.py`**

- Replace the cached `Object(version, data)` shape with a richer `Object` cache entry:

```python
@dataclass
class Patch:
    version: int
    data: Any

@dataclass
class Object:
    version: int
    data: Any
    patches: list[Patch]
    materialized: dict[Any, Any]
```

- Request full or incremental data by choosing the effective request version.
- Parse response rows by `kind`.
- On full response, replace `base`, `patches`, and `version`.
- On patch-only response, require a cached entry, append patches, update `version`, and invalidate materialized values.
- On not-modified, return the materialized cached result.

Generic materialization:

- `deserializer is None`: return the base object, preserving the public API behavior.
- `deserializer is not None`: compute `deserializer(base, [patch.data for patch in patches])`.
- Cache materialized values by deserializer identity within the process. New patches invalidate those values.

Replay-buffer materialization:

- Keep the public `get_object(..., deserializer=...)` contract unchanged.
- Make `ReplayBuffer._deserializer` stateful within the buffer service process.
- Reset the service-local materialized buffer when the base object changes or patch history shrinks.
- Otherwise apply only `deltas[previous_patch_count:]` to the already materialized buffer, then record the new patch count.

Mutations:

- `put_object`, `update_object`, `patch_object`, and `delete_objects` continue to invalidate affected local cache entries.
- `patch_object` should return the new current version from server metadata. It should not mutate a cached object optimistically; the next `get_object` remains the consistency point.

### Data Structures

Server-side object rows:

```rust
pub struct Object {
    pub version: u64,     // base row version or patch row version
    pub data: Vec<u8>,
    pub deltas: Vec<Object>,
}

pub struct ObjectMetadata {
    pub endpoint: String,
    pub key: String,
    pub version: u64,      // current object version
    pub size: u64,
    pub delta_count: u64,
}
```

Patch history must be ordered by version, not just by file index. File index can remain the storage layout as long as reads validate monotonic patch versions.

### Algorithms

**Server mutation path:**

```text
put/update:
  current_version = metadata.version or max loaded row version or 0
  new_version = current_version + 1
  write base object with version = new_version
  clear patches
  metadata.version = new_version

patch:
  current_version = metadata.version or max loaded row version
  new_version = current_version + 1
  write patch object with version = new_version
  metadata.version = new_version
```

**Server Versioned Get:**

```text
acquire per-key read/write coordination

load base + patches
current_version = metadata.version or max loaded row version

if client_version != 0 and client_version == current_version:
  return empty stream

needed_patches = patches with version > client_version
patch_suffix_is_contiguous =
  needed_patches versions are exactly client_version + 1 through current_version

if client_version != 0
   and object.version <= client_version
   and patch_suffix_is_contiguous:
  return needed_patches

return base row + all patch rows
```

**Client `get_object`:**

```text
cache_key = (endpoint, key)
cached = local_cache.get(cache_key)

if ref.version == 0 or cached is None:
  client_version = 0
else:
  client_version = cached.version

response = fetch_object_data(ref, client_version)

if response.not_modified:
  return materialize(cached, deserializer)

if response.full:
  cached = replace_from_full(response)
  local_cache[cache_key] = cached
  return materialize(cached, deserializer)

if response.patches:
  if cached is None:
    request with client_version = 0
  append patches to cached
  cached.version = max_patch_version
  cached.materialized.clear()
  return materialize(cached, deserializer)
```

**Replay-buffer `_deserializer`:**

```text
if no materialized buffer
   or base object changed
   or cached patch count > len(deltas):
  materialized = copy(base)
  applied_patch_count = 0

for patch_batch in deltas[applied_patch_count:]:
  materialized.transitions.extend(patch_batch)
  materialized.total_added += len(patch_batch)

applied_patch_count = len(deltas)
return materialized
```

### System Considerations

**Performance:**

Patch-only get changes read transfer cost from:

```text
O(size(base) + size(all_patches))
```

to:

```text
O(size(new_patches_since_last_read))
```

for clients whose base snapshot is still valid.

**Scalability:**

This benefits workloads with one or more long-lived readers and frequent append-only writers. It does not change write concurrency semantics.

**Reliability:**

Returning a full response is the safety mechanism. If the server cannot prove the requested patch suffix is valid, it sends the full object. `do_get` must return rows from one consistent per-object snapshot; concurrent `patch_object` or `update_object` calls must not produce a response whose base row, patch rows, and advertised current version disagree.

**Resource Usage:**

Client memory may increase because flamepy stores base and deserialized patches instead of only one materialized value. The replay-buffer service also keeps one materialized buffer so `state()` and `sample()` avoid replaying old patch batches. Keep the existing LRU entry limit and count each object entry once. Future work can add byte-based client cache limits.

**Security:**

No new auth surface. Ticket parsing must continue to validate `ObjectKey` before reading storage.

**Observability:**

Add debug logs. Optional cache counters can be added before formal replay-buffer benchmarking for:

- `get_object_not_modified_total`
- `get_object_patch_response_total`
- `get_object_full_response_total`
- response bytes and row counts
- client cache hit/miss counts
- patch upload latency and bytes
- deserializer/materialization latency

These counters are also useful for the replay-buffer evaluation.

**Operational:**

Deploy the cache server and flamepy changes together because the internal `do_get` response schema changes. No runtime switch is required: `version=0` requests full data, and nonzero versions use the incremental behavior. Operators can force full-object reads for debugging or A/B measurement through a benchmark-only path that sends request version `0`.

### Dependencies

No new external dependencies are required.

Internal dependencies:

- Arrow Flight `do_get` streaming in `object_cache/src/cache.rs`.
- Existing flamepy serialization/deserialization helpers in `sdk/python/src/flamepy/core/cache.py`.
- Existing replay buffer example in `examples/rl/replay_buffer`.

Version requirements:

- No package version bump is required for the design itself.
- Implementation should include the change in the next flamepy release because client/server wire behavior changes internally.

### Verification Plan

#### Rust Unit Tests

- Existing `put_object` for a new key creates current object version `1`.
- Patch assigns patch row version `current_version + 1`.
- Update replaces base, clears patches, and advances current object version.
- Nonzero versioned get returns empty for matching current version.
- Nonzero versioned get returns only patches after client version when base is valid.
- Nonzero versioned get returns full object when `client_version < base.version`.
- Nonzero versioned get returns full object when patch history has a gap.
- `client_version=0` always returns the full object.
- Full response framing is base row first, then patches in increasing version order.
- Patch-only response framing contains only patches in increasing version order.
- Concurrent patch/update during get cannot return an inconsistent row set.
- Metadata reconstructed from disk preserves current object version and patch versions.
- Pre-RFE445 stored patch rows with version `0` are assigned synthetic contiguous versions during load.

#### Python Unit Tests

- `get_object` replaces cache on full response.
- `get_object` appends patch-only response to an existing cache entry.
- Patch-only response without local cache retries full.
- Not-modified response returns cached materialized data.
- `version=0` bypasses cache and requests full response.
- Materialized cache invalidates after new patches.
- Existing `deserializer=None` behavior still returns base object only.
- Replay-buffer deserializer applies only newly seen patch batches and resets after base replacement or patch-history shrink.

#### E2E Tests

- Put base, patch twice, read from one Python process twice, verify the second read fetches only the second patch.
- Put base, read, update base, read again, verify full response and correct final data.
- Replay buffer benchmark mode writes configured per-iteration metrics and preserves workload correctness.

## 4. Use Cases

### Basic Use Cases

#### Example 1: Repeated Read with New Patches

1. Buffer service reads replay buffer at version `10`; flamepy caches the base row and patches through `10`.
2. Collectors append patches `11`, `12`, and `13`.
3. Buffer service calls `get_object` again.
4. flamepy sends `<key>:10`.
5. Server sees `base.version=1`, current version `13`, and returns only patches `11..13`.
6. flamepy appends those patches locally and materializes the replay buffer.

Expected outcome: network transfer is proportional to the three new patch batches, not the entire replay buffer history.

#### Example 2: Base Replaced

1. Client caches object at version `10`.
2. Another actor calls `update_object`, creating a new base at version `11`.
3. Client calls `get_object`.
4. Server sees `base.version=11 > client_version=10`.
5. Server returns full base plus any patches after version `11`.

Expected outcome: the client never applies patches to an obsolete base.

#### Example 3: Not Modified

1. Client caches current version `20`.
2. Client calls `get_object` before any mutation.
3. Server returns empty stream.
4. flamepy returns the cached materialized value.

Expected outcome: same behavior as current RFE426 cache hits.

### Advanced Use Cases

#### Replay Buffer Performance Evaluation

**Metrics:**

Primary read-path metrics available from the replay-buffer benchmark hooks:

- `get_object` latency for `ReplayBuffer.state()` and `ReplayBuffer.sample()`.

Optional cache-observability metrics to add before collecting formal numbers:

- Total bytes downloaded by `get_object`.
- Deserializer/materialization CPU time inside `_fetch()`.
- Base size, patch count, and patch rows downloaded per read.
- Number of full, patch-only, and not-modified responses.

Write-path metrics:

- `ReplayBuffer.push()` latency, including `patch_object`.
- Bytes uploaded per collector patch.
- Patch failure count.
- Server `delta_count` before and after collection.

End-to-end metrics:

- Total runtime.
- Transitions per second.
- Collection latency per iteration.
- Failed collector calls.
- Merge latency when `ReplayBuffer.merge()` runs.

**Benchmark Method:**

Primary comparison: run the same replay-buffer workload twice on the same cluster and commit:

```shell
uv run main.py --force-full-get --iterations 50 --collections 20 --steps-per-collection 500 --batch-size 64
uv run main.py --iterations 50 --collections 20 --steps-per-collection 500 --batch-size 64
```

The first run forces request version `0` on replay-buffer reads and keeps merge enabled every 5 iterations by default. The second run uses normal nonzero cached versions after the first full fetch and disables merge by default. This isolates the proposed `get_object` behavior without a runtime switch while preserving compaction for the forced-full baseline.

Add a replay-buffer benchmark mode before collecting final numbers:

- `--metrics-json <path>`: write per-iteration metrics.
- `--merge-every <n>`: override the automatic merge policy.
- `--no-merge`: disable merge, including for forced-full runs.
- `--force-full-get`: force replay-buffer reads to send request version `0` for baseline measurement.

Run at least these cases:

| Case | Purpose |
|------|---------|
| Forced full with default merge every 5 iterations | Baseline that bounds repeated full-read cost with compaction. |
| Incremental with merge disabled | Measures patch-only get plus incremental local materialization. |
| Explicit `--merge-every 1` override | Confirms no regression when full responses are naturally small. |

Secondary comparison: keep the collector work and iteration shape identical, then compare the patch-based replay buffer with a controlled non-patch variant:

- Service-only push path: collectors send transitions to the `ReplayBuffer` service, which owns an in-memory list.
- Whole-object update path: writers fetch, append, and `update_object` the complete replay buffer.

This secondary comparison is not required to prove version-driven incremental get, but it shows where patch-based replay-buffer design sits against simpler alternatives.

**Success Criteria:**

- Patch-heavy replay-buffer reads download at least 70% fewer bytes after the first cached read.
- Median `state()`/`sample()` read latency improves by at least 30% in the no-merge case.
- End-to-end transitions/sec improves in the no-merge case or remains within 5% in cases dominated by environment stepping.
- `patch_object` write latency does not regress by more than 5%.
- No correctness difference in final `size`, `total_added`, sampled batch sizes, failed collector count, or episode counts.

## 5. References

**Related Documents:**

- GitHub issue: https://github.com/xflops/flame/issues/445
- `docs/designs/RFE318-cache/FS.md`
- `docs/designs/RFE426-cache-versioning/FS.md`
- `docs/designs/RFE423-app-cache-key/FS.md`

**External References:**

- Apache Arrow Flight protocol: https://arrow.apache.org/docs/format/Flight.html
- PyArrow Flight API: https://arrow.apache.org/docs/python/api/flight.html

**Implementation References:**

- `object_cache/src/cache.rs`
- `object_cache/src/storage/disk.rs`
- `sdk/python/src/flamepy/core/cache.py`
- `sdk/python/tests/test_cache.py`
- `examples/rl/replay_buffer/replay_buffer.py`
- `examples/rl/replay_buffer/main.py`
- `examples/rl/replay_buffer/README.md`

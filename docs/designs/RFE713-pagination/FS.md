---
Issue: "#713"
Related: "#719"
Status: Draft
Date: 2026-11-15
---

# List pagination

## 1. Motivation

Add pagination to the existing Frontend list RPCs. Applications, nodes,
executors, and sessions are listed from in-memory metadata. Task lists read
metadata from storage. Each response contains one page of object metadata
and paging information.

## 2. Function specification

### Paging

Requests and responses share this message:

```protobuf
message Paging {
  int32 limit = 1;
  int64 number = 2;
  optional uint64 total = 3;
}
```

Each resource filter contains `paging: Option<Paging>`. Existing `list_*`
methods accept that filter for both selection and paging. Filters have no
separate limit field; capped queries request the first page through `paging`. Frontend list
requests carry the same filters and `Paging paging`; responses also carry
`Paging paging`.
A request's `total` is ignored.

- Zero or omitted `limit` selects 100; explicit limits are 1 through 1000.
- Zero or omitted `number` selects 1. Negative values are invalid.
- Calculate `offset = (number - 1) * limit` with checked arithmetic.
- Responses report the effective `limit`, `number`, and filtered `total`.
  These list RPCs always populate `total`; an empty result has `total = 0`.
- A page beyond the end is empty and carries the filtered total.

The internal `Paging` type applies defaults and validates bounds and offset
overflow when constructed or updated, including RPC decoding. Its fields are
private; read accessors return the validated values directly.

Filter, count, order, and select a page from the same captured metadata view.
Requests for subsequent pages read the current state, so concurrent changes
can cause duplicates or omissions across pages. SDK list calls return exactly
one page; callers choose subsequent page numbers through the filter's paging
field. Callers can compare `number * limit` with `total` to determine completion.

### Listing sources, filters and ordering

| Resource | Source | Exact filters | Default order |
| --- | --- | --- | --- |
| Applications | In-memory applications | Names, state | Enabled first, creation time descending, name ascending. |
| Sessions | Cached sessions | IDs, application, state | Open first, creation time descending, ID ascending. |
| Nodes | In-memory nodes | Names, state | Ready first, creation time descending, name ascending. |
| Executors | In-memory executors | IDs, node, application, session ID, state | Node ascending, numeric state ascending, ID ascending. |
| Tasks | Storage task metadata | Required session ID, task IDs, states | Numeric task ID descending. |

Repeated values within a filter are OR; different filters are AND. Empty
repeated filters are unrestricted. Invalid enums, identifiers, and paging
return `INVALID_ARGUMENT`.

State priority precedes creation time. All non-Open session states share
one priority group; non-Enabled applications and non-Ready nodes likewise
share their respective remaining group. Task ordering is independent of
state and timestamps. Identifiers break ties deterministically for other resources.

The cached Node carries its creation time, restored from persisted metadata or
set at initial registration for `none` storage. Updates and re-registration
preserve that field. Node ordering reads it directly from the object.

### Session retention and limits

Retain every session in the existing session cache, including Closed sessions.
Keep the same Session object and pointer across state transitions. Session fields
and final task counters remain available after close.

The session manager's task cache and task index contain only Pending and Running
tasks. Terminal tasks are removed from both while their counters remain on the
Session. Closing finishes lifecycle cleanup and leaves no tasks or task index
entries in the retained Session. Historical task metadata is read from storage
for task lists, without rebuilding a historical task index in the session manager.

`max_sessions` limits the number of Open sessions. Enforce admission atomically
on creation/open and transitions into Open, including resume. Other states
consume no Open slot. Closing frees a slot after the committed state change
is published. Startup restores existing sessions even if their Open count
exceeds a reduced limit; further Open admissions wait for available capacity.

`Storage::load_data()` loads all persisted sessions into the same cache regardless
of state or age. Restore session fields and final counters for Closed sessions
without loading their tasks into the session manager's task cache or task index.
Recover non-Closed execution state during the existing traversal, retaining only
Pending and Running tasks in the cache and index. Session deletion removes its
cached representation and persisted data through the existing lifecycle.
Detailed getters and list metadata projections use the retained session.

With `none`, the storage engine continues to retain authoritative session/task
history in memory independently of the session manager's active-task cache.

Always-on history collection removes expired Closed sessions according to
`cluster.history.retention`, defaulting to 30 days, on a fixed four-hour interval.
See the [history collection design](../RFE719-session-history-gc/FS.md) for
eligibility, lifecycle coordination and retry behavior.

### Response metadata and clients

Reuse existing resource/list protobuf messages. Return metadata needed by
normal list output, including session resources, priority and task counters,
and node capacity/information. Exclude task input/output/affinity bytes,
session common-data bytes, tokens, credential environments, and events.
Timestamps use Unix milliseconds.

Rust and Python sync/async clients return `Page<Resource>` with metadata and
`Paging`. One call returns one page. `flmctl list` preserves its metadata
columns and accepts `limit` and `number`. Detailed getters and task watches
retain their existing behavior.

Internal lifecycle queries return all matches or perform an existence check,
independently of gRPC paging defaults. Application cleanup and node lifecycle
operations retain full execution objects where needed. Scheduling continues
using `Storage::snapshot()`. `flmctl delete` inspects only the selected page and deletes its Closed
sessions or NotReady nodes. Apply inactivity checks after page selection;
Ready nodes and non-Closed sessions are skipped. The command accepts `limit`
and `number` and never traverses later pages. Health checks explicitly walk
all pages. Server-side lifecycle checks remain authoritative for deletion.

## 3. Implementation detail

The existing storage, controller, Frontend, SDK and CLI modules implement the
change.

- Use the existing session cache for every state. Close updates the retained
  Session in place and finalizes task counters; deletion removes that cache entry.
  Keep terminal tasks out of both its task map and task index.
- Populate metadata during `load_data()` and maintain it after each committed
  mutation, including partial operations that later return an error. Complete
  startup recovery before serving requests or starting background mutations.
- Capture resource metadata under short locks, then filter, sort, count, and
  page after releasing the locks. Metadata projections copy neither session
  task maps nor payloads. Hold no directory lock across I/O or `.await`.
- Select task pages inside the storage engine, in descending numeric ID order.
  For an unfiltered filesystem page, derive the total from the fixed-record file
  length and seek directly to task ID `total - paging.offset()`, reading at most
  `paging.limit()` records backwards. An offset at or beyond the total returns
  an empty page without reading headers. Page cost is independent of history size.
- Exact task-ID filters read the requested records directly. State filters may
  scan headers to determine the exact filtered total and page; retain only the
  selected page's task metadata. Payload files are never read for listing.
  `none` applies the same filters, descending ID order, and paging to its
  authoritative task history. Internal requests without paging return all matches.
- Update shared protobufs, SDK mirrors/generated clients, and all list callers.
  Internal full-result queries retain their completeness guarantees.

## 4. Verification

Tests belong in dedicated `tests/` files in their owning modules.

1. Test paging defaults, bounds, overflow, totals, empty pages, exact filters,
   state priorities, creation-time order, and deterministic ties.
2. Verify all retained sessions are loaded regardless of age. Closed sessions
   retain the same cached Session and final counters with empty task maps and
   indexes.
   Preserve Suspended/Suspending recovery for filesystem and `none` storage.
3. Race close, list and delete; verify one cached Session throughout the state
   transition. Test concurrent Open admissions, close freeing capacity, and
   startup above a reduced `max_sessions` limit.
4. Verify complete application/node lifecycle queries beyond 100 entries and
   unchanged scheduler snapshots.
5. List tasks with valid headers and unreadable payloads. Verify metadata-only
   reads and descending task-ID ordering regardless of state or timestamps.
6. Verify Rust/Python page metadata, CLI columns, detailed getters and watches,
   selected-page deletion and full page traversal in health checks.
7. Page over 100,000 historical tasks; verify unfiltered pages read only the
   selected fixed-size records, including later pages and beyond-end offsets.
   Verify out-of-page unreadable headers do not affect unfiltered reads, filtered
   totals remain exact, and page allocations are bounded independently of history
   and payload size.

---
Issue: "#719"
Related: "#713"
Status: Implemented
Date: 2026-11-16
---

# Session history garbage collection

## 1. Scope

The session-manager Controller removes expired Closed sessions to bound retained
history. The Controller selects sessions, coordinates their lifecycle and
reclaims detached directories. Retention uses the Storage and Engine
`stale_session` operation to remove sessions from the live namespace.

`flm export` will send history to a history server in separate future work.
Retention collection removes history without implementing archival, export
transport or export acknowledgements.

Sessions of every age continue to load at startup. Retention is applied after
recovery rather than by a startup age cutoff. Active sessions remain available.

## 2. Configuration

```yaml
cluster:
  endpoint: http://127.0.0.1:8080
  history:
    retention: 30days
```

`cluster.history.retention` is the only setting, defaulting to `30days` when
omitted. Collection is always enabled, with a fixed four-hour interval and no
overlapping passes. Session lists use pages of 100 metadata entries.

Parse retention through the existing human-duration support. Reject zero,
invalid durations and timestamp arithmetic overflow at startup. Configuration
changes require a session-manager restart. Provide matching Helm values, schema
validation and operator examples. Increasing retention cannot restore history
already deleted.

## 3. Eligibility

`collect_history()` reads UTC time once per pass and the retention initialized
with the Controller, then calculates `cutoff = now - retention`.
A candidate must satisfy all of the following at deletion time:

1. The current state is exactly Closed.
2. Completion time is strictly earlier than the cutoff. Use creation time only
   for legacy records without completion time.
3. The selected incarnation is still current.

List/load decoding and the caller's retention selection handle invalid raw
timestamps instead of silently turning them into legacy missing values. Invalid
timestamps, completion before creation and future
timestamps defer removal. Equality with the cutoff is retained until a later pass.
Open, Suspending, Suspended and unknown states are never collected. `max_sessions`
continues to limit Open admission independently of retention.

Selection returns session metadata. Before removal, capture the current cached
Session pointer. Use that Session's existing mutex to revalidate application,
creation time, version, state, age and pointer identity. Storage serializes the
subsequent removal with its existing asynchronous session-admission mutex.

Closed is the lifecycle prerequisite established by the close path. GC and
explicit deletion trust that state: they do not scan task records, inspect task
counters/maps or query assigned executors. A Closed session can still have an
executor association. Retirement leaves executors and their application-scoped
services unchanged.

## 4. Controller ownership and scheduling

`session_manager/src/controller/gc.rs` owns selection, eligibility, collection,
cleanup retries and the periodic worker. Start its supervised worker after
storage recovery and Controller initialization, alongside the existing managers.

Each pass lists Closed sessions through the existing session filter API in
pages of 100. Storage counts and pages matching Closed sessions regardless of
age. GC checks completion time against its retention cutoff after listing, using
creation time when completion is absent. Timestamp validation and retention
eligibility stay entirely in the collector.

The pass starts with page one and continues until it has traversed the matching
sessions. After removals, revisit the current page because subsequent entries
shift into it. Otherwise advance to the next page when more matches remain.
The page number is local to that pass. Each page reads the current metadata
view, so concurrent updates can shift entries; deferred sessions are reconsidered
on the next pass.

The worker awaits one blocking task at a time. That task performs collection
outside frontend, backend and scheduler runtime threads. Use the timer's skip
behavior to avoid bursts of catch-up passes. Slow deletion is acceptable;
unrelated running work takes precedence over reclaiming disk space.

## 5. Detachment and cleanup lifecycle

GC selects expired sessions in `collect_history` using the retention cutoff,
then calls `Controller::stale_session` with only the existing Session pointer.
That operation lives alongside `Controller::delete_session` in the Controller
module and checks Closed state. For each selected session:

1. Recheck the observed metadata and current cached incarnation. Defer busy
   Session mutexes. Retention selection belongs to the GC caller.
2. Call `Storage::stale_session` with the expected cached pointer.
3. Storage acquires its existing asynchronous `session_admission` mutex. While
   holding it, briefly lock the sessions directory and expected Session to check
   pointer identity and `Session::is_closed()`, then release those short locks.
4. Retain admission while removing events through EventManager and directly
   awaiting `Engine::stale_session`. Propagate errors without retiring the cached
   entry. The sessions-directory and Session mutexes are not held during I/O or
   across the backend await.
5. The filesystem engine checks Closed state and registry identity under its
   session lock, then renames the session directory into `.history/<uuid>` on
   the same filesystem and removes its live engine entry. It does not scan tasks
   or decode a session header just to validate detachment.
6. Briefly lock the cache again to clear and evict only the exact expected
   pointer, then release admission. The Controller discovers detached paths through
   its initialized history root.

Explicit `Controller::delete_session` checks Closed state and calls the separate
`Storage::delete_session` operation for immediate physical deletion. Storage uses
that same admission sequence through identity checks, event cleanup, the awaited
backend operation and conditional cache retirement. A delayed operation carrying
an old pointer cannot remove a replacement incarnation. Ordinary deletion
decodes metadata because it returns a Session.

Storage awaits the backend directly, without spawning a retirement worker or
blocking-worker bridge. Cancelling while the backend future is pending releases
admission and retains the old cached session, permitting a retry. Failures likewise
release admission without evicting that entry. There is no deleting flag, RAII
claim or new lifecycle-lock map. The close path already retires task notification
channels; stale/delete do not repeat cleanup. Close/suspend notifier cleanup
retains its identity checks.

Detachment moves the existing directory without copying metadata or task
payloads. A failed rename leaves the live session in place. The `none` engine
removes its authoritative in-memory session. `stale_session` returns only success
or failure; GC already knows the history root.
There is no manifest, archive protocol or storage-owned cleanup worker.

`Controller::new_ptr` initializes immutable GC state containing retention and
the filesystem history root. `run_gc` uses that state to schedule collection.
The `fs://`, `file://` and `filesystem://` aliases use an absolute base path
unchanged, or resolve a relative path under `FLAME_HOME` (default
`/usr/local/flame`), then append `.history`. The `none` backend has no filesystem
history directory. Storage exposes no cleanup or enumeration API.

At the end of each pass, the Controller enumerates `.history` and recursively
removes every actual child directory, preserving the root itself. Directory
names need not be UUIDs. Files and symlinks are skipped, and a symlink history
root is rejected. A missing root is successful cleanup. Failures are logged and
retried on a later pass. Enumeration covers the full directory without an entry
budget or saved cursor. Cleanup runs on the outer collection worker after
session admission has been released. Recursive removal can take substantial
time; the next pass waits for the current worker to finish. Object-cache
reclamation proceeds independently.

The admission mutex serializes create, open, close, resume and deletion through
the backend operation. New admission waits while deletion is in progress. Other
session lookups and task creation in existing sessions can proceed while the
backend future is pending because neither the sessions-directory mutex nor a
Session mutex spans that await. The later recursive `.history` cleanup runs on
the outer GC worker after admission is released. Engine session locks continue
to order task reads against detachment: reads finish before it or observe
NotFound. Previously captured metadata snapshots may finish while new lists/gets
observe removal from the live namespace.

After restart, the Controller constructor resolves the history root again.
The next GC pass removes its child directories, including sessions detached by
a previous process. Detached directories remain outside the live session
namespace and do not restore deleted sessions.

## 6. Object-cache ownership

Object-cache replicas reclaim orphaned data through their own existing lifecycle
APIs; the session manager does not traverse opaque payload references, unlink
replica files or expose a public cache-delete RPC. Reclamation is eventual after
session deletion and owner absence confirmation.

For each deletion candidate, use exact owner filters on existing Frontend list
RPCs and require valid identity/total metadata. A retained Closed session still
owns its objects. Do not infer absence from a capped or changing multi-page list.

The FSM's successfully served owner view is authoritative for object-cache GC.
If the exact query returns no session/application, FOC may remove its data even
when FSM skipped unreadable metadata during startup. Do not probe filesystem
directories or add an owner-existence API. Lookup/transport errors still defer
collection; an empty successful response is absence.

Preserve creation/recreation grace periods. Under the owning cache storage lock,
delete only if the object UUID and observed version are unchanged. Changed or
replaced objects are deferred. Owner protection applies to the final lookup;
subsequent owner recreation is not a distributed transaction with replica unlink.
References into an expired session namespace expire with its history. Shared
application data belongs in the shared namespace, and package/shared namespaces
are not removed by session-history collection.

## 7. Observability and verification

Log retention and the fixed interval at startup. Per-pass logs report examined,
removed, deferred and failed counts and duration. Failure logs identify
the session or detached path and reason without credentials or payloads.

Verification covers:

1. Default/omitted/invalid retention, Helm examples/schema and the fixed four-hour
   schedule with skipped missed ticks.
2. Strict completion cutoff, completion preference, legacy fallback,
   invalid/future timestamps and preservation of active states.
3. Traversing all Closed sessions in pages of 100 within one pass, advancing
   past deferred entries and revisiting the current page after removals.
4. Busy Session mutexes, preservation of Open sessions and unchanged executors
   and services while Closed sessions are retired, including those with executor
   associations. Verify failure retries and identity-safe retirement after
   same-ID recreation.
5. Admission-serialized removal and recreation, cancellation of a pending backend
   future retaining the cached session for retry, existing-session task progress
   while the backend is pending, and admission release before history cleanup.
6. Filesystem detachment without payload copies, rename failures, `none` removal,
   same-ID reuse and removal of every child directory while preserving `.history`.
7. Restart between detachment and removal, cleanup of earlier detached sessions,
   retry after removal failure and preservation of external symlink targets.
8. Exact cache-owner probes above 100 resources, unreadable owners, lookup failures,
   UUID/version replacement races and preservation of package/shared data.

Tests live under their owning modules' `tests/` directories.

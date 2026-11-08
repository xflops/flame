# Design: Finished task cache eviction

Tracking issue: [#659](https://github.com/xflops/flame/issues/659).

Status: implemented. This document describes task-cache ownership and
persistent-history read contracts.

## 1. Motivation

The session manager retains every task in `Session.tasks` and
`Session.tasks_index`, including its input, output, and affinity data.
`Storage::load_data` reloads the same history through `Engine::find_tasks`.
Consequently a long-lived session retains memory proportional to all tasks it
has ever executed, even when it has no work left.

The scheduler needs active tasks. Clients still need completed results, failure
details, lists, and late watches. Filesystem and SQLite storage already own
those records; retaining a second copy indefinitely is unnecessary.

The target is to make the scheduling cache retain only active tasks for every
backend, with constant-size state counts per session. NoneEngine deliberately
retains complete task history in its own memory for fast reads and updates.
Historical reads must continue to work without repopulating the scheduler cache. Recovery and
listing may materialize complete task history temporarily; the target is the
retained session cache after those operations finish.

## 2. Function Specification

### Configuration and compatibility

No new user configuration, CLI flags, protobuf fields, or SDK methods are
required. Terminal tasks leave the scheduling cache for filesystem, SQLite,
and `none` storage. Filesystem and SQLite retain their history on disk;
NoneEngine retains all task records and results in memory. Get, watch, and list
can read finished tasks from every engine after scheduling-cache eviction.
Session counters remain accurate for every backend. NoneEngine history lasts
until explicit session deletion or loss of the engine process; a new instance
starts empty and provides no disk recovery.

For every backend, get, list, and watch responses retain their input/output and
event semantics. In particular, the current method named `get_task_metadata` means
"without historical events"; it does **not** mean "without payloads". Do not
silently remove output from watch responses or input/output from list responses.
Unknown task/session IDs remain `NotFound`; storage failures remain errors.
For reads, a cache miss alone is no longer evidence that a task does not exist.
For mutations, active cache membership is required: skip an update when its
task is absent. Historical read fallback never authorizes a task mutation.

`ListTasks` remains a server stream. It returns all active tasks copied from
the session cache, followed by all finished tasks read from storage, without a
limit or pagination. IDs already present in the active snapshot are excluded
from finished history so a completion racing the history read does not produce
a duplicate. This is not a transactional snapshot: task creation and state
changes during the two reads can affect the records and states returned.
Existing clients need no changes. Task input/output semantics are preserved.

### Scope

In scope: terminal scheduling-cache eviction, exact state counts, engine read fallback,
active-only scheduling cache after recovery, complete task lists, late completion
handling, and in-memory task history for `none`. Terminal states are `Succeed`, `Failed`, and `Cancelled`, using
`TaskState::is_terminal()`.

Out of scope: evicting active tasks or sessions, payload garbage collection,
changing session retention, retrying terminal tasks, pagination or list limits,
distributed writers, executor service lifetime changes, and a general
cache or LRU. Application services remain executor scoped across bindings.

Related contracts remain intact: RFE546 watches subscribe before taking their
first snapshot, notifications carry task IDs, and terminal responses load event
details. Session close removes event history under the current retention rule;
eviction does not change event retention. A new watch on an already-closed
session remains rejected by the current subscription contract.

## 3. Implementation Detail

### Current implementation constraints

- `Session::update_task` currently handles both admission and replacement. Its
  version guard works only while an old entry exists. Reusing it unchanged
  after eviction would permit stale tasks to be reinserted.
- `Storage::update_task_state` and `update_task_result` admit tasks through the
  active cache, then call the same engine API for every backend. `none` updates
  its retained Task inside the engine; Storage must not fabricate updates from
  `NotFound` or inspect the configured storage type.
- Filesystem and SQLite retain their existing task mutation contracts. The
  controller/storage orchestration layer must check active cache membership
  before an engine write. A stale caller-held pointer does not authorize a
  write after eviction.
- Session RPC conversion and `SessionInfo::try_from` both derive counts from
  `tasks_index`. Scheduler snapshots already copy only active task details.
- `ListTasks` remains streaming at the wire and may allocate the complete task
  list before sending. It must combine active cached tasks and finished storage
  records without repopulating the active cache.
- `Storage::close_session` cancels pending tasks in a batch. Recovery retries
  `Running` tasks only for `Open` sessions; historical `Running` records in a
  closed session must not be rewritten just to simplify the cache.

### Ownership and counts

For every engine, `Session.tasks` and `tasks_index` own only active
`TaskPtr`s. Terminal transition publication removes the ID from **all** index
buckets and from `tasks`. An in-flight operation can retain a temporary Arc;
eviction releases the session's ownership rather than promising immediate RSS
reduction. No per-terminal-task tombstone, version map, or deduplication set is
retained.

Add `TaskCounts` with five `u64` fields to the cached Session. Counts describe
logical task states, independently of scheduling queue membership. This matters
because `pop_pending_task` removes a task from the pending index before its
Running update is published. Both the RPC conversion and scheduler snapshot
use one shared count accessor. Preserve the existing protobuf `int32` fields;
use checked/saturating conversion rather than wrapping above `i32::MAX`, and
document that existing wire limit.

Counts are derived state, not a new persisted counter file/table. Startup
rebuilds them by loading all stored tasks and counting their states. Creation
increments Pending once; accepted state publication decrements the old state
and increments the new state under the Session lock. Retrying Running to
Pending uses the same delta.
A repeated publication that finds no active entry cannot increment a terminal
counter. NoneEngine also maintains session counts from its authoritative stored
task records and mutations. It retains those records, payloads, and current
versions in memory independently of the scheduling cache.

Keep the existing `Session::update_task` insert/update helper. Storage allows
insertion for creation; ordinary task mutation first checks active cache
membership before the engine write, and publication checks membership again
while holding the Session lock. That check prevents late responses
from reinserting evicted IDs without a separate admission function:

| Operation | May insert an absent ID? | Terminal handling |
| --- | --- | --- |
| Successful creation | Yes, exactly once | Normally Pending |
| Quiescent recovery | Yes, active records only | Count, without caching |
| Task update/retry | No | Skip if absent; count accepted transition, then evict |
| Get/list/watch | No | Return detached engine snapshot |

Do not advance the shared cached Task in place before comparing versions.
Compare the returned committed version with the cached version, then publish
under the Session lock. Ignore older/equal versions. A delayed Pending/Running
publication after terminal eviction sees no entry and is ignored. Insertion
must never be a fallback for an ordinary update response.

### Engine read interfaces

Keep the existing full task read used by recovery, and add a finished-history
read for listing:

| Query | Contract |
| --- | --- |
| `find_tasks(&TaskFilter)` | Return all matching stored tasks and payloads, with no limit; filter by owning session and optional states |
| `get_task(gid)` | Existing point read of a full committed Task, without event history |

Reuse the existing `TaskFilter`, following the other `find_xxx` query conventions.
`TaskFilter::by_session` matches all states for recovery.
`TaskFilter::terminal` selects `Succeed`, `Failed`, and `Cancelled` records for
listing. Explicit state lists select those states; an empty state
list matches no tasks. Unknown sessions and storage errors remain errors.
Filesystem reads inspect fixed-size `tasks.bin` metadata before materializing
matching finished-task payloads. SQLite selects terminal states in its history
query. No summary, cursor, high-water query, or internal pagination interface is
needed. Startup calls `find_tasks` and may load every historical input, output,
and affinity payload temporarily before discarding finished records.

The filesystem also has a second history-sized structure:
`SessionEntry.tasks` retains a lock per task forever today, including locks
created by metadata reads. Bound it as part of this change. In the synchronous
`with_task` / `with_task_read` wrappers, scope the local task Arc and its guard
around the operation, then prune that ID while holding the registry mutex only
if the registry is its sole Arc owner. Lookup/clone and the ownership test/removal
must share that mutex. Waiting and running operations retain their Arcs, so the
entry cannot be removed while either exists; a later lookup cannot create a
second gate for the same task. Centralize cleanup, including error returns,
rather than removing gates on terminal transition alone. Audit direct
`task_lock` users and test concurrent readers/writers/waiters. Metadata scans
must leave no per-ID gate history behind. This bounds lock entries by in-flight
operations rather than completed-task count without introducing striped-lock
contention.

Existing corruption/I/O errors must surface instead of silently dropping tasks
or returning partial counts. SQLite missing-row reads must be normalized to
`NotFound`, distinct from database failure. Full recovery and list reads may
allocate memory proportional to total history and payload sizes. Existing
response queues and concurrent requests also contribute transient memory.

### Mutation admission and publication

```mermaid
sequenceDiagram
    participant C as Controller / Storage orchestration
    participant S as Session cache
    participant E as Engine
    participant W as Task watch
    C->>S: Check membership; clone current active Task
    alt Task is absent
        C->>C: Skip mutation
    else Task is active
        C->>E: Apply mutation with active Task snapshot
        E-->>C: Committed Task
        C->>C: Record task event
        C->>S: Publish if ID exists and version advances
        S->>S: Update counts; remove terminal ID
        C->>W: Notify task ID
    end
    W->>E: Read historical task on cache miss
```

The controller/storage orchestration layer owns active task admission. Before
updating state, completing a task, or retrying it during executor cleanup,
resolve the task from the active cache. If it is absent, skip the mutation.
Do not read the historical record to reconstruct a mutable TaskPtr. The same
rule applies to `none`, even though its engine retains historical records.
Recovery is a separate, quiescent path: it retries only Running records selected from storage
before publishing the recovered session.

The healthy FEM lifecycle awaits launch, invocation, and completion in sequence.
Use that lifecycle and the existing active-cache admission check rather than
adding a lock around engine writes, events, and cache publication. A late
operation that starts after terminal eviction finds no active task and returns
without a write, event append, or counter change. Executor-cleanup retries use
the same active-cache rule.

`Engine::update_task_state` and `Engine::update_task_result` receive a snapshot
of the current active cached Task and return `Result<Task, FlameError>`.
Storage resolves that snapshot during active-cache admission, releases cache
guards, and calls the same mutation API for every backend. It performs no
storage-type checks and fabricates no fallback update on `NotFound`.

All backends use the supplied snapshot to identify the task while their stored
record remains authoritative for state and version. NoneEngine validates its
session lifecycle, updates its stored Task and session counters, and returns
the newer record. It retains input/output, results, and current versions in
memory. Filesystem and SQLite perform their existing authoritative storage
mutation under their transaction/task lock. A caller's cached state or version
cannot replace the stored authority.

NoneEngine looks up a shared session entry under a short global directory lock,
then releases that lock before taking the session-local mutex. After briefly
rechecking directory pointer identity, it accesses task history, lifecycle
state, allocation, and counters under only the local mutex. Deletion removes
the matching entry; stale waiters return `NotFound` after the identity check.
Unrelated sessions do not share a task/history critical section.

SQLite's `mutate_task` retains `BEGIN IMMEDIATE` before lifecycle validation and
task mutation. Lifecycle validation selects only the session's scalar state
instead of loading and decoding a complete Session, including its common data.
Missing-session and invalid-state errors keep their existing behavior, and the
writer reservation continues to coordinate task writes with session close.

Active admission remains above the engine boundary. The initial active-task
lookup rejects a Closed session, and engine lifecycle validation rejects writes
after close. Publication uses only the existing Session-locked membership and
version checks: close clears the task maps, so a late engine snapshot finds no
active entry and is skipped without another Closed-session check. Eviction
introduces no engine terminal-state predicate, transition-outcome wrapper, or
`applied` flag. Persistent `NotFound` and other engine errors propagate for an
admitted mutation. Historical reads always try the engine on a cache miss and
return retained finished records for `none` as well as persistent engines.
Unknown task/session IDs remain `NotFound`. These internal Rust interface
changes require no SDK or protobuf changes.

After a successful write and event-record attempt, publish the returned Task
under the Session lock. Compare versions before changing counters or replacing
the cached pointer, and never insert an absent ID during update publication.
A successful terminal publication updates counts and evicts the task. A later
sequential duplicate or conflicting update sees the missing active entry and
skips the mutation. Publication still checks active membership and versions
under the Session lock, so an old response cannot reinsert an evicted task.

Publish eviction after the event-record attempt, preserving the existing
write, event, then cache-publication order. Engine commit, EventManager append,
and cache publication remain separate operations with their existing failure
behavior; no new lock or atomic operation spans them.

Do not hold global storage-map locks or synchronous Session/Task guards across
engine awaits, event I/O, or network sends. Take pointers/snapshots in lexical
scopes. Use the existing session-before-task cache lock order and engine-local
lifecycle/task locking; do not introduce a terminal-history lock registry.

### Reads and late controller messages

Make Storage and Controller task get methods async and await their results
through frontend handlers and watch refresh paths. On a cached hit, clone the
current Task in a short scope. On a miss, call the engine without holding cache
locks and return a detached snapshot. Never admit that snapshot as a side
effect of a read. `get_task` adds EventManager history; `get_task_metadata`
clears events. A successful read can race a deletion and linearize before it;
a read after deletion returns `NotFound`, without recreating the session.

Keep `get_task_ptr` explicitly cache-only for scheduler-owned active work and
mutation admission. Completion, cancellation, executor unregister, and retry
paths skip task mutation on a cache miss. A caller-held task pointer or a
detached history read cannot restore active ownership. Remaining executor
cleanup validates that the executor still owns the expected assignment before
clearing it, without another task transition or counter increment. An absent
assignment keeps the existing invalid-state response when the request provides
no task identity; do not retain historical assignments just to acknowledge it.

Controller lifecycle paths use the active cache only. Launch/relaunch of an
assigned task that is absent clears the stale assignment and returns no task;
it does not read or replay history. Completion skips the missing task update
while cleaning up the expected executor assignment. Unregister likewise skips
the missing task update.

Client get/watch/list reads use historical fallback. `Storage::get_task_metadata`
returns an existing active or terminal engine record on a cache miss without
admission to the scheduling cache. Recovery remains the explicit mechanism for
rebuilding active scheduling state. Historical reads never grant permission
for a controller lifecycle mutation.

Preserve watch subscription-before-snapshot ordering. First registrations,
duplicate registrations, matching task notifications, lag refreshes, and final
refreshes all use async fallback. Terminal responses include output and the
available event history before removing the registration. Notify only after
the event-record attempt and cache publication. A notification arriving
before a registration is harmless because registration reads storage. Cache
eviction does not change watch channel closure or session-close behavior.
Task commit and EventManager append remain separate operations. A crash between
them may leave a committed terminal result without its final diagnostic event,
as today. Skipped updates after eviction append no event. Atomic event
persistence is outside this change.

### Complete listing

Clone all active Tasks from `Session.tasks` while holding the Session lock for
that snapshot, then release the lock before reading storage. Read all finished
tasks using `Engine::find_tasks(&TaskFilter::terminal(session_id))`. Append only history records whose
IDs are absent from the active snapshot, and return the active records first,
followed by the finished history. There is no task limit, cursor, high-water ID,
or pagination. A persistent history query error fails the list rather than
silently returning only active work.

Excluding active snapshot IDs is required because a task can finish after the
cache snapshot and appear in the storage result. Keep its active snapshot once,
rather than returning two states for the same ID. These reads do not form a
transactional snapshot: concurrent creation or completion can change which
records appear and which states are observed. No cross-source chronological or
task-ID ordering is promised.

For `none`, read finished history from the engine's retained in-memory records
through the same filtered query. Read `Session.tasks`, not `tasks_index`: a
dequeued Pending task may
be temporarily absent from every index while still owned by the active cache.
Use the same complete-list path for Controller and frontend consumers; scheduler
logic continues using active snapshots rather than historical task lists. Do
not hold cache or engine guards while awaiting network sends.

### Recovery and session lifecycle

For each recovered session, call `Engine::find_tasks(&TaskFilter::by_session(session_id))` to load all stored tasks,
including finished records and their input/output/affinity payloads. Derive the
five `TaskCounts` fields from their states. For Open sessions, retry each Running
task through the existing engine API and apply its Running-to-Pending count
delta. Suspending/Suspended sessions keep their existing recovery policy.
Cache only nonterminal records, then discard
the temporary full history. Publish the recovered Session only after its active
cache and counts are complete. Fail recovery on read or mutation errors rather
than exposing partial counts.

Normal close leaves only terminal tasks, so recovered Closed sessions retain
counts without task cache entries. Preserve legacy nonterminal records and their
existing recovery behavior; do not rewrite them just to empty an index. Recovery does not reduce startup payload I/O or
peak history allocation; it reduces scheduling-cache retention after recovery.
A new NoneEngine instance is empty because it has no disk persistence. Retained
records remain readable if only the scheduling cache is rebuilt against the
same live engine instance.

All engines implement the same close contract:

```rust
async fn close_session(&self, id: SessionID) -> Result<Session, FlameError>;
```

Storage obtains the cached session pointer and calls `Engine::close_session`
with only the session ID. The engine owns Running-task validation and counting;
Storage takes no counter snapshot and performs no cached Running preflight. A
successful engine response contains the final Closed status, version, completion
timestamp, and accurate `task_counts`. Storage uniformly applies those returned fields and clears both task maps under the
Session lock. It needs neither a backend-specific close branch nor a separate
post-close task-history read or public summary query. Existing event cleanup
remains part of successful close.

Under their existing close lock or transaction, filesystem and SQLite reject
Running work, cancel only still-Pending records, preserve terminal records, and
derive final counts from authoritative stored task states. They also return authoritative counts on repeated close.
This covers a completion committed before close while cache publication was
still delayed, without guessing from cached Pending records.

NoneEngine uses its actual stored task records for close and counters. It
rejects Running work, changes stored Pending records to Cancelled, preserves
finished results, and returns the final accurate counts and lifecycle metadata.
Repeated close returns those final values without recancelling tasks. Close
keeps the session and task history in memory; scheduling-cache eviction also
keeps it. Explicit session deletion removes that history, and a new engine
process starts empty. No cached counter parameter or separate counter registry
is needed.

Engine close failures propagate. A retry returns the engine's final counters
if close already succeeded before its response or cache publication was lost.
Delete and session retention remove storage and cached counts together; this
design neither extends nor shortens retention.

### Resource and operational considerations

For every backend, the scheduling task cache retains O(active work), with
O(sessions) counters. NoneEngine additionally retains O(all retained task
history and payloads) in backend memory intentionally for performance.
`max_sessions` limits scheduling-cache residency, not NoneEngine history;
explicit session deletion or loss of the engine process releases that history.
Recovery and complete task lists may also allocate full histories temporarily.
Point reads and response queues add their existing allocations. Persistent
engine disk consumption remains unchanged.

Disk fallback increases historical read latency. Record cache-miss/engine-read
latency, active cached task counts, evictions, and failed terminal transitions
using bounded-cardinality metrics or tracing. Do not label metrics with task
or session IDs or log payloads. Existing authorization, transport security,
and session selection apply to fallback reads without bypasses.

Existing task records rebuild counts without rewriting payloads or filesystem
records. Every engine derives accurate counts from its own task state and
mutations; NoneEngine retains its task state and session metadata in memory.
SQLite adds a `(ssn_id, id)` index through its normal migration runner to support per-session filtered
`find_tasks` reads. Implement active mutation admission and
historical reads before enabling eviction. An older binary understands the task
record format but restores the old memory behavior; SQLite rollback must also
account for its migration-version checks. This remains a single session-manager
writer design.

### Verification and delivery

Implement and validate in this order:

1. Engine full-task and finished-task read contracts, bounded filesystem gate
   lifetimes, and predefined `none` behavior; test filesystem and SQLite
   independently. Pass the active Task snapshot through the shared engine
   state/result API, keeping every engine's stored records authoritative,
   including the complete task history retained by NoneEngine.
2. Require active cache membership for live mutations and introduce shared
   state counts. Keep Session-locked membership/version checks at publication.
   Update RPC conversion, scheduler snapshots, and recovery together. Have every
   engine close return final counters for uniform cached-session publication.
3. Async point reads and controller late-message handling, then complete
   frontend listing and all watch refresh paths. Enable eviction with these
   paths ready.
4. SDK/CLI integration and memory/latency comparison against the baseline.

Required acceptance cases:

- Every terminal state leaves scheduling cache/indexes, retains its result in
  its engine, and increments its counter once. Active tasks remain cached.
- Sequential late updates, duplicate completions, conflicting results, and
  executor-cleanup retries that start after eviction skip mutation without
  engine writes, new events, or counter changes. A delayed publication cannot
  reinsert an absent task or increment its counters.
- Historical reads never authorize mutation or restore an active cache entry.
  Verify missing active entries skip updates for persistent engines and `none`.
- Engine write failure leaves cached task/state counts unchanged. Persistent
  `NotFound` propagates. The same state/result engine API handles `none` using
  its stored authoritative Task after active-cache admission.
- All engine mutations use the stored state and version even when the
  supplied active snapshot is older. Storage has no configured-backend branches
  for state/result mutations. Initial admission and engine lifecycle validation
  reject Closed-session updates; publication after close skips missing tasks
  through the membership/version guard.
- Late get/watch, duplicate registration, notification lag, and notification
  races return terminal output and failure details after eviction.
- Lists return all active cached tasks plus all finished storage tasks without
  a limit, active records first. A completion racing the history read does not
  duplicate an active snapshot ID. Finished reads do not reload active records
  from storage or repopulate the cache.
- Persistent-engine restart loads all stored tasks and payloads, reconstructs
  counts, and retains only active records in the scheduling cache. Closed Running history is preserved; Open Running
  recovery changes its state/counts once. Read, mutation, and corruption errors
  cannot publish partial recovery.
- Persistent close uses authoritative engine counts, including a completion
  committed before close but not yet published in cache. Repeated closes return
  the same final counts without a Storage history query.
- NoneEngine retains all task records and payloads. Creation, state/result
  mutation, and retry use authoritative stored versions/states and maintain
  accurate counters. Close rejects Running work, changes actual stored Pending
  records to Cancelled, and preserves results/counts across repeated closes.
- Storage applies returned close status, counters, version, and timestamp
  uniformly for every backend and clears task maps.
- Close cancels pending tasks once and preserves previous terminal counts;
  delete/retention races return results or errors without recreating records.
  A dequeued Pending task remains visible in `none` listings.
- `none` get/watch/list return finished results after scheduling-cache eviction.
  Its engine session metadata and task history survive closed-session cache
  eviction until explicit session deletion. A new engine instance is empty;
  no disk durability is introduced.
- Repeated full metadata scans and late reads do not accumulate filesystem task
  gates; waiter/lookup races never create two live gates for one ID.
- Session RPC counts and scheduler snapshot counts agree before/after eviction,
  pending-task dequeue, recovery, suspend/resume, and close.

For the performance check, hold active concurrency and payload sizes fixed while
increasing completed history in one open session. Compare cache cardinality,
heap allocation/RSS after settling, restart payload bytes read, startup time,
completion throughput, and historical get/watch/list latency. Prove the cache
contains zero terminal tasks; RSS alone is insufficient because allocators may
retain freed pages. Report startup and list peak allocations separately from
retained cache memory: full history and payloads are loaded temporarily. This
document makes no unmeasured speedup claim.

## 4. Use Cases

**Long-lived serving session:** tasks complete while the session stays open.
The scheduling cache retains only outstanding work; a later result read obtains
the original output from the engine without growing that cache. With `none`,
finished records remain in backend memory for fast reads.

**Late watcher after reconnect:** the task completes before the client registers.
The watcher subscribes, misses the cache, receives the stored terminal result
and available events, and removes its registration normally.

**Restart after a benchmark burst:** persistent engines load complete stored
task history and payloads to reconstruct counts, retry Open Running tasks under
the current policy, then retain only active scheduling tasks. A new NoneEngine
instance starts empty; retaining history in RAM adds no restart durability.

**Late completion after eviction:** a task has completed and left the active
cache, but the executor sends another completion. The controller skips the task
update and validates ownership before remaining executor cleanup. The stored
result and session counters remain unchanged; no history read grants permission
to mutate the task.

## 5. References

- [Issue #659](https://github.com/xflops/flame/issues/659).
- [RFE546: Session Task Watch](../RFE546-session-task-watch/FS.md).
- [RFE209: Filesystem Storage](../RFE209-filesystem-storage/FS.md).
- [RFE394: None Storage](../RFE394-none-storage/FS.md).
- [Storage orchestration](../../../common/src/storage/mod.rs), especially
  `load_data`, task reads/writes, and `close_session`.
- [Engine interface](../../../common/src/storage/engine/mod.rs),
  [filesystem](../../../common/src/storage/engine/filesystem/mod.rs),
  [SQLite](../../../common/src/storage/engine/sqlite.rs), and
  [none](../../../common/src/storage/engine/none.rs).
- [Session cache updates](../../../common/src/apis/session.rs),
  [API types](../../../common/src/apis/types.rs), and
  [RPC conversion](../../../common/src/apis/to_rpc.rs).
- [Controller](../../../session_manager/src/controller/mod.rs),
  [scheduler snapshots](../../../session_manager/src/controller/snapshot.rs),
  [frontend/watch handlers](../../../session_manager/src/apiserver/frontend.rs),
  and [frontend RPC schema](../../../rpc/protos/frontend.proto).

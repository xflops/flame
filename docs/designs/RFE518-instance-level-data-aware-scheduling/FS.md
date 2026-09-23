# RFE518: Instance-Level Data-Aware Executor Selection

## 1. Motivation

RFE275 attaches affinity keys to tasks and lets a service publish the data held
by its executor. Its first implementation exposes publication through
`SessionContext` and chooses a task by scanning the Pending queue for the
requesting executor.

That model makes data look session-scoped even when the cache belongs to the
long-lived service instance. It also lets locality reorder the Pending queue.
RFE518 changes the ownership boundary and the scheduling decision:

- publication belongs to a service instance, not a session;
- an executor, its instance, and its attributes may survive leaving one session;
- the scheduler periodically binds the best matching Idle instance to a
  session through a DAS plugin; and
- task delivery remains FIFO and always pops the first Pending task.

### Goals

- Expose instance-level `self.publish` APIs on `FlameService` and
  `FlameInstance`.
- Retain published attributes while the executor and its service instance
  survive session leave and rebind.
- Add a `das` scheduler plugin that ranks Idle executors for a session using the
  union of all its Pending-task affinity keys.
- Restore `Session::pop_pending_task()` to its pre-RFE275 FIFO behavior.
- Keep DAS soft: no match still permits normal execution.

### Non-goals

- Reordering Pending tasks for locality.
- Routing a task among executors already Bound to the same session.
- Hard affinity, reservations, global matching, preemption, or cache migration.
- Persisting runtime attributes across executor, Executor Manager, or Session
  Manager loss.
- Changing task retry, task ownership, completion, or failover semantics.

### Dependency

This design revises the publication, lifecycle, and routing portions of RFE275.
It retains RFE275's task-affinity wire fields, validation limits, opaque-key
contract, and the completion-ordering fix from #517.

## 2. Function Specification

### Instance-level publication API

Python service implementations add locality keys through their instance:

```python
self.publish({prefix_key, block_key})
```

Rust `FlameService` implementations use the same form. High-level Rust handlers
publish through their `FlameInstance` argument:

```rust
instance.publish([prefix_key, block_key])?;
```

`SessionContext.publish` and the top-level package publication functions are
removed. Invocation-scoped helpers live in `flamepy.app`.

App payloads are plain functions or classes. For a class service, its
constructor arguments travel with the service context and flmrun constructs the
object in the executor. flmrun binds the active session context and an attribute
accumulator while it invokes user code.
User code accesses that invocation-scoped state through the public
`flamepy.app` module helpers rather than reserved object fields.

`flamepy.app.service()` exposes a function through a client-side
`ServiceInstance`. For a class, it creates a service factory whose calls return
`ServiceInstance` handles and manage their sessions.

```python
import flamepy.app as app


class Cache:
    def store(self, block_key):
        session_id = app.session_context().session_id
        app.publish_attributes({block_key})
```

`flamepy.app.session_context()` and `flamepy.app.publish_attributes(attrs)` are
valid during a task invocation. Publication adds opaque `bytes` keys to the
current response. Repeated calls in that round accumulate. flmrun publishes and
drains the round after the callback, so each response carries only attributes
published in that round. Session Manager unions them into the executor's
retained set.

RFE518 changes RFE275's replacement behavior to cumulative executor attributes
and aggregates all calls made before each publication is transported:

- repeated calls from one task invocation are unioned;
- duplicate keys remain single set entries;
- each task response transports the union and clears the local accumulator;
- Session Manager extends the executor's retained set with that union;
- an empty union is a no-op;
- keys are opaque, nonempty byte strings;
- each key is at most 256 bytes; and
- one publication round contains at most 1,024 distinct keys and 64 KiB after
  deduplication.

A round starts after the previous `take` and ends at the next `take`. Attributes
from completed rounds are retained only in Session Manager's executor set; they
are not retained in the publisher or counted toward later publication limits.

For example, `self.publish({a})` followed by `self.publish({b, c})` causes the
enclosing session-enter or task response to carry `{a, b, c}`.

Each `FlameService` owns one `Publisher` object. It keeps the accumulated
attribute set and its byte count in separate fields. Rust protects the set with
the publisher mutex and shares the byte count through an atomic; the set mutex
serializes their updates.
`publish` atomically unions keys into that set. `Publisher.take` atomically
returns the complete set, replaces it with an empty set, and resets the byte
count; no separate pending flag is needed and no response can observe a partial
union. A failed session enter and session leave do not call `take`, so their
undelivered publication remains for the next consuming response. The publisher
has the same lifetime as the instance. The publisher is runtime-owned
infrastructure, not application configuration.

Python stores the publisher directly on `FlameService`. Rust `Publisher` hides
its shared pointer and lock internally: the service owns the object, while a
high-level `FlameInstance` handle receives a clone sharing the same attributes.
No task-local, process-global publisher, or registration layer is needed.

### Publication transport

RFE518 reuses the RFE275 shim and backend attribute fields. It adds no Publish
RPC.

- Successful `OnSessionEnterResponse` and every `OnTaskInvokeResponse` call
  `Publisher.take` and include the returned set. Session Manager unions it into
  the executor's existing set, and an empty set is a no-op. A failed
  session-enter response is not accepted by Executor Manager, so it neither
  includes nor consumes the publisher; the next consuming shim response can
  deliver its attributes.
- `on_session_leave` neither clears the publisher nor consumes its pending set.
- The shim calls `take` only after the wrapped callback completes, so the
  response observes the complete union from that callback.

Publication is still piggybacked. While an instance is Idle, Session Manager
continues using its accumulated attributes. Only attributes collected since the
previous `take` are sent with a response; previously accepted attributes remain
in Session Manager until executor removal.

### Instance attribute ownership

Published attributes belong to the service instance, and the executor and
instance share one lifetime. Moving the executor from Bound to Idle does not
destroy the instance or clear its publication state. Session Manager likewise
retains the instance's accumulated attributes when the executor leaves a
session.

Idle reuse is bounded by time. Actions run in Dispatch → Allocate → Shuffle
order, so Dispatch first offers an Idle retained instance to compatible pending
sessions. Shuffle keeps a newly Idle instance for twice its application's
configured `delay_release` (120 seconds by default), then releases it so its
resources can be reallocated. A continuously Idle instance is retained for at
most the doubled delay plus one scheduler interval, preventing unused retained
instances from starving other applications indefinitely. When an application
is unregistered, Controller attempts to release that application's Idle
instances immediately. Shuffle releases an owned instance once it becomes Idle
if unbind completion raced application cleanup. An immediate-release failure
is logged; RFE518 does not add release retry state. Active reuse starts a new
lifecycle interval.

When DRF is enabled, retained Idle and Releasing executors remain charged to
their node's allocated capacity during that interval, but Idle executors have
no session owner and do not contribute to any session's dominant share.

Session Manager assigns the session application when it creates an executor.
Every Idle executor is eligible only for sessions of that application. Executor
ownership is persisted by Session Manager, and Executor Manager carries the
same application ID in executor registration and node reconnection. This is
scheduler eligibility only: RFE518 adds no Executor Manager mismatch check,
bind result, or lifecycle path.

When the instance exits, the executor is removed and its volatile attributes
disappear with it through Flame's existing executor lifecycle. RFE518 changes
when an Idle executor is selected for release, as described above, but does not
change the release protocol or executor loss, executor unregister, task retry,
failover, or restart behavior. It adds no instance identity or recovery
protocol. After Session Manager loss, attributes are empty until they are
published again.

### FIFO task selection

`Session::pop_pending_task` restores the pre-RFE275 minimum-ID selection and
keeps the no-attribute API:

```rust
pub fn pop_pending_task(&mut self) -> Option<TaskPtr> {
    let pending_tasks = self.tasks_index.get_mut(&TaskState::Pending)?;
    let task_id = *pending_tasks.keys().next()?;
    pending_tasks.remove(&task_id)
}
```

Task IDs are monotonic within a session, so this removes the lowest Pending task
ID. `pop_pending_task` accepts no executor attributes and performs no affinity
scan or affinity bookkeeping. Whichever Bound executor asks for work first
receives the same FIFO head.

### Session scheduling affinity

For scheduler selection, each scheduling snapshot copies lightweight records
for the tasks in every session's non-terminal task-index buckets:

```rust
pub struct SnapShot {
    pub sessions: MutexPtr<HashMap<SessionID, SessionInfoPtr>>,
    // existing fields
}

pub struct SessionInfo {
    // existing fields
    pub task_index: HashMap<TaskState, BTreeMap<TaskID, TaskInfoPtr>>,
}

pub struct TaskInfo {
    // existing fields
    pub affinity: HashSet<Bytes>,
}
```

The snapshot omits terminal-task buckets and uses `TaskInfo` so it does not copy
task input, output, events, or other runtime data. During `DasPlugin::setup`,
once per scheduling cycle, DAS reads each session's Pending bucket and rebuilds
a deduplicated union:

```rust
pub struct DasPlugin {
    affinity: HashMap<SessionID, HashSet<Bytes>>,
}
```

Every Pending task contributes, while Running and terminal tasks do not. Task
affinity is immutable after creation, and duplicate keys across tasks collapse
in the plugin-owned `HashSet`. The union represents the full queued workload
even though tasks themselves still run in FIFO order. `setup` clears its prior
map before rebuilding, so completed or popped demand cannot survive into the
next scheduling cycle.

For an Idle executor `E` and session `S`:

```text
score(E, S) = count(key in E.attributes where DAS.affinity[S.id].contains(key))
```

Every unique affinity key participates. A full match scores the size of the
session affinity and beats any partial match; when no full match exists,
the largest partial match wins. Extra executor attributes do not reduce the
score. A key requested by multiple Pending tasks appears once; RFE518 measures
coverage, not request frequency.

Existing scheduler checks determine which Idle executors are eligible for the
session. A new executor has an empty attribute set. Equal scores, including
all-zero and empty affinity, fall back to executor ID for deterministic
behavior.

### DAS scheduler plugin

The scheduler adds the configurable `das` plugin to the existing plugin
registry immediately after `priority` and `drf`. It is added to each existing
default policy list. Code, generated configuration, and local-development
defaults use:

```yaml
cluster:
  policies:
    - priority
    - drf
    - das
```

The Helm chart preserves its existing `drf` policy and appends `das`; RFE518
does not enable another scheduler policy as a side effect.

The plugin ranks, but does not filter, executors. `DispatchAction` selects a
ready session and passes it with the `ExecutorState::Idle` executor map to
`Context::select_executor(session, idle_executors)`. After a successful bind,
Dispatch requeues a session that still has unsatisfied demand so retained Idle
executors satisfy that demand before Allocate creates new ones. The scheduling
context owns the plugin manager, applies the existing executor ownership,
resource, shim, and policy availability checks, orders eligible candidates,
and returns the executor with the greatest DAS score.

The plugin interface gains a session-aware executor comparator:

```rust
fn executor_order_fn(
    &self,
    session: &SessionInfo,
    e1: &ExecutorInfo,
    e2: &ExecutorInfo,
) -> Option<Ordering>;
```

`PluginManager` applies the existing first-non-equal composition rule.
`DasPlugin` orders larger scores first and, for equal scores, lexicographically
smaller executor IDs first. `Context::select_executor` collects eligible
executors into an array, sorts greatest-first with the plugin-manager
comparator, and takes the first entry. The stable sort preserves the first
eligible candidate when every plugin returns equal. Removing `das` from
`cluster.policies` therefore restores the prior eligible-executor choice; it
does not affect FIFO task delivery. Composition follows the fixed
`PLUGIN_REGISTRY` order—`priority`, `drf`, `das`, then the always-on `shim`
plugin—rather than the order written in YAML.

The scheduler snapshot and plugin add only the data needed by this comparator:

- the Pending bucket of each `SessionInfo.task_index` for actual Pending-index
  members;
- `DasPlugin.affinity: HashMap<SessionID, HashSet<Bytes>>`; and
- `ExecutorInfo.attributes: HashSet<Bytes>`.

Pending membership is fixed by the scheduling-cycle snapshot, and task affinity
does not change after creation. Dispatch still
revalidates ordinary executor/session eligibility before binding. Affinity may
be stale when the bind commits; because it is a preference, the next FIFO task
still runs normally on a cache miss.

### Bound executors

The DAS plugin chooses among actual `ExecutorState::Idle` executors before they
bind to a session. It does not route work among executors already Bound to that
session. A Bound executor retains its accumulated attributes, calls
`pop_pending_task()`, and receives the FIFO head.

Consequently, when a session already has a Bound waiter, that executor may take
the head before a better Idle instance is bound by the next scheduler cycle.
This is deliberate: RFE518 preserves immediate work conservation and does not
delay a task for locality.

## 3. Implementation Detail

### SDK and shim service

Python adds `FlameService.publish`; `FlameInstance` inherits it. Rust adds a
default `FlameService.publish` method and `FlameInstance.publish`. Macro-generated
Rust services own a `Publisher` automatically. Direct Rust `FlameService`
implementations provide their owned publisher through the trait accessor.
This accessor is a source-level migration requirement for direct trait
implementations. A manually constructed Rust `FlameInstance` is detached from
the service runtime, so `publish` returns an error; macro-provided handles share
the service publisher and transport their attributes normally.

flmrun binds the active session context and attribute accumulator around each
user invocation. The public `flamepy.app.session_context()` and
`flamepy.app.publish_attributes(attrs)` helpers access those capabilities
without attaching runtime fields to the execution object. After invoke, flmrun
stages the accumulated attributes in invocation-local context, and the shim
servicer consumes that set while assembling the same RPC response. App
invocations therefore do not share the `FlameService` publisher, so concurrent
responses cannot consume each other's attributes. flmrun constructs
class execution objects remotely from the arguments stored in the service
context. Each constructed class handle has a generated service ID. flmrun
retains its object in a process-local map keyed by that ID and rebinds the same
object when the executor enters another session for that service. Distinct
handles have distinct service IDs and therefore never share a constructed
object, even when they use the same class and constructor arguments. App
declarations accept functions and classes rather than already constructed
objects; functions remain session-scoped and may use imported module-level
state. A class App workload may therefore
publish keys for data held directly by its retained execution object. The cache
is volatile and is discarded when the executor process exits; its contents are
not serialized to the object cache.
`FlameInstanceServicer` normally takes attributes from its service publisher
after callbacks; flmrun overrides that hook to consume the staged invocation
set. Rust `ShimService` takes attributes from its service publisher.
`SessionContext` carries only session information.

The protobuf wrappers introduced by RFE275 remain unchanged. Executor Manager
continues forwarding snapshots through `BindExecutorCompletedRequest` and
`CompleteTaskRequest`.

### Executor and controller state

Executor Manager retains `shim_instance` after successful session leave instead
of dropping it when the executor becomes Idle. It does not independently check
application identity or add an application-mismatch bind result.

Session Manager keeps the creation-time application association and last
accepted attributes on its existing in-memory executor object across Bound →
Unbinding → Idle:

```rust
pub struct Executor {
    // existing fields
    pub application: String,
    pub attributes: HashSet<Bytes>,
    pub latest_updated_timestamp: DateTime<Utc>,
}
```

The application is persisted in the executor DAO, filesystem metadata, and
SQLite schema, and is also carried in `ExecutorSpec` for Executor Manager
registration and node reconnection. Attributes remain memory-only and reload as
an empty set. `latest_updated_timestamp` is also memory-only: it is initialized
to the current time on creation or recovery and refreshed on every executor
state transition. Unbind clears neither application nor attributes, while
removing the executor removes them naturally. No parallel controller map or
lifecycle lock is added.

`Session::update_task` maintains only the existing task map and state indexes.
Session loading restores tasks and their Pending-index membership as before.
There is no derived session affinity to update, persist, or recover. No task
payload or attribute value is logged.

### Scheduler and queue

`DasPlugin` is registered beside the existing scheduler plugins. Dispatch asks
the scheduling context to select from the Idle executors; the context uses its
owned plugin manager for availability and deterministic ordering. Allocation
is unchanged. Shuffle releases an Idle executor only when
`now - latest_updated_timestamp >= 2 × application.delay_release`; a missing
application releases an executor with a known owner immediately because reuse
is impossible. A configured non-positive delay releases immediately. A negative
age caused by wall-clock rollback also releases immediately rather than
extending the retention bound.

The executor-attribute argument and locality scan are removed from
`Session::pop_pending_task()`. The original no-argument FIFO implementation and
focused minimum-task-ID tests are restored. No event-driven router, waiter
registry, assignment map, task attempt, or new task-delivery protocol is added.

### Complexity

For per-task key limit `K`, cumulative executor attribute count `A`, `Q` non-terminal
tasks, `P` Pending tasks, and `I` available Idle executors, task state updates
perform no affinity work. A scheduling snapshot builds `Q` lightweight
`TaskInfo` records and copies their affinity sets in `O(Q × K)` while ignoring
finished tasks. `DasPlugin::setup` rebuilds the union in `O(P × K)` once per
cycle. Sorting candidates is `O(I log I × A)`: comparisons score executors by
iterating their bounded attribute sets and testing membership in the plugin
`HashSet`. Task pop remains the Pending `BTreeMap` operation.

No individual task/executor Cartesian match is constructed. Each task and
publication round is bounded to 1,024 keys and 64 KiB. An executor's cumulative
set can grow until that executor is removed. The per-cycle session union can
likewise grow with the Pending queue, but each unique key has one set entry and
the union is not rebuilt once per executor. RFE518 adds no persistent
task/executor routing index.

The implementation is guarded by tests for per-cycle union rebuilding, shared
keys, FIFO removal, snapshot membership, and sorted executor selection.
End-to-end scheduler latency and allocator memory depend on
deployment sizing and are measured by the existing benchmark/operations
workflow; this RFE does not introduce a new release-blocking microbenchmark or
an environment-independent p99 threshold.

## 4. Verification

### Publication and lifecycle

- Python and Rust `self.publish` union repeated calls made before the next
  session-enter or task response; Session Manager extends the executor's set
  with every response.
- `FlameService` owns the publisher; Rust `FlameInstance` handles share its
  internally synchronized attributes.
- App module helpers expose the active session context and mutable publication
  API without attaching reserved runtime fields to execution objects. After
  invocation, flmrun stages the accumulated set for the shim servicer to consume
  in that RPC response, including failed task invocation.
- The retained class-object cache excludes the invocation context and its
  response-local publication accumulator.
- A response with no publication produces a present empty set and leaves the
  backend unchanged.
- Each publication round enforces the same deduplicated 1,024-key and 64-KiB
  limits in Python and Rust; `take` resets both counters for the next round.
- `take` runs after the wrapped callback and transports its complete union.
- Session leave retains the executor's shim instance and attributes.
- flmrun retains a constructed class object by service ID across bindings;
  separately constructed handles remain isolated even when they use the same
  class and constructor arguments.
- Dispatch gets the first opportunity to reuse an Idle instance; Shuffle keeps
  it for twice `application.delay_release` before releasing it.
- Application cleanup attempts to release each currently Idle executor, with
  Shuffle as the fallback when unbind completion races cleanup.
- Executor lifecycle transitions refresh the in-memory
  `latest_updated_timestamp`; recovery initializes it to the current time.
- Executor creation persists the session application; unbind and Session
  Manager recovery retain it, and selection requires an exact application
  match.
- Executor removal clears its attributes; unbind alone does not.
- Session Manager restart restores the application association and reloads
  empty attributes; Executor Manager reconnection carries the same application.
- `flmctl list --executor` shows the persisted application owner in its `App`
  column.
- Concurrent service instances publish only to their own publisher.

### FIFO and DAS

- `pop_pending_task()` always removes the smallest Pending task ID regardless of
  task affinity or executor attributes.
- `DasPlugin::setup` rebuilds a `HashSet<Bytes>` union from every task in the
  snapshot's Pending index and excludes Running and terminal tasks.
- Pending affinities `{a,b}` and `{b,c}` produce session affinity `{a,b,c}`;
  executors `{a,b,c}`, `{a,b}`, `{c}`, and `{}` score `3`, `2`, `1`, and `0`.
- Repeated use of the same key by multiple Pending tasks is deduplicated and
  does not increase its score.
- Re-running plugin setup replaces the previous cycle's union and cannot retain
  keys that are no longer requested by a Pending task.
- Popping a task removes it from the next snapshot's Pending membership even
  before its stored task state changes to Running.
- A best partial match wins when no full match exists; extra executor keys do not
  reduce its score.
- Empty/all-zero affinity falls back to executor-ID order and still binds.
- Existing resource, shim, readiness, and plugin filters run before DAS ordering.
- Disabling `das` restores prior executor choice without changing FIFO task pop.
- Omitted policies enable `priority`, `drf`, and `das`; the chart preserves
  `drf` and adds `das`; an explicit legacy policy list disables DAS; unknown
  names fail.
- A Bound executor immediately gets the FIFO head even when a better Idle
  instance exists.
- Snapshot staleness can affect preference but never task eligibility or FIFO
  correctness.
- Complexity-focused tests cover per-cycle aggregation and single-pass DAS;
  deployment-scale latency and memory remain part of the existing operational
  benchmark workflow.

Run SDK unit suites, Executor Manager lifecycle tests, Session Manager plugin
and queue tests, the full scheduler regression suite, and the App DAS E2E
scenario covering process-stable attributes across session leave, execution
object rebinding, retention across multiple scheduler cycles, and Idle
executor selection.

## 5. Rollout and Rollback

Flame upgrades are performed with no running workloads. Stop workload
submission and drain/remove every executor before upgrading the Session Manager,
Executor Managers, and SDKs. Resume workloads only after all components run the
new version. Because no retained executor exists during the version-skew window,
manager rollout order cannot expose an instance to cross-application reuse.

For rollback, stop workload submission, drain/remove all retained executors,
disable `das`, and then roll back Session Manager, Executor Managers, and SDKs.
Volatile attributes may be dropped safely; task affinity remains an additive
persisted field.

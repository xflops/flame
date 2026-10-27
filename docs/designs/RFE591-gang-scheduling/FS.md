# Design: Gang Scheduling with `batch_size`

**Status:** Draft
**Created:** 2026-10-17
**Issue:** [#591](https://github.com/xflops/flame/issues/591)

---

## 1. Motivation

A session whose work needs several executors at once (`batch_size >= 2`): data-parallel
training over NCCL, tensor-parallel inference, MPI-style jobs. The scheduler gives such a
session executors one at a time. `DispatchAction` binds one Idle executor per step and
`AllocateAction` pipelines or allocates one executor per step; each step takes effect at once.

When the cluster can supply only part of the minimum, the session keeps what it got and waits:

- a group that blocks until all members are present (a collective `init_process_group`)
  occupies its executors without making progress;
- other sessions that could have used those executors are starved;
- two such sessions can each hold part of the cluster and neither starts.

## 2. Goals and non-goals

**Goals**

1. While a session has pending tasks, it is bound to executors only when it holds `batch_size`
   after the bind, decided in one scheduling step; if the Idle executors cannot cover the gang,
   nothing is bound.
2. The same flow serves every session: a `batch_size` of 1 (the default) needs
   no gang, so those sessions behave as before.
3. The change is confined to `DispatchAction`, the plugin hook that asks "is the minimum
   held" and a small `Statement` type. Allocate and Shuffle are unchanged.
4. A gang member can read the session id, the task id and `batch_size` at run time.
5. `min_instances` keeps its own meaning: long-lived instances that are warmed up and never
   trimmed. On a session that is not a gang it is not part of the all-or-nothing bind. A gang
   is the whole session: its `min_instances` is 0 or `batch_size` and its `max_instances` is
   unset or `batch_size`, and a session that sets anything else is rejected when it is created.
   A gang with `min_instances` equal to `batch_size` is kept warm, and is formed all at once like
   any other, even before its first task exists.

**Non-goals**

- Gang sizes other than `batch_size`.
- Preemption of other sessions to make room for a gang (Shuffle is unchanged).
- Node or topology placement rules.
- Gang-aware executor creation in Allocate (see 4.3).

## 3. Current behavior

`Context::actions` runs Dispatch, Allocate, then Shuffle each cycle on one snapshot.

| Action | Supply it uses | Step |
| ------ | -------------- | ---- |
| Dispatch | Idle executors with no session | `select_executor` + `bind_session` |
| Allocate | Void executors, then Unbinding executors | `pipeline_executor` |
| Allocate | Node capacity (`is_allocatable`) | `allocate_executor` (creates one) |

Dispatch loops over sessions ordered by the plugins, skips a session that is not underused or
is `is_ready`, binds one executor, and re-queues the session while it is still underused.
MinMax counts `session_executors` (bound plus pipelined plus created in this cycle) and
reports a session not ready while that count is below `min_instances` or, for a gang with
pending tasks, below `batch_size`.

`is_ready` means "the session was given enough capacity this cycle, stop adding": it also
counts demand and DRF fair share, so it can be false at the minimum and true below it. It
cannot tell whether the minimum is held.

The plugin hooks (`on_session_bind`, `on_executor_pipeline`, `on_executor_create`) mutate
plugin state. `on_session_unbind` is the exact inverse of `on_session_bind`; the other two have
no inverse, which is why only binds are staged.

## 4. Design

Dispatch stages the decisions in a `Statement`, then
commits them if the session satisfies its gang and discards them otherwise.

### 4.1 `Statement`

`scheduler/statement.rs`. A batch of Dispatch decisions that takes effect all at once.

| Method | Effect |
| ------ | ------ |
| `bind(exec, ssn)` | Stage: run `on_session_bind` and mark the executor Binding in the cycle's snapshot. The controller is untouched. |
| `commit()` | Call `Controller::bind_session` for each staged bind, in order. |
| `discard()` | Undo, newest first: `on_session_unbind` and the executor back to Idle in the snapshot. Returns the executors. |

Staging updates the plugins and the snapshot immediately, so later decisions in the cycle
(including other sessions) see them. Only `commit` makes a bind real.

If a controller call fails during `commit`, the binds already applied cannot be undone. The
statement keeps them, discards the rest from the plugins and the snapshot, and returns the
error. The next cycle sees the remaining shortfall and completes the gang.

### 4.2 `is_satisfied`

A new plugin hook:

```
fn is_satisfied(&self, ssn) -> Option<bool>       // Plugin
PluginManager::is_satisfied -> bool               // first opinion wins; no opinion = true
```

MinMax answers `session_executors >= min(gang, session max_executors)`, where the gang is
`batch_size` while the session is a gang (`batch_size > 1`) with pending tasks and 0 otherwise.
It counts executors bound or staged in this cycle. The same gang size raises the session's
executor demand, so Allocate creates executors up to it. The cap keeps a gang larger than the session limit
from waiting forever.

It is not `is_ready`: see section 3.

### 4.3 Dispatch

For each session popped from the queue that is underused and not `is_ready`:

1. Open a `Statement`.
2. While there is an eligible Idle executor (`select_executor`): stage a bind, remove the
   executor from the pool, and stop once `is_satisfied` holds.
3. If nothing was staged, or `is_satisfied` is false: discard and return the executors to the
   pool for the next session.
4. Otherwise commit, and re-queue the session while it is still underused and not ready, so
   executors beyond the minimum are bound one per turn as before.

A session that is not a gang (`batch_size` of 1, or no pending task) is satisfied by its first
bind, so it commits one bind per turn, as before.

**Allocate is unchanged.** It counts only executors that are bound or pipelined for the session,
and an executor it creates has no session (`ssn_id` is none) until Dispatch binds it. A gang
still assembles correctly: Allocate takes one step per session per cycle and pipelines a Void
executor before creating a new one, so while Dispatch keeps discarding, creation tracks the
shortfall (`test_gang_assembles_*` run full cycles and end with exactly the minimum). Two
costs remain:

- a gang of N takes about N cycles to assemble, as Allocate adds one executor per cycle;
- Allocate does not see the Idle executors Dispatch is waiting on, so a created executor that
  fails to register can lead to an extra one. Nothing runs partially, and extras are released
  after the application's `delay_release`.

Counting those Idle executors needs Allocate's pipeline and creation staged in the same
`Statement`; see open questions.

### 4.4 Context exported to the service

A gang member must know its session, its task and the size of the gang.

```python
ctx = flamepy.app.context()
ctx.session_id      # the session the invocation runs in
ctx.task_id         # the task being invoked
ctx.batch_size      # the session's gang size (1 if it is not a gang)
```

`context()` returns an `InstanceContext` and raises `RuntimeError` outside an invocation.
`batch_size` travels like the other session fields:

- `rpc/protos/shim.proto`: `uint32 batch_size = 5;` in `SessionContext` (copied to
  `sdk/rust/protos` and `sdk/python/protos`).
- `common::apis::SessionContext` carries it from the session spec to the shim request.
- `flamepy.core.service.SessionContext.batch_size`, filled by the aio servicer.
- `flamepy.app.runpy` binds the `TaskContext` with the session context for each invocation, so
  `context()` has the task id.
- The Rust SDK `SessionContext` carries `batch_size`; the WASM shim does not yet.

Notes:

- **Rank.** The rank of a task is `task_id % batch_size`. Task ids of a session are a
  sequence that starts at 1 and a retried task keeps its id. A gang submits `batch_size`
  tasks back to back, which get consecutive ids, and N consecutive ids cover every residue
  mod N exactly once, so the ranks are unique and complete (ids 1..N give ranks 1..N-1, 0).
  This holds when:
  1. the N tasks of the gang are submitted together, in one session, without other tasks
     interleaved by another submitter; and
  2. each of the N tasks blocks until all members have joined (a collective
     `init_process_group` or a barrier), so each runs on its own executor.

  The rank is a property of the task, not of the executor: a later call may run on a different
  executor. A service that needs a stable rank per executor reads it in the blocking call and
  keeps it. The rendezvous address is shared through the cache: rank 0 publishes its address
  under a key derived from the session id, the others wait for it.

### 4.5 `batch_size` is the gang size

The session `batch_size` field was reserved for batch scheduling, fixed to 1 and never read.
#593 first removed it and used `min_instances` as the gang size; that mixed two meanings, since
`min_instances` also keeps long-lived instances alive. The field is restored and is now the
only gang knob:

- `types.proto`: `batch_size = 7` in `SessionSpec` again (a value of 0 is treated as 1, as an
  old client that does not send it).
- Rust API, storage engines and Python/Rust SDKs carry the field again, with default 1.
  `@app.service(batch_size=N)` sets it for a Python service.
- SQLite: a new migration adds `sessions.batch_size` back (the earlier migration that dropped
  it is unchanged). The filesystem engine reads sessions written without the field as 1.
- `min_instances` goes back to its meaning before #593: warm-up and the floor that Shuffle and
  the MinMax plugin keep. On a session that is not a gang it no longer takes part in
  `is_satisfied`. A gang's `min_instances` is 0 or `batch_size`; with `batch_size` the gang is
  kept warm, and MinMax's `is_satisfied` counts it as a gang whether or not a task is pending.

The reserved executor column `batch_index` is not part of this change.

### 4.6 What does not change

- Sessions with a `batch_size` of 1.
- Growth beyond the minimum for pending tasks: it happens one executor at a time as today.
- `is_underused`, `is_ready`, `is_overused`, and Shuffle.
- `min_instances` semantics: warm-up and the retained floor.

## 5. Edge cases

| Case | Behavior |
| ---- | -------- |
| A gang whose `min_instances` is not 0 or `batch_size`, or whose `max_instances` is not unset or `batch_size` | Rejected when the session is created, as an invalid argument. |
| `batch_size` above the application's `max_instances` | Rejected when the session is created. |
| Gang larger than a cluster-wide session limit | The gang size is capped at the limit (4.2). |
| Gang session with no pending task and `min_instances` 0 | Not a gang yet: it needs and holds only what its running tasks need. |
| Gang kept warm (`min_instances` = `batch_size`) with no pending task | Formed all at once like any gang, and held. |
| Gang larger than the cluster | Never satisfied; Dispatch discards each cycle and logs a debug line. |
| Executor of a gang session released or interrupted | Count drops below the minimum; Dispatch binds a replacement only when the whole shortfall can be bound. Running tasks are not stopped by the scheduler. |
| Two gangs competing for Idle executors | Sessions are processed in plugin order. The first stages and commits; the second finds too few Idle executors and discards. |
| Gang plus a non-gang session | The non-gang session can use executors a blocked gang skipped. A gang that never fits can starve behind a stream of small sessions; see open questions. |
| `bind_session` fails during commit | Binds already applied stay; the rest are discarded and the next cycle retries (4.1). |
| Session closed | Demand is zero; the existing release paths run. |

## 6. Alternatives considered

- **Count Idle executors up front and bind the shortfall.** Works for Dispatch alone but needs
  a shortfall count from the plugin and a separate path for gangs. The `Statement` uses one
  flow for every session and the plugin only answers "is the minimum held".
- **Opt-in with a session attribute.** Safer for existing users, but the meaning of
  `min_instances` already is "at least this many". Resolved by a separate `batch_size`.

## 7. Test plan

Unit tests (`session_manager/src/scheduler`):

- Dispatch: 2 Idle and min 2 binds both; 1 Idle and min 2 binds none; 3 Idle and min 2 binds 2
  when there are no tasks; min 1 binds one, as before.
- Dispatch: two gangs, supply for one: the first binds, the second binds none.
- `Statement`: `discard` restores the plugin counters and the snapshot and never touches the
  controller; `is_satisfied` flips when the second bind is staged and back on discard.
- Allocate: unchanged behavior, covered by the existing tests.

SDK (`sdk/python/tests`, `sdk/rust`): `batch_size` reaches `SessionContext`;
`app.context()` returns the session id, task id and `batch_size` inside an invocation and
raises outside one.

End-to-end:

- `test_app.py`: `test_app_instance_context` checks `session_id`, `task_id` and
  `batch_size`; `test_app_gang_members_run_concurrently` runs two long tasks of a
  `batch_size=2` service and checks unique ranks, distinct executors and overlapping start
  times.
- `test_session.py` (`TestGangScheduling`) sizes the cluster from `list_nodes` by picking a cpu
  request that fits 4 to 6 executors, then checks, using `list_executors`:
  - a minimum above the capacity binds nothing and its task stays pending;
  - two gangs that fit together both get all their executors, and a task runs;
  - two gangs that each fit but not together: the executor count of every gang is always 0 or
    its minimum, exactly one gang runs, and the other starts once the first is closed;
  - a gang that cannot start does not hold back a session that is not a gang.

## 8. Rollout

Behavior changes only for sessions with `batch_size >= 2`. Document it in the user guide next
to `batch_size`, together with `app.context()`.

## 9. Open questions

1. Resolved: a session opts in with `batch_size >= 2`; `min_instances` never makes a gang on its own, but a gang's `min_instances` may be its size, to keep it warm.
2. Report a gang that cannot start (a session event or condition) so users see why it is pending.
3. Stage Allocate's pipeline and creation in the `Statement` too, so a gang assembles in one
   cycle and Idle executors it waits on are counted (4.3)? It needs inverse plugin hooks for pipeline and create and deferred executor creation.
4. A gang that never fits can starve behind small sessions. Reserve capacity for it (backfill
   rules), or leave this to priority and Shuffle?
5. The WASM shim's `SessionContext` does not carry `batch_size` yet.

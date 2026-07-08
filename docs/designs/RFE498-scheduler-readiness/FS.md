# Design: Scheduler `is_ready` / `is_fulfilled` Semantics

**Status:** Draft

**Author:** Klaus Ma

**Created:** 2026-07-08

**Issues:** [#498](https://github.com/xflops/flame/issues/498), [#500](https://github.com/xflops/flame/pull/500), [#508](https://github.com/xflops/flame/pull/508)

---

## 1. Motivation

Flame's scheduler runs **Allocate → Dispatch → Shuffle** once per cycle. The existing APIs answer two action-specific questions:

| API | Existing meaning retained by this design |
|-----|------------------------------------------|
| `is_fulfilled(session)` | Allocate-side fulfillment: enough executor supply exists for the session's incomplete tasks, including associated executors and reusable Void/Idle executors selected by the current Statement. |
| `is_ready(session)` | Dispatch-side readiness: enough executors are ready, bound, or committed in flight for the session's tasks. |

[Issue #498](https://github.com/xflops/flame/issues/498) exposed duplicate binding while an executor remained in `Binding`. [PR #500](https://github.com/xflops/flame/pull/500) addressed that and two related regressions, but added separate non-gang counters and mixed rate limiting into `PluginManager`. [PR #508](https://github.com/xflops/flame/pull/508) reverted #500 so the behavior could be redesigned.

This design targets the tree after merged #508. It keeps `is_ready` / `is_fulfilled`, the existing plugin callbacks, `Statement`, and the existing Gang state fields. It introduces **no new scheduler function and no new scheduler state field**.

### Problems to solve

| Problem | Symptom | Root cause |
|---------|---------|------------|
| Gang under-provision | Only one executor is created for multiple incomplete tasks | Modulo readiness becomes true at every batch multiple instead of at the session's demand target. |
| Duplicate dispatch/binding | Extra executors are selected while one is already in `Binding` | Dispatch has no ready pre-check, and old readiness requires a new bind in the current cycle. |
| Duplicate allocation | An additional batch can be created after demand is already met | Old fulfillment requires a new pipeline/allocation operation before it can be true. |
| Cap overshoot | One gang allocation pass can cross `max_instances` | The action checks the cap only before entering the allocation loop. |

### Objectives

1. Fix #498 and the gang demand regressions described by #500 while treating an all-abstaining policy set as already satisfied.
2. Preserve `Context::is_ready` for Dispatch and `Context::is_fulfilled` for Allocate.
3. Keep `Option<bool>` inside plugins, while PluginManager and Context return a concrete `bool` after ignoring abstaining plugins.
4. Reuse only `GangState.batch_size`, `allocated`, `pipelined`, and `bound`; derive task demand and caps from the supplied `SessionInfo`.
5. Restore #500's valid non-scheduler changes after #508: default `priority + gang`, DRF opt-in, harmless explicit `shim`, and the process-wide `SHIMS_TEST_LOCK`.

---

## 2. Function Specification

### Configuration

No new configuration is introduced.

| Field | Role |
|-------|------|
| `cluster.policies` | Loading `gang` enables absolute batch readiness; if every configured plugin abstains, the action is already satisfied. |
| `session.batch_size` | Gang batch width, normalized with `max(1)`. |
| `session.max_instances` | Hard upper bound for gang allocation demand. |

On the #508 baseline, restore `priority + gang` as the default policy list. DRF remains opt-in. Shim remains always enabled; explicitly listing `shim` is accepted and ignored, while unknown policies remain errors.

### Existing API contract

No new method is added. The existing methods retain their names and delegate to `PluginManager`:

```rust
impl Context {
    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError>;

    pub fn is_fulfilled(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError>;
}
```

The existing `Plugin::is_ready` / `Plugin::is_fulfilled` methods remain optional. PluginManager ignores `None` and combines only concrete plugin opinions:

| Plugin opinions | Context result |
|-----------------|----------------|
| One or more concrete opinions | `true` only when every concrete opinion is `true`; `None` opinions are ignored. |
| Every plugin returns `None` | `true`; no configured plugin requires more work from the action. |

This keeps abstention inside the Plugin API while exposing a simple boolean contract to Context and actions.

### Action contract

**AllocateAction**

1. Before creating a `Statement`, call `is_fulfilled(session)` and skip when it returns true.
2. Consider existing Void and Idle executors before creating a new executor. Record selected reusable executors through the existing pipeline operation so Gang's existing `pipelined` field reflects them.
3. After each operation:
   - stop when `is_fulfilled` returns true;
   - continue when it returns false.
4. Commit a non-empty statement when fulfillment is true; discard a non-empty false statement as an incomplete gang batch.

**DispatchAction**

1. Runs after Allocate in the same `Context`.
2. Before creating a `Statement`, call `is_ready(session)` and skip when it returns true.
3. Bind available idle executors.
4. After each bind:
   - stop when `is_ready` returns true;
   - continue when it returns false.
5. Commit a non-empty statement when readiness is true; discard a non-empty false statement as an incomplete gang batch.

**ShuffleAction** runs last and keeps its existing behavior.

All three actions share one `Context` and PluginManager for the cycle. Allocate's speculative `pipelined` updates remain visible to later actions. Reusable Idle executors selected by Allocate remain eligible for Dispatch in the same cycle. Newly created executors are still Void and become dispatchable only after registration in a later cycle.

An Idle pipeline reservation updates Gang fulfillment only. Priority and DRF do not charge an Idle executor during Allocate because it already exists and consumes node resources; they assign its session share when Dispatch actually binds it. This prevents the reservation from making Dispatch incorrectly consider the session no longer underused.

### Scope

**In scope**

- Keep optional opinions inside plugins and return resolved booleans from PluginManager and Context
- Set the cycle order to Allocate → Dispatch → Shuffle
- Update Allocate and Dispatch to use the resolved boolean checks without a progress argument
- Correct Gang formulas using existing state fields plus `SessionInfo`
- Include reusable Void/Idle executors in Allocate's fulfillment calculation through existing Statement pipeline operations
- Add Dispatch's pre-session `is_ready` guard
- Restore the valid configuration and shared shim-test-lock changes removed by #508
- Add regression coverage for #498 and every scheduler configuration described by #500

**Out of scope**

- New demand trackers, counters, helper methods, or state fields
- Priority and DRF policy algorithms
- `SessionInfo::is_ready(retry_limits)`, which is a separate bind-failure recovery gate

---

## 3. Gang Semantics Using Existing Fields

`GangState` remains exactly:

```rust
struct GangState {
    batch_size: u32,
    allocated: u32,
    pipelined: u32,
    bound: u32,
}
```

`setup()` continues to initialize `batch_size`, count snapshot executors with the session ID into `allocated`, and reset `pipelined` / `bound` to zero. Existing Statement callbacks continue to increment and roll back `pipelined` and `bound`.

### Derived target

Both predicates derive the target directly from the supplied `SessionInfo`; no target is cached:

```text
incomplete_tasks = Pending + Running
batch_size       = max(session.batch_size, 1)
uncapped         = div_ceil(incomplete_tasks, batch_size) * batch_size
aligned_max      = floor(max_instances / batch_size) * batch_size, or the largest u32 batch multiple
needed           = min(uncapped, aligned_max)
```

Perform summation and rounding in `u64`, apply the cap, and convert the final value to `u32`. The API already rejects non-batch-aligned limits; defensive alignment keeps legacy persisted sessions within both the cap and the full-batch invariant.

### `is_fulfilled`

```text
total        = allocated + pipelined
is_fulfilled = needed == 0 || total >= needed
```

Allocate records reusable Void/Idle executors and new allocations through the existing pipeline/allocation callbacks, so they contribute through `pipelined`. This fixes gang under-provisioning, duplicate allocation, and `max_instances` overshoot without storing `incomplete_tasks`, `needed`, or `max_instances` in `GangState`.

### `is_ready`

```text
total    = allocated + bound
is_ready = needed == 0 || total >= needed
```

Dispatch uses this predicate before and during task/executor dispatch. An executor in `Binding` has a session ID and is included in `allocated`; it counts as committed in flight even though it is not yet runnable, preventing another dispatch/bind while `on_session_enter` is running. Idle executors selected by a speculative bind contribute through `bound`.

### Rollback

The existing callbacks remain the only speculative-state mechanism:

```text
allocate/pipeline → pipelined += 1
discard           → pipelined -= 1
bind              → bound += 1
unbind/discard    → bound -= 1
```

No state ownership or lifecycle is added.

---

## 4. Use Cases

### UC1: #498 executor remains in Binding

Gang enabled, `batch_size=1`, one incomplete task, and one snapshot executor in `Binding`:

```text
needed = 1
allocated = 1
pipelined = 0
bound = 0

is_fulfilled = 1 >= 1 → true
is_ready     = 1 >= 1 → true
```

Allocate runs first and skips because `is_fulfilled` is satisfied by the snapshot executor. Dispatch then skips because `is_ready` is satisfied. Repeating the cycle does not change executor or bind counts.

### UC2: Priority-only session

Priority does not own readiness or fulfillment, so it returns `None` for both. With no concrete plugin opinion, both Context methods return true and Allocate/Dispatch perform no operation. The default `priority + gang` configuration supplies Gang's concrete demand decision.

### UC3: Gang batch_size=1 with three tasks

```text
needed = div_ceil(3,1)*1 = 3
```

Allocate continues until `allocated + pipelined == 3` and `is_fulfilled` returns true, then commits.

### UC4: Gang batch_size=2 with insufficient resources

With `needed=2`, one speculative operation leaves `is_fulfilled` false. The non-empty incomplete Statement is discarded and the existing callback rolls back `pipelined`.

### UC5: Gang demand exceeds max_instances

For `batch_size=2`, eight incomplete tasks, and `max_instances=4`:

```text
uncapped = 8
aligned_max = 4
needed = 4
```

Allocate stops at four total associated/speculative executors. The fulfillment target itself prevents a fifth operation inside the loop.

### UC6: Dispatch with reusable Idle supply

With Gang enabled, an idle executor has no session ID. Allocate first includes it as reusable supply rather than creating another executor. Gang's fulfillment becomes true, and Dispatch then records one bind. Gang's readiness becomes true, so the Statement commits and the executor transitions from `Idle` to `Binding`.

---

## 5. Test Plan

### Gang unit tests

- Allocate-side `is_fulfilled` uses `allocated + pipelined` and reaches true at the executor-supply target
- Dispatch-side `is_ready` uses `allocated + bound` and reaches true at the dispatch target
- Existing snapshot executors can satisfy both predicates without a new operation
- Reusable Void and Idle executors selected by Allocate contribute through the existing `pipelined` field
- Reserving an Idle executor does not advance Priority/DRF session allocation until Dispatch binds it
- `batch_size=1` with three incomplete tasks requires three total executors
- Partial `batch_size=2` Statements remain unsatisfied and roll back
- Demand above an aligned `max_instances` stops exactly at the cap
- Large task counts use overflow-safe target arithmetic
- Existing allocate/pipeline/bind rollback tests remain valid with no new state fields

### PluginManager tests

- No plugin opinion returns true
- One `Some(false)` opinion returns false
- All concrete opinions true returns true
- Mixed true and false opinions return false

### Scheduler integration tests

- DRF-only and priority-only sessions perform no allocation when every plugin abstains
- Allocate does not create an executor when sufficient compatible Void/Idle supply already exists
- Gang `batch_size=1` allocates one executor per incomplete task
- Gang `batch_size=2` commits only complete batches
- Gang allocation never exceeds `max_instances`
- Two independent scheduler cycles with an executor held in `Binding` keep both executor count and bind-call count at one
- `Context.actions` executes Allocate, Dispatch, then Shuffle in that exact order
- Allocate, Dispatch, and Shuffle share the same PluginManager so Statement callbacks remain visible across actions

### Configuration and shim tests

- Default context and generated `flmadm` configuration use `priority + gang`
- Explicit DRF remains accepted
- Explicit `shim` is accepted as a no-op; unknown policies still fail
- Every shim test that mutates process-global environment or working directory uses the same `SHIMS_TEST_LOCK`

---

## 6. Migration / Rollout

1. Start from merged #508, which establishes the pre-#500 implementation baseline.
2. Restore the shared `SHIMS_TEST_LOCK`, `priority + gang` defaults, DRF opt-in configuration, and harmless explicit `shim` handling.
3. Keep optional opinions in the Plugin trait; resolve them to boolean in existing PluginManager and Context methods, returning true when every plugin abstains.
4. Set and test the action sequence as Allocate → Dispatch → Shuffle, with one shared Context and PluginManager.
5. Update Allocate to use boolean `is_fulfilled` and include Void/Idle supply.
6. Update Dispatch to use boolean `is_ready`.
7. Replace Gang's modulo checks with the derived, max-capped absolute target using only existing fields.
8. Close #498 only after the two-cycle Binding-stall integration test passes.

**Compatibility:** No scheduler method or state field is added. Context returns boolean readiness/fulfillment without a progress argument; an all-abstaining plugin set resolves to true. The action order changes from the post-#508 baseline's Dispatch → Allocate → Shuffle to Allocate → Dispatch → Shuffle. An executor created by Allocate is not eligible for Dispatch until it registers and appears as Idle in a later cycle. Explicit policy lists remain valid, but readiness-dependent work requires a policy such as Gang that supplies a concrete opinion. Clusters that relied on implicit DRF must add `drf` explicitly when the default returns to `priority + gang`.

**Rollback:** Revert this replacement on top of #508. This restores the pre-#500 optional plugin API and modulo behavior without a data migration.

---

## 7. Files and References

| File | Change |
|------|--------|
| `session_manager/src/scheduler/plugins/gang.rs` | Change formulas only; keep the existing struct fields and callbacks. |
| `session_manager/src/scheduler/plugins/mod.rs` | Ignore abstaining plugins and resolve the all-`None` case to true; add no counter or field. |
| `session_manager/src/scheduler/plugins/priority.rs`, `drf.rs` | Defer Idle-executor session accounting until Dispatch binds it. |
| `session_manager/src/scheduler/ctx.rs` | Return resolved booleans from the existing methods. |
| `session_manager/src/scheduler/actions/allocate.rs` | Use `is_fulfilled` and consume reusable Void/Idle supply first. |
| `session_manager/src/scheduler/actions/dispatch.rs` | Use `is_ready` and add the pre-check. |
| `common/src/ctx.rs`, `flmadm/src/managers/config.rs` | Restore `priority + gang` defaults and DRF opt-in. |
| `executor_manager/src/shims/` | Restore the process-wide test lock. |

**Related designs**

- [RFE400 Batch Session](../RFE400-batch-session/FS.md)
- [RFE413 Priority Scheduling](../RFE413-priority-scheduling/FS.md)
- [RFE408 Fairshare batch_size](../RFE408-fairshare-batch-size/FS.md)

**External**

- [PR #500](https://github.com/xflops/flame/pull/500) — prior fix being replaced
- [PR #508](https://github.com/xflops/flame/pull/508) — merged revert and implementation baseline

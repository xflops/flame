# Design: Enhance Fairshare for batch_size

**Status:** Draft  
**Author:** Klaus Ma  
**Created:** 2026-04-20  
**Issue:** [#408](https://github.com/xflops/flame/issues/408)

---

## 1. Motivation

### Background

The `batch_size` feature (RFE400) introduced gang scheduling, enabling coordinated multi-executor workloads such as multi-node LLM inference with tensor parallelism. The Gang plugin ensures executors are committed in complete batches; however, the Fairshare plugin's resource distribution algorithm does not fully account for batch boundaries.

### Current Limitations

1. **Non-batch-aligned `deserved` allocation**: Fairshare distributes `deserved` slots evenly across sessions without considering `batch_size`. For a session with `batch_size=4` and `slots=1`, Fairshare may allocate `deserved=6` slots, resulting in only one complete batch (4 executors) while 2 slots remain unusable.

2. **Inefficient `is_underused` check**: The current implementation (`allocated < deserved`) does not consider batch boundaries. A session with `allocated=4` and `deserved=6` appears "underused" by 2 slots, but those 2 slots cannot form a complete batch, leading to wasted scheduling cycles.

3. **Fairshare-Gang plugin mismatch**: Fairshare calculates what a session "deserves" while Gang enforces batch atomicity at commit time. When Fairshare grants a non-batch-aligned allocation, Gang rejects partial batches, causing scheduling thrashing.

### Problem Example

```
Session A: batch_size=4, slots=1, 8 pending tasks
Session B: batch_size=1, slots=1, 4 pending tasks
Available cluster slots: 10

Current Fairshare behavior:
- Session A desired: 8 slots (8 tasks × 1 slot)
- Session B desired: 4 slots (4 tasks × 1 slot)
- After fair distribution: A deserved=6, B deserved=4

Problem:
- Session A with deserved=6 can only run 1 complete batch (4 executors)
- 2 slots are "deserved" but can never be used (incomplete batch)
- Allocate action keeps trying to allocate for Session A
- Gang plugin's is_ready() blocks until a full batch is pipelined
- Those 2 slots could have gone to Session B instead
```

### Objectives

1. **Batch-aligned `deserved` distribution**: Fairshare shall allocate `deserved` slots in multiples of `batch_size × slots`, ensuring every allocated slot can be utilized.
2. **Batch-aware `is_underused`**: Report a session as underused only if it can benefit from at least one more complete batch.
3. **Efficient scheduling**: Eliminate thrashing where Fairshare allocates resources that Gang rejects.

---

## 2. Function Specification

### Configuration

This enhancement uses the existing `batch_size` session attribute. No new configuration options are introduced.

### API

**Session Creation Validation:**

When `batch_size > 1`, the frontend validates that `min_instances` and `max_instances` are batch-aligned:

| Field           | Validation Rule                            | Error Message                                                          |
| --------------- | ------------------------------------------ | ---------------------------------------------------------------------- |
| `min_instances` | Must be 0 OR a multiple of `batch_size`    | `InvalidConfig: min_instances must be 0 or a multiple of batch_size` |
| `max_instances` | Must be None OR a multiple of `batch_size` | `InvalidConfig: max_instances must be a multiple of batch_size`      |

**Validation Examples (batch_size=4):**

| Parameter       | Value | Result   | Reason            |
| --------------- | ----- | -------- | ----------------- |
| `min_instances` | 0     | ✓ Valid  | No minimum        |
| `min_instances` | 4     | ✓ Valid  | 1 batch minimum   |
| `min_instances` | 2     | ✗ Reject | Not batch-aligned |
| `max_instances` | None  | ✓ Valid  | Unlimited         |
| `max_instances` | 8     | ✓ Valid  | 2 batches maximum |
| `max_instances` | 10    | ✗ Reject | Not batch-aligned |

### CLI

No CLI changes are required. Validation errors are returned when invalid parameter combinations are provided.

### Scope

**In Scope:**

- Frontend validation for `min_instances`/`max_instances` batch alignment
- Batch-aligned `deserved` calculation in Fairshare
- Batch-aware `is_underused` implementation
- Unit tests for batch-aligned Fairshare behavior

**Out of Scope:**

- Changes to Gang plugin (already handles batch atomicity correctly)
- New metrics or observability (may be added in follow-up work)

**Limitations:**

- When cluster capacity is not a multiple of `batch_size × slots`, some resources may remain unallocated to batch sessions.
- Sessions with large `batch_size` may receive fewer resources than "fair" if cluster capacity is limited.

### Feature Interaction

**Related Features:**

- **RFE400 (Batch Session)**: Introduced `batch_size` concept and Gang plugin
- **Gang Plugin**: Enforces batch atomicity at allocation/binding time
- **Allocate/Dispatch Actions**: Use Fairshare's `is_underused` to determine scheduling decisions

**Required Updates:**

| Component                    | Change                                                             |
| ---------------------------- | ------------------------------------------------------------------ |
| `Frontend::create_session()` | Add validation for `min_instances`/`max_instances` batch alignment |
| `FairShare::setup()`         | Modify `deserved` distribution to respect batch boundaries         |
| `FairShare::is_underused()`  | Check if at least one more batch can be allocated                  |
| `SSNInfo`                    | Add `batch_size` field to track batch configuration                |

**Integration Flow:**

```
Frontend validates batch alignment
    → Fairshare calculates deserved (batch-aligned)
    → Allocate/Dispatch use is_underused
    → Gang enforces batch atomicity
```

**Compatibility:**

- Fully backward compatible: `batch_size=1` sessions behave exactly as before (no validation required)
- No changes to proto definitions or storage schema

---

## 3. Implementation Detail

### Architecture

This enhancement modifies the Fairshare plugin's internal algorithm while maintaining its existing interface with other scheduler components:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Scheduler Context                               │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │                       PluginManager                             │    │
│  │  ┌─────────────────────┐  ┌─────────────────────┐               │    │
│  │  │   FairShare         │  │   GangPlugin        │               │    │
│  │  │                     │  │                     │               │    │
│  │  │ ENHANCED:           │  │ UNCHANGED:          │               │    │
│  │  │ - setup() aligns    │  │ - is_ready()        │               │    │
│  │  │   deserved to       │  │ - is_fulfilled()    │               │    │
│  │  │   batch boundaries  │  │ - on_executor_*     │               │    │
│  │  │                     │  │ - on_session_*      │               │    │
│  │  │ - is_underused()    │  │                     │               │    │
│  │  │   checks batch gap  │  │                     │               │    │
│  │  └─────────────────────┘  └─────────────────────┘               │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                     │                                   │
│            ┌────────────────────────┴──────────────────────┐            │
│            ▼                                               ▼            │
│  ┌─────────────────────┐                       ┌─────────────────────┐  │
│  │   AllocateAction    │                       │   DispatchAction    │  │
│  │                     │                       │                     │  │
│  │ Uses is_underused() │                       │ Uses is_underused() │  │
│  │ to decide if        │                       │ and is_fulfilled()  │  │
│  │ session needs more  │                       │ for binding         │  │
│  │ executors           │                       │                     │  │
│  └─────────────────────┘                       └─────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
```

### Components

#### 1. Frontend Validation

Location: `session_manager/src/apiserver/frontend.rs`

```rust
fn validate_session_spec(spec: &SessionSpec) -> Result<(), FlameError> {
    let batch_size = spec.batch_size.max(1);

    // Skip validation for batch_size=1 (backward compatible)
    if batch_size == 1 {
        return Ok(());
    }

    // min_instances must be 0 or a multiple of batch_size
    if spec.min_instances != 0 && spec.min_instances % batch_size != 0 {
        return Err(FlameError::InvalidArgument(format!(
            "min_instances ({}) must be 0 or a multiple of batch_size ({})",
            spec.min_instances, batch_size
        )));
    }

    // max_instances must be None or a multiple of batch_size
    if let Some(max) = spec.max_instances {
        if max % batch_size != 0 {
            return Err(FlameError::InvalidArgument(format!(
                "max_instances ({}) must be a multiple of batch_size ({})",
                max, batch_size
            )));
        }
    }

    Ok(())
}
```

#### 2. Fairshare Plugin Callbacks

The Fairshare plugin tracks `allocated` slots via callbacks from scheduler actions:

| Callback                 | Triggered By           | Action         | Purpose                                     |
| ------------------------ | ---------------------- | -------------- | ------------------------------------------- |
| `on_executor_allocate`   | `Statement.allocate()` | AllocateAction | Creating new executors on nodes             |
| `on_executor_unallocate` | `Statement.discard()`  | AllocateAction | Rolling back new executor creation          |
| `on_executor_pipeline`   | `Statement.pipeline()` | AllocateAction | Reserving existing void/unbinding executors |
| `on_executor_discard`    | `Statement.discard()`  | AllocateAction | Rolling back executor reservation           |
| `on_session_bind`        | `Statement.bind()`     | DispatchAction | Binding idle executors to sessions          |
| `on_session_unbind`      | `Statement.discard()`  | DispatchAction | Rolling back executor binding               |

All callbacks update `SSNInfo.allocated` because they all represent slots being consumed by a session. These callbacks remain unchanged; only the `setup()` distribution algorithm and `is_underused()` check are modified.

#### 3. SSNInfo Enhancement

Location: `session_manager/src/scheduler/plugins/fairshare.rs`

```rust
#[derive(Default, Clone)]
struct SSNInfo {
    pub id: SessionID,
    pub slots: u32,
    pub desired: f64,
    pub deserved: f64,
    pub allocated: f64,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,  // NEW: batch size for this session (default: 1)
}
```

#### 4. Enhanced setup()

The fair-share distribution algorithm is modified to allocate in batch units. The frontend guarantees that `min_instances` and `max_instances` are already batch-aligned when `batch_size > 1`.

```rust
impl Plugin for FairShare {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        // ... existing session initialization ...

        for ssn in open_ssns.values() {
            let batch_size = ssn.batch_size.max(1);

            // Frontend validates: min_instances and max_instances are batch-aligned

            // Calculate desired (batch-aligned from existing code)
            let batched_tasks = (task_count / batch_size as f64).floor() * batch_size as f64;
            let mut desired = batched_tasks * ssn.slots as f64;

            // Cap desired by max_instances
            if let Some(max) = ssn.max_instances {
                desired = desired.min((max * ssn.slots) as f64);
            }

            // Ensure desired is at least min_instances × slots
            let min_allocation = (ssn.min_instances * ssn.slots) as f64;
            desired = desired.max(min_allocation);

            self.ssn_map.insert(
                ssn.id.clone(),
                SSNInfo {
                    id: ssn.id.clone(),
                    desired,
                    deserved: min_allocation,
                    slots: ssn.slots,
                    min_instances: ssn.min_instances,
                    max_instances: ssn.max_instances,
                    batch_size,
                    ..SSNInfo::default()
                },
            );
        }

        // ... existing node and executor initialization ...

        // ENHANCED: Fair distribution in batch units
        let mut underused = BinaryHeap::from_iter(self.ssn_map.values_mut());
        loop {
            if remaining_slots < 0.001 {
                break;
            }

            if underused.is_empty() {
                break;
            }

            let ssn = underused.pop().expect("checked non-empty above");

            // Distribute in batch units
            let batch_unit = (ssn.batch_size * ssn.slots) as f64;

            // Skip if session already has its desired allocation
            if ssn.deserved >= ssn.desired {
                continue;
            }

            // Skip if not enough remaining slots for one batch unit
            if remaining_slots < batch_unit {
                continue;
            }

            // Allocate one batch unit
            let allocation = batch_unit.min(ssn.desired - ssn.deserved);
            ssn.deserved += allocation;
            remaining_slots -= allocation;

            // Re-add to heap if still underused
            if ssn.deserved < ssn.desired {
                underused.push(ssn);
            }
        }

        Ok(())
    }
}
```

#### 5. Enhanced is_underused()

```rust
fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
    self.ssn_map.get(&ssn.id).map(|ssn_info| {
        let batch_unit = (ssn_info.batch_size * ssn_info.slots) as f64;

        // Check if session can use at least one more batch.
        // Only compare against deserved; desired is already factored into
        // deserved during setup() (deserved <= desired is guaranteed).
        let allocated_batches = ssn_info.allocated / batch_unit;
        let deserved_batches = ssn_info.deserved / batch_unit;

        allocated_batches < deserved_batches
    })
}
```

### Data Structures

**SSNInfo:**

| Field           | Type          | Description                               |
| --------------- | ------------- | ----------------------------------------- |
| `id`            | `SessionID`   | Session identifier                        |
| `slots`         | `u32`         | Slots per executor                        |
| `desired`       | `f64`         | Total slots desired (batch-aligned)       |
| `deserved`      | `f64`         | Fair-share allocation (batch-aligned)     |
| `allocated`     | `f64`         | Currently allocated slots                 |
| `min_instances` | `u32`         | Minimum instances (validated at frontend) |
| `max_instances` | `Option<u32>` | Maximum instances (validated at frontend) |
| `batch_size`    | `u32`         | Executors per batch (default: 1)          |

### Algorithms

#### Frontend Validation Rules

For sessions with `batch_size > 1`:

| Parameter       | Validation                               | Examples (batch_size=4) |
| --------------- | ---------------------------------------- | ----------------------- |
| `min_instances` | Must be 0 OR multiple of `batch_size`    | 0 ✓, 4 ✓, 8 ✓, 2 ✗      |
| `max_instances` | Must be None OR multiple of `batch_size` | None ✓, 4 ✓, 8 ✓, 10 ✗  |

#### Batch-Aligned Fair Distribution

```
Input:
  sessions: list with (desired, min_instances, max_instances, batch_size, slots)
            (min_instances and max_instances validated at frontend)
  remaining_slots: total cluster capacity minus reserved minimums

Algorithm:
  1. Initialize each session's deserved = min_instances × slots
  2. Create priority queue ordered by deserved (ascending)

  3. While remaining_slots > 0 and queue not empty:
     a. Pop session with lowest deserved
     b. Calculate batch_unit = batch_size × slots

     c. If deserved >= desired:
        Session is satisfied, skip

     d. If remaining_slots < batch_unit:
        Cannot fit a batch, try next session

     e. Allocate one batch unit:
        deserved += min(batch_unit, desired - deserved)
        remaining_slots -= allocation

     f. If deserved < desired:
        Push back to queue for more allocation

  4. Remaining slots that cannot form complete batches are left unallocated

Output:
  Each session has deserved set to a batch-aligned value
```

**Complexity:** O(n × k × log n) where n = sessions, k = max batches per session

#### Example Walkthrough

```
Initial state:
  Session A: batch_size=4, slots=1, desired=8, min=0
  Session B: batch_size=1, slots=1, desired=4, min=0
  Cluster capacity: 10 slots

Iteration 1: Pop A (deserved=0)
  batch_unit=4, remaining=10 >= 4 ✓
  A.deserved = 4, remaining = 6
  Push A back

Iteration 2: Pop B (deserved=0)
  batch_unit=1, B.deserved = 1, remaining = 5
  Push B back

Iteration 3-5: B receives 3 more slots
  B.deserved = 4, remaining = 2
  B satisfied, not pushed back

Iteration 6: Pop A (deserved=4)
  batch_unit=4, remaining=2 < 4 ✗
  Skip (cannot fit another batch)

Final state:
  Session A: deserved=4 (1 complete batch)
  Session B: deserved=4 (4 executors)
  Unused: 2 slots (cannot form a batch for A, B is satisfied)
```

### System Considerations

| Aspect         | Impact                                                                    |
| -------------- | ------------------------------------------------------------------------- |
| Performance    | Minimal overhead; one additional field per session, simple arithmetic     |
| Scalability    | No impact on cluster scalability                                          |
| Reliability    | More predictable scheduling; eliminates Fairshare-Gang mismatch thrashing |
| Resource Usage | May leave slots unallocated when they cannot form complete batches        |

### Dependencies

No new dependencies. This enhancement uses existing Fairshare plugin infrastructure.

---

## 4. Use Cases

### Use Case 1: Multi-Node Inference with Fair Sharing

**Scenario:** Two LLM inference sessions share a cluster, one using 4-way tensor parallelism.

```
Session A (GPT-4): batch_size=4, slots=1, 16 pending tasks
Session B (GPT-2): batch_size=1, slots=1, 8 pending tasks
Cluster: 16 slots
```

| Metric     | Before Enhancement  | After Enhancement |
| ---------- | ------------------- | ----------------- |
| A deserved | 10-12 (not aligned) | 8 (2 batches)     |
| A usable   | 8 or 12             | 8                 |
| B deserved | 4-6                 | 8                 |
| Wasted     | 2-4 slots           | 0 slots           |

### Use Case 2: Mixed Batch Sizes

**Scenario:** Sessions with different batch sizes competing for resources.

```
Session A: batch_size=4, slots=2, desired=16 (2 batches)
Session B: batch_size=2, slots=1, desired=6 (3 batches)
Session C: batch_size=1, slots=1, desired=4
Cluster: 20 slots
```

**Distribution:** A=8, B=4, C=4, then A=8 (total), remaining=4 for B.

### Use Case 3: Cluster Fragmentation

**Scenario:** Cluster capacity does not align with batch requirements.

```
Session A: batch_size=5, slots=1, desired=10
Cluster: 8 slots
```

**Result:** A receives 5 slots (1 batch). Remaining 3 slots are available for sessions with smaller batch sizes.

### Use Case 4: Backward Compatibility

**Scenario:** Sessions without explicit batch_size (defaulting to 1).

```
Session A: batch_size=1, slots=1, desired=7
Session B: batch_size=1, slots=1, desired=5
Cluster: 10 slots
```

**Result:** Distribution identical to pre-enhancement behavior. `batch_unit=1` means any allocation is valid.

---

## 5. References

### Related Documents

- [RFE400 - Batch Support in Session](../RFE400-batch-session/FS.md)
- [Gang Scheduling (Wikipedia)](https://en.wikipedia.org/wiki/Gang_scheduling)

### Implementation References

| File                                                 | Description      |
| ---------------------------------------------------- | ---------------- |
| `session_manager/src/scheduler/plugins/fairshare.rs` | Fairshare plugin |
| `session_manager/src/scheduler/plugins/gang.rs`      | Gang plugin      |
| `session_manager/src/scheduler/plugins/mod.rs`       | Plugin trait     |
| `session_manager/src/scheduler/actions/allocate.rs`  | Allocate action  |
| `session_manager/src/scheduler/actions/dispatch.rs`  | Dispatch action  |
| `session_manager/src/apiserver/frontend.rs`          | Frontend API     |

---

## 6. Design Decisions

| Decision                      | Rationale                                                                                                                                                                                 |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Batch-unit distribution**   | Distribute in batch units during the fair-share algorithm rather than aligning after distribution. This ensures fairness is calculated on usable resources.                               |
| **Incomplete batch handling** | When cluster capacity cannot accommodate a full batch, those resources go to other sessions or remain unallocated. Allocating unusable resources wastes scheduling cycles.                |
| **No Gang plugin changes**    | The Gang plugin already correctly enforces batch atomicity. This enhancement ensures Fairshare uses consistent batch boundaries.                                                          |
| **Greedy batch allocation**   | Sessions receive one batch unit at a time via priority queue. This ensures fairness across sessions with different batch sizes.                                                           |
| **Frontend validation**       | Validate `min_instances` and `max_instances` at the API boundary rather than silently aligning them in the scheduler. This provides clear feedback to users about invalid configurations. |
| **Validation rules**          | When `batch_size > 1`, `min_instances` must be 0 or a multiple of `batch_size`, and `max_instances` must be None or a multiple of `batch_size`.                                           |

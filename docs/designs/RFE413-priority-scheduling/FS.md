# Design: Priority-Based Scheduling

**Status:** Draft  
**Author:** TBD  
**Created:** 2026-04-22  
**Issue:** [#413](https://github.com/xflops/flame/issues/413)

---

## 1. Motivation

### Background

The Flame scheduler historically supported **fairshare scheduling** (RFE400/RFE408), which distributed cluster resources proportionally across sessions based on their demand. While fairshare was appropriate for general equal-weight workloads, it did not support explicit prioritization:

- All sessions compete proportionally with no priority ordering
- There is no way to express that some workloads are more time-critical than others
- Low-priority sessions can consume resources that high-priority sessions need

In practice, organizations run diverse workloads with significantly different business importance: production inference services are more critical than development experiments; SLA-bound jobs must complete before exploratory batch processing. The current scheduler cannot express or enforce these preferences.

### Current Limitations

1. **No priority field**: Sessions have no `priority` attribute; all sessions are treated as equally important regardless of their business criticality.
2. **Cross-application ordering without priority awareness**: The legacy proportional-share `ssn_order_fn` ordered sessions by allocation balance, which reflected usage but not user-specified importance.
3. **Low-priority sessions can consume resources high-priority sessions need**: When cluster resources are limited, proportional sharing distributes resources to low-priority sessions at the expense of high-priority ones waiting to be scheduled.

### Problem Example

```
Cluster: 8 CPU total, all idle

Session A (critical inference): resreq=cpu=4,mem=…, pending=8, priority=unset → 0
Session B (batch experiment):   resreq=cpu=1,mem=…, pending=32, priority=unset → 0

Legacy proportional-share behavior (pre-PriorityPlugin):
  Session A deserved: ~5 CPU
  Session B deserved: ~3 CPU
  Both get resources simultaneously — no way to guarantee A runs first
```

### Objectives

1. **Explicit priority**: Add a `priority` field to sessions so users can express relative importance.
2. **CLI support**: Allow `flmctl create` to specify priority when creating a session.
3. **Priority-based allocation**: Implement a `PriorityPlugin` that ensures high-priority sessions receive resources before low-priority ones. If a high-priority session is still needy (has pending tasks), low-priority sessions are not allocated new resources.
4. **Global cross-application ordering**: Sessions are ordered globally by priority regardless of the application they belong to.

---

## 2. Function Specification

### Configuration

| Parameter  | Type     | Default | Description                                                                                        |
| ---------- | -------- | ------- | -------------------------------------------------------------------------------------------------- |
| `priority` | `uint32` | `0`     | Session priority. Higher value = higher priority. Sessions with `priority = 0` use default priority. |

**Scheduling policy**: Priority-based scheduling is always active when the `PriorityPlugin` is registered. No configuration flag is required to enable it; the plugin is registered by default alongside `DRFPlugin` and `GangPlugin`. When all sessions share the same priority, the priority dimension has no effect and ordering falls through to the downstream deferral plugins.

### API

**Proto Changes (`rpc/protos/types.proto`):**

```protobuf
message SessionSpec {
  string application = 2;
  // Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse.
  reserved 3;
  reserved "slots";
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  uint32 batch_size = 7;

  // NEW: Session priority (default: 0, higher value = higher priority)
  uint32 priority = 8;

  optional ResourceRequirement resreq = 9;  // Explicit resource request; server applies cluster default when omitted
}
```

**Validation Rules:**

| Field      | Validation                        | Notes                                          |
| ---------- | --------------------------------- | ---------------------------------------------- |
| `priority` | No upper bound; any `uint32` value | Proto3 default of `0` represents lowest priority |

**Default Value Handling:**
- Proto3 `uint32` defaults to `0`; treat `priority = 0` as lowest (default) priority
- `SessionAttributes::default()` sets `priority = 0`
- All existing sessions without an explicit priority continue to function as `priority = 0`

### CLI

**Modified command: `flmctl create`**

```bash
# Current usage
flmctl create --app <APPLICATION> [--resreq <RESREQ>] [--batch-size <BATCH_SIZE>]

# Updated usage
flmctl create --app <APPLICATION> [--resreq <RESREQ>] [--batch-size <BATCH_SIZE>] [--priority <PRIORITY>]
```

**New flag:**

| Flag         | Short | Type   | Default | Description                           |
| ------------ | ----- | ------ | ------- | ------------------------------------- |
| `--priority` | `-p`  | `u32`  | `0`     | Session priority (higher = more important) |

**Usage examples:**

```bash
# Create a high-priority production session
flmctl create --app llm-inference --resreq cpu=4,mem=16g,gpu=1 --priority 100

# Create a medium-priority training session
flmctl create --app model-training --resreq cpu=8,mem=32g,gpu=2 --priority 50

# Create a low-priority batch session (default priority; resreq from cluster default)
flmctl create --app data-preprocessing

# Combine priority with batch/gang scheduling
flmctl create --app llm-inference --resreq cpu=4,mem=16g,gpu=1 --batch-size 4 --priority 100
```

**Session listing (`flmctl list -s`):**

```
ID                State   App               Resreq                  Batch  Priority  Pending  Running  Succeed  Failed
inference-001     Open    llm-inference     cpu=4,mem=16g,gpu=1     1      100       16       4        32       0
training-abcd     Open    model-training    cpu=8,mem=32g,gpu=2     1      50        8        2        0        0
preprocess-xyz    Open    data-preprocess   cpu=1,mem=1g,gpu=0      1      0         100      1        50       0
```

### Other Interfaces

**Python SDK (`flamepy`):**

```python
# open_session accepts priority (passed through to SessionSpec)
session = flame.open_session(
    session_id="inference-001",
    application="llm-inference",
    resreq="cpu=4,mem=16g,gpu=1",
    priority=100,  # NEW optional argument, default 0
)
```

### Scope

**In Scope:**

- `priority` field in `SessionSpec` proto, `SessionAttributes`, `Session`, and `SessionInfo`
- `flmctl create` flag `--priority` / `-p`
- `PriorityPlugin` implementing priority-based session ordering and allocation blocking
- Session listing shows `priority` column
- SQLite storage for `priority` field

**Out of Scope:**

- **Priority-based preemption**: Reclaiming executors from low-priority sessions for high-priority ones. Deferred to a future RFE due to executor lifecycle complexity.
- **Dynamic priority adjustment**: Changing a session's priority after it is created.
- **Application-level priority**: Priority set at the application registration level (separate from per-session priority).
- **Priority inheritance**: Propagating priority from sessions to tasks or across related sessions.
- **Negative priority**: Using signed integers to express deprioritization below default.

**Limitations:**

- Priority affects **new** resource allocation only; executors already assigned to low-priority sessions are not reclaimed when a higher-priority session arrives (no preemption in V1).
- Sessions with equal priority are ordered by downstream plugins (DRF) exactly as before.
- A session with pending tasks at a given priority level blocks all sessions at lower priority levels from receiving new resources, even if those lower-priority sessions could use idle resources that the high-priority session cannot reach.

### Feature Interaction

**Related Features:**

| Feature | Interaction |
| ------- | ----------- |
| **DRF (RFE433)** | Priority ordering overlays DRF's dominant-resource fairness ordering. Within the same priority level, DRF determines per-session resource shares and session ordering by dominant resource fraction. |
| **GangPlugin (RFE400)** | Orthogonal. `batch_size` constraints apply within a priority level independently of priority ordering. |
| **AllocateAction** | Consults `ssn_order_fn` from all plugins; PriorityPlugin's ordering ensures high-priority sessions are processed before low-priority ones. |
| **DispatchAction** | Consults `is_underused` from all plugins; PriorityPlugin blocks dispatch to lower-priority sessions when any higher-priority session is needy. |
| **ShuffleAction** | Unchanged in V1. Priority-based preemption (reclaiming executors via ShuffleAction) is deferred. |

**Required Updates:**

| Component | Change |
| --------- | ------ |
| `rpc/protos/types.proto` | Add `priority = 8` to `SessionSpec` |
| `common/src/apis/types.rs` | Add `priority: u32` to `Session` and `SessionAttributes` |
| `session_manager/src/model/mod.rs` | Add `priority: u32` to `SessionInfo` |
| `session_manager/src/storage/` | Add `priority` column to sessions table (default `0`) |
| `session_manager/src/apiserver/frontend.rs` | Pass `priority` from proto to `SessionAttributes` |
| `flmctl/src/create.rs` | Add `--priority` flag; pass to `SessionAttributes` |
| `flmctl/src/main.rs` | Add `priority` argument to `CreateArgs` |
| `session_manager/src/scheduler/plugins/priority.rs` | **NEW** `PriorityPlugin` |
| `session_manager/src/scheduler/plugins/mod.rs` | Register `PriorityPlugin`; ensure it is consulted before downstream deferral plugins (DRF, Gang) in `ssn_order_fn` |

**Compatibility:**

- **Fully backward compatible.** All existing sessions default to `priority = 0`. When all sessions share the same priority, PriorityPlugin returns `None` for both `ssn_order_fn` and `is_underused`, leaving behavior identical to the downstream plugin chain (DRF, Gang).
- Proto3 default for `uint32` is `0`, matching the desired default. No proto migration required.
- SQLite column added with `DEFAULT 0`. No row migration required.

**Breaking Changes:** None.

---

## 3. Implementation Detail

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            Scheduler Context                                │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                          PluginManager                              │    │
│  │                                                                     │    │
│  │  ┌──────────────────┐  ┌───────────────────┐  ┌──────────────────┐  │    │
│  │  │  PriorityPlugin  │  │    DRFPlugin      │  │   GangPlugin     │  │    │
│  │  │                  │  │                   │  │                  │  │    │
│  │  │ setup():         │  │ setup():          │  │ setup():         │  │    │
│  │  │  read cluster    │  │  compute          │  │  track batch     │  │    │
│  │  │  total           │  │  dominant         │  │  state           │  │    │
│  │  │  ResourceReq     │  │  resource shares  │  │                  │  │    │
│  │  │  distribute by   │  │                   │  │                  │  │    │
│  │  │  (priority desc, │  │                   │  │                  │  │    │
│  │  │   creation asc)  │  │                   │  │                  │  │    │
│  │  │  → ssn_desired   │  │                   │  │                  │  │    │
│  │  │  (ResourceReq)   │  │                   │  │                  │  │    │
│  │  │  init            │  │                   │  │                  │  │    │
│  │  │  ssn_allocated   │  │                   │  │                  │  │    │
│  │  │                  │  │                   │  │                  │  │    │
│  │  │ ssn_order_fn():  │  │ ssn_order_fn():   │  │   (no opinion)   │  │    │
│  │  │  sort descending │  │  sort by          │  │                  │  │    │
│  │  │  by priority     │  │  dominant share   │  │                  │  │    │
│  │  │  (primary key)   │  │  (tiebreaker)     │  │                  │  │    │
│  │  │                  │  │                   │  │                  │  │    │
│  │  │ is_underused():  │  │ is_underused():   │  │ is_underused():  │  │    │
│  │  │  Some(false) if  │  │  check dominant   │  │  check batch     │  │    │
│  │  │  lower priority  │  │  share vs target  │  │  capacity        │  │    │
│  │  │  than max_needy; │  │                   │  │                  │  │    │
│  │  │  else Some(true) │  │                   │  │                  │  │    │
│  │  │  while alloc <   │  │                   │  │                  │  │    │
│  │  │  ssn_desired     │  │                   │  │                  │  │    │
│  │  └──────────────────┘  └───────────────────┘  └──────────────────┘  │    │
│  │       consulted first       consulted second       consulted third  │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                       │                                     │
│             ┌─────────────────────────┴──────────────────────┐              │
│             ▼                                                ▼              │
│   ┌─────────────────────┐                        ┌─────────────────────┐    │
│   │   AllocateAction    │                        │   DispatchAction    │    │
│   │                     │                        │                     │    │
│   │ Sessions ordered by │                        │ Sessions filtered   │    │
│   │ ssn_order_fn():     │                        │ by is_underused():  │    │
│   │   Priority(desc) →  │                        │   lower-priority    │    │
│   │   DRF dominant share│                        │   sessions blocked  │    │
│   │ High-prio first     │                        │   when higher-prio  │    │
│   │                     │                        │   session is needy  │    │
│   └─────────────────────┘                        └─────────────────────┘    │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Components

#### 1. Proto and Data Model Changes

**`rpc/protos/types.proto`:**

Add `priority` as field `8` in `SessionSpec`. No existing fields are modified.

```protobuf
message SessionSpec {
  string application = 2;
  // Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse.
  reserved 3;
  reserved "slots";
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  uint32 batch_size = 7;
  uint32 priority = 8;  // NEW: default 0 (lowest priority)
  optional ResourceRequirement resreq = 9;  // Explicit resource request; server applies cluster default when omitted
}
```

**`common/src/apis/types.rs`:**

```rust
pub struct SessionAttributes {
    pub id: SessionID,
    pub application: String,
    pub common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,  // NEW: default 0
    pub resreq: Option<ResourceRequirement>,
}

impl Default for SessionAttributes {
    fn default() -> Self {
        Self {
            id: String::new(),
            application: String::new(),
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,  // NEW
            resreq: None,
        }
    }
}

pub struct Session {
    // ... existing fields unchanged ...
    pub priority: u32,  // NEW
}
```

**`session_manager/src/model/mod.rs`:**

```rust
pub struct SessionInfo {
    pub id: SessionID,
    pub application: String,
    pub tasks_status: HashMap<TaskState, i32>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub state: SessionState,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,  // NEW
    pub resreq: Option<ResourceRequirement>,
}
```

#### 2. Storage Changes

**SQLite schema:**

```sql
-- Add priority to sessions table (backward compatible: existing rows default to 0)
ALTER TABLE sessions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
```

No data migration is required. Existing rows receive `priority = 0` automatically.

#### 3. Frontend Changes

**`session_manager/src/apiserver/frontend.rs`:**

The `create_session` handler maps `SessionSpec.priority` to `SessionAttributes.priority`. No validation beyond the proto type is required.

```rust
fn session_spec_to_attributes(id: &str, spec: &SessionSpec) -> SessionAttributes {
    SessionAttributes {
        id: id.to_owned(),
        application: spec.application.clone(),
        common_data: spec.common_data.clone(),
        min_instances: spec.min_instances,
        max_instances: spec.max_instances,
        batch_size: spec.batch_size.max(1),
        priority: spec.priority,  // NEW
        resreq: spec.resreq.map(apis::ResourceRequirement::from),
    }
}
```

`resolve_session_resreq` (also in `apiserver::frontend`) then turns the optional client-supplied `resreq` into a concrete `ResourceRequirement` for the session — see RFE433 for the full resolution chain.

#### 4. PriorityPlugin

**Location:** `session_manager/src/scheduler/plugins/priority.rs`

The `PriorityPlugin` owns priority-aware resource distribution. Each scheduling cycle, `setup()`:

1. Retrieves the cluster's total capacity as a `ResourceRequirement` (the per-resource sum of every node's `allocatable` — `cpu`, `memory`, `gpu`). Capacity is tracked per-resource, never as a derived scalar.
2. Distributes that total `ResourceRequirement` across open sessions in descending order of priority. Within a priority tier, sessions are ordered by creation time ascending — earlier sessions take precedence. For each session, the per-executor effective resreq `per_task` is taken directly from `ssn.resreq` (which the apiserver populates via `resolve_session_resreq` before the scheduler ever sees the session, so the plugin `.expect`s it); `compute_demand(ssn, &per_task)` produces a per-resource demand which is then clamped per-field against `remaining` (`demand.min(&remaining)`). The result is recorded as `ssn_desired[id]` (a `ResourceRequirement`), and the chosen `per_task` is cached in `ssn_unit[id]` so the lifecycle callbacks can adjust `ssn_allocated` without re-deriving it from the snapshot.
3. Initializes `ssn_allocated[id]` from currently-bound executors using each executor's `exe.resreq` directly. The update process for `ssn_allocated` after `setup()` is unchanged in shape: per-session lifecycle callbacks adjust the counter by adding/subtracting the cached `ResourceRequirement` (`ssn_unit`) per event.

`max_needy_priority` is computed in the same pass — the highest priority among sessions that still have pending tasks — and used by `is_underused` to block lower-priority sessions even when slack appears in their tier.

```rust
pub struct PriorityPlugin {
    /// Maximum priority among open sessions with pending tasks.
    /// Computed in `setup()`; used in `is_underused`.
    max_needy_priority: u32,
    /// Priority for each open session, keyed by session ID.
    /// Populated in `setup()` for fast lookup during `ssn_order_fn` / `is_underused`.
    ssn_priority: HashMap<SessionID, u32>,
    /// Per-session priority-distributed share, expressed as a `ResourceRequirement`
    /// (cpu / memory / gpu). Populated in `setup()` step 2 from the cluster-capacity
    /// distribution loop. Read-only thereafter for the cycle.
    ssn_desired: HashMap<SessionID, ResourceRequirement>,
    /// Executor resources currently allocated per session, expressed as a
    /// `ResourceRequirement`. Initialised in `setup()` from existing bound
    /// executors; updated thereafter by the executor / session lifecycle callbacks.
    ssn_allocated: HashMap<SessionID, ResourceRequirement>,
    /// Per-executor effective resreq for each session, cached in `setup()` step 2
    /// so callbacks can add/sub the right `ResourceRequirement` on bind / unbind /
    /// allocate / unallocate without re-deriving it from the snapshot.
    /// `resreq` is guaranteed populated by `resolve_session_resreq` in the apiserver,
    /// so this is simply a clone of `ssn.resreq`.
    ssn_unit: HashMap<SessionID, ResourceRequirement>,
}

impl PriorityPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(PriorityPlugin {
            max_needy_priority: 0,
            ssn_priority: HashMap::new(),
            ssn_desired: HashMap::new(),
            ssn_allocated: HashMap::new(),
            ssn_unit: HashMap::new(),
        })
    }
}

impl Plugin for PriorityPlugin {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_priority.clear();
        self.ssn_desired.clear();
        self.ssn_allocated.clear();
        self.ssn_unit.clear();
        self.max_needy_priority = 0;

        // ── Step 1: total cluster capacity (per-resource) ────────────────────
        // Cluster's physical scheduling capacity, taken from the same snapshot
        // every plugin sees this cycle. Each field (cpu, memory, gpu) is summed
        // independently so the priority distribution can clamp per-resource.
        let mut total = ResourceRequirement::default();
        for n in ss.find_nodes(ALL_NODE)?.values() {
            total.add(&n.allocatable);
        }

        // ── Step 2: distribute `total` by (priority desc, creation_time asc) ─
        // Earlier-created sessions take precedence within a priority tier.
        let open_ssns = ss.find_sessions(OPEN_SESSION)?;
        let mut sessions: Vec<SessionInfoPtr> = open_ssns.values().cloned().collect();
        sessions.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)                            // priority descending
                .then(a.creation_time.cmp(&b.creation_time)) // earlier first
        });

        let mut remaining = total.clone();
        for ssn in &sessions {
            self.ssn_priority.insert(ssn.id.clone(), ssn.priority);

            // Per-task / per-executor effective resreq. After the slots-cleanup
            // refactor, `resolve_session_resreq` in `apiserver::frontend` always
            // populates `ssn.resreq` before the session reaches the scheduler,
            // so the `expect` documents the post-condition.
            let per_task = ssn
                .resreq
                .clone()
                .expect("SessionInfo.resreq must be populated by resolve_session_resreq");

            let demand = compute_demand(ssn, &per_task);   // task-driven ceiling
            // Per-field min — guaranteed `granted ≤ remaining` per resource.
            let granted = demand.min(&remaining);

            self.ssn_desired.insert(ssn.id.clone(), granted.clone());
            self.ssn_allocated
                .insert(ssn.id.clone(), ResourceRequirement::default());
            self.ssn_unit.insert(ssn.id.clone(), per_task);

            // `granted = remaining.min(demand)` per-field, so `granted ≤ remaining`
            // always holds in every dimension; sub() cannot underflow here.
            remaining
                .sub(&granted)
                .expect("granted ≤ remaining by construction (per-field min)");

            // max_needy_priority pass (same loop, unchanged criterion)
            let pending = ssn
                .tasks_status
                .get(&TaskState::Pending)
                .copied()
                .unwrap_or(0);
            if pending > 0 && ssn.priority > self.max_needy_priority {
                self.max_needy_priority = ssn.priority;
            }
        }

        // ── Step 3: ssn_allocated initial counts from bound executors ────────
        // Use the executor's `resreq` directly — it is the source of truth for
        // what resources are actually consumed.
        for exe in ss.find_executors(ALL_EXECUTOR)?.values() {
            if let Some(ssn_id) = &exe.ssn_id {
                if let Some(alloc) = self.ssn_allocated.get_mut(ssn_id) {
                    alloc.add(&exe.resreq);
                }
            }
        }

        Ok(())
    }

    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let p1 = self.ssn_priority.get(&s1.id).copied().unwrap_or(0);
        let p2 = self.ssn_priority.get(&s2.id).copied().unwrap_or(0);

        if p1 != p2 {
            // Higher priority comes first (descending order).
            Some(p2.cmp(&p1))
        } else {
            // Equal priority: defer to creation time at AllocateAction level via
            // the same comparator (earlier session first); for ssn_order_fn we
            // return None and let the next plugin in the chain break the tie
            // within the priority tier.
            None
        }
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let priority = self.ssn_priority.get(&ssn.id).copied()?;

        // Lower than the highest needy priority → hard-blocked.
        if priority < self.max_needy_priority {
            return Some(false);
        }

        // Eligible tier: still underused while allocated is short of desired in
        // ANY resource dimension. Demand of zero (default) is treated as
        // "no demand → defer to the next plugin in the chain".
        let desired = self.ssn_desired.get(&ssn.id)?;
        let allocated = self.ssn_allocated.get(&ssn.id)?;
        let zero = ResourceRequirement::default();

        if !desired.equal(&zero) && !allocated.great_equal(desired) {
            Some(true)   // overrides any downstream-plugin veto
        } else {
            None         // demand met (or no demand) — defer to DRF / Gang
        }
    }

    // ── ssn_allocated lifecycle callbacks ─────────────────────────────────────
    // Each callback looks up the session's cached per-executor effective resreq
    // (`ssn_unit`, populated in setup() step 2) and adds or subtracts it from
    // `ssn_allocated`. Sessions absent from `ssn_unit` (e.g. closed mid-cycle)
    // are skipped. `sub` callbacks defensively log on underflow instead of
    // panicking, since `ResourceRequirement::sub` returns `Result`.

    fn on_executor_allocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        let unit = match self.ssn_unit.get(&ssn.id) { Some(u) => u.clone(), None => return };
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            alloc.add(&unit);
        }
    }
    fn on_executor_unallocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        let unit = match self.ssn_unit.get(&ssn.id) { Some(u) => u.clone(), None => return };
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            if let Err(e) = alloc.sub(&unit) {
                tracing::warn!("[PriorityPlugin] sub underflow on unallocate for ssn <{}>: {e}", ssn.id);
            }
        }
    }
    fn on_executor_pipeline(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        let unit = match self.ssn_unit.get(&ssn.id) { Some(u) => u.clone(), None => return };
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            alloc.add(&unit);
        }
    }
    fn on_executor_discard(&mut self, _exec: ExecutorInfoPtr, ssn: SessionInfoPtr) {
        let unit = match self.ssn_unit.get(&ssn.id) { Some(u) => u.clone(), None => return };
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            if let Err(e) = alloc.sub(&unit) {
                tracing::warn!("[PriorityPlugin] sub underflow on discard for ssn <{}>: {e}", ssn.id);
            }
        }
    }
    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {
        let unit = match self.ssn_unit.get(&ssn.id) { Some(u) => u.clone(), None => return };
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            alloc.add(&unit);
        }
    }
    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        let unit = match self.ssn_unit.get(&ssn.id) { Some(u) => u.clone(), None => return };
        if let Some(alloc) = self.ssn_allocated.get_mut(&ssn.id) {
            if let Err(e) = alloc.sub(&unit) {
                tracing::warn!("[PriorityPlugin] sub underflow on unbind for ssn <{}>: {e}", ssn.id);
            }
        }
    }

    // All other Plugin methods (node ordering, preemptibility, availability,
    // allocatability, reclaimability, gang readiness) return defaults.
}
```

**`compute_demand(ssn, per_task)`** — the per-session demand ceiling used in the distribution loop, as a `ResourceRequirement`. `per_task` is the session's effective per-executor resreq, which after the slots-cleanup refactor is always `ssn.resreq` (server-populated).

```rust
fn compute_demand(ssn: &SessionInfo, per_task: &ResourceRequirement) -> ResourceRequirement {
    // Sum pending + running task counts; round down to whole batches; scale per-resource.
    let mut task_count: u32 = 0;
    for state in [TaskState::Pending, TaskState::Running] {
        if let Some(c) = ssn.tasks_status.get(&state) {
            task_count = task_count.saturating_add((*c).max(0) as u32);
        }
    }
    let batch_size = ssn.batch_size.max(1);
    // Integer floor: for non-negative counts, `/` is floor.
    let batched = (task_count / batch_size) * batch_size;

    let mut demand = per_task.mul(batched);

    if let Some(max_i) = ssn.max_instances {
        // Per-field clamp from above.
        demand = demand.min(&per_task.mul(max_i));
    }
    let floor = per_task.mul(ssn.min_instances);
    // Per-field clamp from below.
    demand.max(&floor)
}
```

#### 5. PluginManager Registration and ssn_order_fn Chain

**`session_manager/src/scheduler/plugins/mod.rs`:**

`PriorityPlugin` is registered before the downstream deferral plugins (DRF, Gang) in the plugin list. The `PluginManager::ssn_order_fn` walks plugins in registration order and returns the first non-`None` result:

```rust
pub fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Ordering {
    let plugins = lock_ptr!(self.plugins).expect("plugins lock");
    for plugin in plugins.values() {
        if let Some(ord) = plugin.ssn_order_fn(s1, s2) {
            return ord;
        }
    }
    Ordering::Equal
}
```

**Registration order and semantics:**

| Order | Plugin | ssn_order_fn behavior |
| ----- | ------ | --------------------- |
| 1 | `PriorityPlugin` | Returns `Some(ord)` when priorities differ; `None` when equal |
| 2 | `DRFPlugin` | Returns `Some(ord)` based on dominant resource share |
| 3 | `GangPlugin` | Returns `None` (no ordering opinion) |

This chain ensures priority is the primary sort key, with DRF providing tiebreaking within a priority level.

The `is_underused` aggregation rule is **ANY** (any plugin returning `Some(true)` makes a session underused). `PriorityPlugin` returns `Some(false)` for blocked lower-priority sessions, and `None` for sessions at or above the needy priority tier. The `None` return defers to DRF and GangPlugin, preserving all existing underuse logic for unblocked sessions.

#### 6. flmctl Changes

**`flmctl/src/create.rs`:**

```rust
pub async fn run(
    ctx: &FlameContext,
    app: &str,
    resreq: Option<ResourceRequirement>,
    batch_size: &u32,
    priority: &u32,  // NEW parameter
) -> Result<(), Box<dyn Error>> {
    let conn = build_connection(ctx).await?;
    let attr = SessionAttributes {
        id: format!("{app}-{}", stdng::rand::short_name()),
        application: app.to_owned(),
        common_data: None,
        min_instances: 0,
        max_instances: None,
        batch_size: *batch_size,
        priority: *priority,  // NEW
        resreq,
    };
    let ssn = conn.create_session(&attr).await?;
    println!("Session <{}> was created.", ssn.id);
    Ok(())
}
```

**`flmctl/src/main.rs`:**

```rust
#[derive(Args)]
struct CreateArgs {
    /// Application name
    #[arg(short = 'a', long)]
    app: String,

    /// Per-executor resource request (e.g. "cpu=4,mem=16g,gpu=1");
    /// when omitted, the server applies `cluster.resreq`.
    #[arg(short = 'r', long)]
    resreq: Option<String>,

    /// Executors per batch (gang scheduling)
    #[arg(short = 'b', long, default_value = "1")]
    batch_size: u32,

    /// Session priority (higher = more important, default: 0)
    #[arg(short = 'p', long, default_value = "0")]  // NEW
    priority: u32,
}
```

### Data Structures

**`PriorityPlugin` internal state:**

| Field                | Type                                          | Description                                                                 |
| -------------------- | --------------------------------------------- | --------------------------------------------------------------------------- |
| `max_needy_priority` | `u32`                                         | Highest priority among sessions with pending tasks; computed in `setup()`   |
| `ssn_priority`       | `HashMap<SessionID, u32>`                     | Priority for each open session; populated in `setup()`; consulted in order functions |
| `ssn_desired`        | `HashMap<SessionID, ResourceRequirement>`     | Per-session priority-distributed share, as a `ResourceRequirement` (cpu / memory / gpu); populated in `setup()` step 2 from the cluster-capacity distribution loop; read-only thereafter for the cycle |
| `ssn_allocated`      | `HashMap<SessionID, ResourceRequirement>`     | Resources currently allocated to each session; initialised in `setup()` step 3 from bound executors using `exe.resreq`; updated thereafter by the executor / session lifecycle callbacks |
| `ssn_unit`           | `HashMap<SessionID, ResourceRequirement>`     | Per-executor effective resreq for each session (a clone of `ssn.resreq`, which is guaranteed populated by `resolve_session_resreq` in the apiserver); populated in `setup()` step 2; used by lifecycle callbacks to know how much to add / sub on bind / unbind / allocate / unallocate |

**`SessionInfo` extension:**

| Field      | Type                            | Default | Description                              |
| ---------- | ------------------------------- | ------- | ---------------------------------------- |
| `priority` | `u32`                           | `0`     | Session priority; higher = more important |
| `resreq`   | `Option<ResourceRequirement>`   | `None`  | Per-task resource request. Already present from RFE433; the wire field is `Option` for backward compat, but after the slots-cleanup refactor the apiserver's `resolve_session_resreq` always populates a concrete value before sessions reach the scheduler, so `PriorityPlugin::setup` `.expect`s it when deriving the per-executor effective resreq (`per_task`). |

### Algorithms

#### Priority Ordering (ssn_order_fn)

```
Input: two sessions s1, s2

PriorityPlugin.ssn_order_fn(s1, s2):
  p1 = ssn_priority[s1.id]  (default 0)
  p2 = ssn_priority[s2.id]  (default 0)

  if p1 > p2 → return Some(Less)     (s1 comes first in sorted order)
  if p1 < p2 → return Some(Greater)  (s2 comes first)
  if p1 == p2 → return None          (defer to the next plugin in the chain)

PluginManager chain (first non-None wins):
  PriorityPlugin → DRFPlugin → GangPlugin → Ordering::Equal
```

#### Priority-Aware Resource Distribution (PriorityPlugin.setup)

The distribution algorithm runs once per scheduling cycle inside `PriorityPlugin::setup`. It produces `ssn_desired[id]` for every open session. `ssn_allocated[id]` is updated by the existing process — initialized from current bound executors and adjusted thereafter by the executor / session lifecycle callbacks.

```
Input : SnapShot ss
Output: ssn_desired   : map<SessionID, ResourceRequirement>
        ssn_allocated : map<SessionID, ResourceRequirement>   (initial counts only; runtime updates via callbacks)
        ssn_unit      : map<SessionID, ResourceRequirement>   (per-executor effective resreq; cached for callbacks)
        max_needy_priority : u32

# Step 1 — total cluster capacity (per-resource)
total     = Σ node.allocatable                     # per-field add across nodes
remaining = total

# Step 2 — distribute by (priority desc, creation_time asc)
sorted = ss.open_sessions sorted by:
    primary  : priority      (descending)          # higher priority first
    secondary: creation_time (ascending)           # earlier session first

for ssn in sorted:
    per_task = ssn.resreq                           # populated by resolve_session_resreq; always concrete
    demand   = compute_demand(ssn, per_task)        # per-resource ceiling

    granted = demand.min(&remaining)                # per-field min — guaranteed ≤ remaining
    ssn_desired[ssn.id]   = granted
    ssn_allocated[ssn.id] = ResourceRequirement::default()   # filled in step 3
    ssn_unit[ssn.id]      = per_task                # cache for callbacks

    remaining.sub(&granted)                         # cannot underflow by construction

    if pending(ssn) > 0 and ssn.priority > max_needy_priority:
        max_needy_priority = ssn.priority

# Step 3 — initial ssn_allocated from bound executors (uses exe.resreq directly)
for exe in ss.executors where exe.ssn_id is set:
    ssn_allocated[exe.ssn_id].add(&exe.resreq)

# After setup, ssn_allocated continues to be updated by the
# on_executor_allocate / on_executor_unallocate / on_executor_pipeline /
# on_executor_discard / on_session_bind / on_session_unbind callbacks,
# each adding or subtracting the cached ssn_unit[ssn.id].
```

**Invariants enforced by this algorithm:**

| Invariant | Justification |
| --------- | ------------- |
| `Σ ssn_desired ≤ total` per-field (cpu, memory, gpu) | Each grant is capped at `remaining` via per-field `min`, and `remaining` starts at `total` and is monotonically decremented per-field. |
| `Σ ssn_desired = total` per-field when aggregate `compute_demand ≥ total` in that field | Higher-priority (and earlier within a tier) sessions saturate first; later sessions absorb the residual until cluster capacity is exhausted in that resource dimension. |
| Within equal priority, earlier-created sessions are filled first | `creation_time ascending` is the secondary sort key. |
| `ssn_allocated` update flow is in shape unchanged | Only the *initial* counts are populated in `setup()`; runtime adjustments use the same six callbacks, each adding or subtracting `ssn_unit[ssn.id]` instead of a scalar. |

**Why creation time as the tiebreaker:**

Within a priority tier, the earlier session represents work that has been waiting longer for resources. Filling it first reduces head-of-line latency for established sessions while remaining deterministic and stable across scheduling cycles. Session IDs are not used as a tiebreaker because they are not ordered by submission time and would give arbitrary winners.

#### Priority Blocking (is_underused)

```
Computed in setup():
  max_needy_priority = max{ ssn.priority | ssn has pending tasks > 0 }

For each session in is_underused(ssn):
  priority = ssn_priority[ssn.id]

  if priority < max_needy_priority:
    return Some(false)   → blocked; not considered for new allocation

  desired   = ssn_desired[ssn.id]
  allocated = ssn_allocated[ssn.id]

  # Any-field-less semantics: still underused if allocated is short of desired
  # in ANY resource dimension. Zero demand is "no demand → defer".
  if desired != ResourceRequirement::default() and !allocated.great_equal(&desired):
    return Some(true)    → overrides any downstream-plugin veto
  else:
    return None          → let DRF and GangPlugin decide

Aggregation (ANY semantics):
  is_underused(ssn) = PriorityPlugin OR DRFPlugin OR GangPlugin
  If PriorityPlugin returns Some(false), session is not underused
  regardless of what DRF/GangPlugin return.
```

**Why `pending > 0` as the "needy" criterion:**

A session with pending tasks has work it cannot yet run — it can benefit from additional executors. A session with zero pending tasks is either idle or satisfied; it should not block lower-priority sessions even if it holds fewer executors than its `deserved` allocation.

Because PriorityPlugin caps `Σ ssn_desired ≤ total` per-field (see invariants), the previous over-allocation symptom — where aggregate demand asked for more than the cluster physically has, in any resource dimension — cannot occur even when individual session demand is large.

#### Example Walkthrough

All sessions below use `batch_size=1` for clarity, so `compute_demand(ssn, per_task) = task_count × per_task` capped/floored as usual (per-field).

**Note on units in these examples.** All three sessions in this walkthrough have `resreq = (cpu=2, memory=2, gpu=0)` for A and B and `resreq = (cpu=4, memory=4, gpu=0)` for C — the numbers happen to coincide with the legacy "slots × unit = (1, 1, 0)" view, so the values below can be read as either cpu units or memory units (gpu = 0 throughout). The per-resource implementation in §3 *PriorityPlugin* is what runs in production.

**Case 1 — Cluster has slack (cluster `cpu = memory = 22`, `gpu = 0`):**

```
Cluster: cpu = memory = 22, all idle

Sessions (creation_time shown as relative t₀ < t₁ < t₂):
  Session A: priority=100, resreq=(cpu=2,mem=2,gpu=0), pending=4,  created t₀ → demand = 8
  Session B: priority=100, resreq=(cpu=2,mem=2,gpu=0), pending=3,  created t₁ → demand = 6
  Session C: priority=10,  resreq=(cpu=4,mem=4,gpu=0), pending=5,  created t₂ → demand = 20

PriorityPlugin.setup() — Step 1: total cpu = memory = 22, remaining = 22
                        Step 2: sort by (priority desc, creation_time asc)
                                → [A (100, t₀), B (100, t₁), C (10, t₂)]

  A: demand = 8;  granted = min(8, 22) = 8  → ssn_desired[A] = 8,  remaining = 14
  B: demand = 6;  granted = min(6, 14) = 6  → ssn_desired[B] = 6,  remaining = 8
  C: demand = 20; granted = min(20, 8) = 8  → ssn_desired[C] = 8,  remaining = 0

  Σ ssn_desired = 22 = total cluster cpu (= memory) ✓
  max_needy_priority = 100

is_underused():
  A: priority=100 == max_needy_priority → ssn_allocated(0) < ssn_desired(8)  → Some(true)
  B: priority=100 == max_needy_priority → ssn_allocated(0) < ssn_desired(6)  → Some(true)
  C: priority=10  <  max_needy_priority → Some(false)  ← BLOCKED, even though C
                                                        was granted 8 in step 2

AllocateAction iterates [A, B, C]:
  A: underused → fill ssn_desired=8  → ssn_allocated[A] = 8
  B: underused → fill ssn_desired=6  → ssn_allocated[B] = 6
  C: blocked by PriorityPlugin → skip
```

**Case 2 — Cluster is contended (cluster `cpu = memory = 4`, `gpu = 0`):**

Same three sessions, smaller cluster.

```
Cluster: cpu = memory = 4

PriorityPlugin.setup():
  Sort: [A (100, t₀), B (100, t₁), C (10, t₂)]

  A: demand = 8;  granted = min(8, 4) = 4  → ssn_desired[A] = 4,  remaining = 0
  B: demand = 6;  granted = min(6, 0) = 0  → ssn_desired[B] = 0,  remaining = 0
  C: demand = 20; granted = min(20, 0) = 0 → ssn_desired[C] = 0,  remaining = 0

  Σ ssn_desired = 4 = total cluster cpu (= memory) ✓ (capacity binds; demand exceeds cluster)
  max_needy_priority = 100

is_underused():
  A: priority=100 == max_needy_priority → 0 < 4  → Some(true)
  B: priority=100 == max_needy_priority → 0 == 0 → None → next plugin decides
                                                          (DRF's target share
                                                          for B is also bounded
                                                          by the cluster.)
  C: priority=10  <  max_needy_priority → Some(false)

Result:
  A receives all 4 cluster cpu (and memory).
  B (same priority as A but created later) waits until A drains.
  C waits behind both A and B.
```

**Key points:**

- `priority` is the primary sort key.
- `creation_time ascending` resolves equal-priority ties — earlier sessions are filled first (Case 1 fills A before B; Case 2 starves B until A completes).
- `Σ ssn_desired ≤ total` per-field holds by construction, with equality once cluster capacity is the binding constraint (Case 1 and Case 2).
- `ssn_allocated` is initialized in step 3 of `setup()` from currently-bound executors and tracked thereafter by the existing executor / session callbacks — that update path is unchanged.

### System Considerations

**Performance:**

| Aspect             | Impact                                                              |
| ------------------ | ------------------------------------------------------------------- |
| `setup()` overhead | O(n) scan of open sessions; negligible for expected session counts  |
| `ssn_order_fn()`   | O(1) hash lookup per comparison; no sorting overhead added          |
| `is_underused()`   | O(1) integer comparison per call                                    |
| Overall            | No measurable impact on scheduling cycle latency                    |

**Scalability:**

The `ssn_priority` map grows linearly with the number of open sessions. Session counts are expected to be in the hundreds to low thousands, well within hash map performance bounds.

**Reliability:**

- Sessions at the same priority level retain full downstream-plugin (DRF) fairness guarantees; priority introduces no new unfairness within a tier.
- If all sessions have `priority = 0` (default), `max_needy_priority = 0`, PriorityPlugin returns `None` for all calls, and scheduling behavior is determined entirely by the downstream plugins.
- Priority starvation is by design: an operator setting lower priority accepts that their workload yields to higher-priority sessions.

**Resource Usage:**

| Resource | Per-session overhead |
| -------- | -------------------- |
| Memory   | 4 bytes (`priority: u32`) in `SessionInfo` + 4 bytes in `ssn_priority` map entry |
| Storage  | 4 bytes per session row in SQLite |

**Security:**

- Priority values are user-supplied via the API. High-priority sessions can starve low-priority ones indefinitely.
- Operators should document and enforce priority conventions (e.g., reserving priority > 100 for production workloads) through access control at the application registration layer or external policy tooling.

**Observability:**

- Log the effective priority ordering and `max_needy_priority` at the start of each scheduling cycle:
  ```
  [PriorityPlugin] max_needy_priority=100, session order: A(100) ≥ B(100) > C(10)
  ```
- Log when a session is blocked by priority:
  ```
  [PriorityPlugin] Session C (priority=10) blocked: needy session at priority=100 exists
  ```

**Operational:**

- No new deployment steps. `PriorityPlugin` is always registered.
- Existing clusters upgrade transparently: all sessions start at `priority = 0`, behavior is unchanged until users explicitly set non-zero priorities.
- Rollback: removing `PriorityPlugin` from the registry reverts ordering decisions entirely to the downstream plugin chain (DRF, Gang). The `priority` column in SQLite remains but is ignored.

### Dependencies

| Dependency | Type | Notes |
| ---------- | ---- | ----- |
| `common/src/apis/types.rs` | Internal | Session and SessionAttributes structs |
| `session_manager/src/model/mod.rs` | Internal | SessionInfo (scheduler snapshot type) |
| `session_manager/src/scheduler/plugins/mod.rs` | Internal | Plugin trait, PluginManager, registration |
| `session_manager/src/scheduler/plugins/gang.rs` | Internal | GangPlugin (unchanged; orthogonal) |
| SQLite | External | Session storage; `ALTER TABLE` for `priority` column |

---

## 4. Use Cases

### Basic Use Cases

**Example 1: Production vs. Development Workloads**

**Description:** An organization runs production inference sessions alongside developer experiments. Production sessions must receive cluster resources first.

**Setup:**

```bash
# Production session (high priority)
flmctl create --app llm-inference --resreq cpu=4,mem=16g,gpu=1 --priority 100

# Developer experiment (default priority; uses cluster default resreq)
flmctl create --app dev-experiment
```

**Workflow:**
1. Cluster has 8 CPU (and matching memory/gpu) available
2. `llm-inference` (priority=100) is needy: `max_needy_priority = 100`
3. `dev-experiment` (priority=0) is blocked: `0 < 100`
4. All 8 CPU go to `llm-inference`
5. Once `llm-inference` has no more pending tasks, `dev-experiment` can receive resources

**Outcome:**

| Session | Priority | Allocated | Status |
| ------- | -------- | --------- | ------ |
| inference-001 | 100 | 8 CPU (2 executors × cpu=4) | Active — all available resources |
| experiment-xyz | 0 | 0 | Blocked until inference satisfied |

---

**Example 2: Multi-Tier Priority Scheduling**

**Description:** Three applications share a cluster with a tiered SLA model.

```bash
# Tier 1: SLA-bound inference (highest priority)
flmctl create --app sla-inference --resreq cpu=4,mem=16g,gpu=1 --priority 200

# Tier 2: Regular production training
flmctl create --app prod-training --resreq cpu=2,mem=8g,gpu=0 --priority 100

# Tier 3: Best-effort preprocessing (lowest priority; uses cluster default resreq)
flmctl create --app batch-preprocess
```

```
Cluster: cpu = 16 total

Priority ordering:
  sla-inference(200) → prod-training(100) → batch-preprocess(0)

Allocation:
  sla-inference:    8 CPU (satisfied: 2 executors × cpu=4)
  prod-training:    8 CPU (satisfied with remaining capacity)
  batch-preprocess: 0 CPU (blocked: both higher-priority sessions still needy)

After sla-inference tasks complete:
  max_needy_priority = 100 (prod-training still pending)
  batch-preprocess still blocked until prod-training satisfied
```

---

**Example 3: Equal-Priority Fair Sharing (Unchanged Behavior)**

**Description:** Two sessions with the same priority share resources via the downstream DRF plugin. Priority has no effect.

```bash
flmctl create --app app-a --resreq cpu=2,mem=8g --priority 50
flmctl create --app app-b --resreq cpu=2,mem=8g --priority 50
```

```
Cluster: cpu = 8, both sessions need cpu = 8

PriorityPlugin.ssn_order_fn → None (same priority)
DRF decides: app-a and app-b share resources by dominant resource fraction
Both get equal resources — identical to the downstream plugin behaviour
```

### Advanced Use Cases

**Example 4: Priority with Gang/Batch Scheduling**

**Description:** Combine priority with gang scheduling for multi-node tensor-parallel inference.

```bash
# High-priority multi-node inference (gang of 4)
flmctl create --app llm-inference --resreq cpu=4,mem=16g,gpu=1 --batch-size 4 --priority 100

# Low-priority single-node batch jobs
flmctl create --app batch-work --priority 0
```

```
Cluster: cpu = 20

PriorityPlugin: llm-inference (priority=100) is needy → max_needy_priority=100
  batch-work (priority=0) is blocked

AllocateAction:
  llm-inference: GangPlugin requires batches of 4 executors × cpu=4 = 16 CPU
  Statement pipelines 4 executors → is_ready() true → commit
  llm-inference gets 16 CPU in one complete batch

  batch-work: is_underused → Some(false) from PriorityPlugin → skip

Remaining 4 CPU idle (cannot form another batch for llm-inference). Once
llm-inference pending=0, batch-work becomes eligible.
```

---

## 5. References

### Related Documents

- [RFE400 - Batch Support in Session](../RFE400-batch-session/FS.md)
- [RFE408 - Enhance Fairshare for batch_size](../RFE408-fairshare-batch-size/FS.md) (historical; fairshare plugin removed)

### External References

- [Fixed-priority Pre-emptive Scheduling (Wikipedia)](https://en.wikipedia.org/wiki/Fixed-priority_pre-emptive_scheduling)
- [Kubernetes Pod Priority and Preemption](https://kubernetes.io/docs/concepts/scheduling-eviction/pod-priority-preemption/)
- [Volcano Queue Priority](https://volcano.sh/en/docs/queue/)

### Implementation References

| File | Description |
| ---- | ----------- |
| `rpc/protos/types.proto` | SessionSpec proto definition — add `priority = 8` |
| `common/src/apis/types.rs` | Session and SessionAttributes — add `priority` field |
| `session_manager/src/model/mod.rs` | SessionInfo — add `priority` field |
| `session_manager/src/scheduler/plugins/ty.rs` | **NEW** PriorityPlugin implementation |
| `session_manager/src/scheduler/plugins/mod.rs` | Plugin trait, PluginManager, registration order |
| `session_manager/src/scheduler/plugins/gang.rs` | GangPlugin (unchanged) |
| `session_manager/src/scheduler/actions/allocate.rs` | AllocateAction (unchanged) |
| `session_manager/src/scheduler/actions/dispatch.rs` | DispatchAction (unchanged) |
| `session_manager/src/apiserver/frontend.rs` | Frontend — pass `priority` from proto to attributes |
| `flmctl/src/create.rs` | Session creation — add `priority` parameter |
| `flmctl/src/main.rs` | CLI argument parsing — add `--priority` flag |

---

## 6. Design Decisions

| Decision | Rationale |
| -------- | --------- |
| **Higher `priority` value = higher priority** | Consistent with Kubernetes `PriorityClass.value` and Volcano queue priority. Allows future expansion (e.g., system sessions at priority > 1000, user session–999). |
| **PriorityPlugin as a separate plugin, not a modification of the proportional-share plugin** | Keeps the proportional-share / DRF plugin focused on resource fairness. Priority is an independent scheduling dimension that composes with, rather than replaces, the downstream distribution. |
| **Plugin consultation order: Priority → DRF → Gang** | `ssn_order_fn` returns the first non-`None` result. PriorityPlugin gives a definitive answer when priorities differ; DRF breaks ties within a priority level; GangPlugin has no ordering opinion. This ensures priority is the primary sort key globally. |
| **Blocking via `is_underused = Some(false)`, not preemption** | V1 focuses on controlling new resource allocation. Preemption (reclaiming executors from running low-priority sessions) requires executor lifecycle management, session state coordination, and policy decisions about partial reclaim — deferred to a future RFE. |
| **"Needy" criterion: `pending > 0`** | A session with no pending tasks cannot benefit from additional executors regardless of `allocated vs. deserved`. Using task backlog directly avoids dependency on the downstream plugin's internal `deserved` computation while correctly capturing whether the session needs more resources. |
| **Default `priority = 0`** | Proto3 default for `uint32` is `0`. All existing sessions automatically start at the lowest priority without any migration or explicit opt-in. Clusters that don't use the priority feature are unaffected. |
| **Global priority across applications** | Priority is a per-session attribute independent of application. Sessions from different applications compete globally. This is the natural consequence of `ssn_order_fn` acting on all open sessions in a single scheduler snapshot. |
| **No preemption in V1** | Preemption adds significant complexity: partial batch reclaim must respect gang constraints, executor teardown has latency, and priority inversion must be avoided. A dedicated follow-on RFE can add `is_preemptible` logic to `PriorityPlugin` once the simpler ordering semantics are validated in production. |
| **PriorityPlugin owns the cluster-capacity cap on `ssn_desired`** | The previous behaviour computed each session's `desired` independently of cluster size, allowing `Σ desired > cluster total` (the "deserved-based path allocated more than the cluster could provide" symptom). Moving the cap into `PriorityPlugin::setup` — as a single priority-ordered distribution loop bounded by the per-resource cluster total — makes capacity an explicit invariant by construction. Within-tier fairness is delegated to the downstream plugin (DRF); only the source of `ssn_desired` changes. |
| **Within-tier tiebreaker: creation time ascending (earlier first)** | When two sessions share a priority, the one that has been waiting longer should be filled first. This reduces head-of-line latency for established sessions, is deterministic across cycles, and reflects user intuition (FIFO within priority). Session IDs are not used because they are not ordered by submission time. |
| **`ssn_allocated` update process is unchanged** | The runtime adjustments via `on_executor_allocate` / `on_executor_unallocate` / `on_executor_pipeline` / `on_executor_discard` / `on_session_bind` / `on_session_unbind` already correctly reflect bind/release events. This RFE only changes the *initial value* of `ssn_allocated` (computed in `setup()` from the snapshot's bound executors); per-event updates after `setup()` keep their existing implementation. |
| **`ssn_desired` / `ssn_allocated` typed as `ResourceRequirement`** | Sessions request specific cpu/memory/gpu via `resreq` (RFE433); after the slots-cleanup refactor this is the only way to describe per-task resources. Per-resource accounting lets the cluster cap cpu, memory, and gpu independently — granting cpu-bound sessions cpu without exhausting gpu capacity (or vice versa). |
| **Effective per-task resreq comes directly from `ssn.resreq`** | The apiserver's `resolve_session_resreq` always populates a concrete `ResourceRequirement` before sessions reach the scheduler (explicit client value → `cluster.resreq` default → hardcoded `cpu=1,mem=1g,gpu=0` fallback). The scheduler therefore `.expect`s `ssn.resreq` and caches it in `ssn_unit` so lifecycle callbacks can update `ssn_allocated` without re-deriving it. |
| **`is_underused` uses any-field-less semantics** | A session whose cpu is filled but whose memory is still short is genuinely under-served and should keep allocating. Strict-less (all fields below desired) would never fire for sessions with `gpu = 0`, since `0 < 0` is false in the gpu dimension; any-field-less correctly tracks "still has unmet demand in some resource". |

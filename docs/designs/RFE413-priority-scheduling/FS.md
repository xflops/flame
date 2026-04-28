# Design: Priority-Based Scheduling

**Status:** Draft  
**Author:** TBD  
**Created:** 2026-04-22  
**Issue:** [#413](https://github.com/xflops/flame/issues/413)

---

## 1. Motivation

### Background

The Flame scheduler currently supports **fairshare scheduling** (RFE400/RFE408), which distributes cluster resources proportionally across sessions based on their demand. While fairshare is appropriate for general equal-weight workloads, it does not support explicit prioritization:

- All sessions compete proportionally with no priority ordering
- There is no way to express that some workloads are more time-critical than others
- Low-priority sessions can consume resources that high-priority sessions need

In practice, organizations run diverse workloads with significantly different business importance: production inference services are more critical than development experiments; SLA-bound jobs must complete before exploratory batch processing. The current scheduler cannot express or enforce these preferences.

### Current Limitations

1. **No priority field**: Sessions have no `priority` attribute; all sessions are treated as equally important regardless of their business criticality.
2. **Cross-application ordering by ratio only**: FairShare's `ssn_order_fn` orders sessions by their `allocated/deserved` ratio, which reflects usage balance but not user-specified importance.
3. **Low-priority sessions can consume resources high-priority sessions need**: When cluster resources are limited, fairshare may distribute resources to low-priority sessions at the expense of high-priority ones waiting to be scheduled.

### Problem Example

```
Cluster: 8 slots total, all idle

Session A (critical inference): slots=4, pending=8, priority=unset → 0
Session B (batch experiment):   slots=1, pending=32, priority=unset → 0

Current FairShare behavior:
  Session A deserved: ~5 slots
  Session B deserved: ~3 slots
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

**Scheduling policy**: Priority-based scheduling is always active when the `PriorityPlugin` is registered. No configuration flag is required to enable it; the plugin is registered by default alongside `FairShare` and `GangPlugin`. When all sessions share the same priority, scheduling behavior is identical to pre-existing fairshare behavior.

### API

**Proto Changes (`rpc/protos/types.proto`):**

```protobuf
message SessionSpec {
  string application = 2;
  uint32 slots = 3;
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  uint32 batch_size = 7;

  // NEW: Session priority (default: 0, higher value = higher priority)
  uint32 priority = 8;
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
flmctl create --app <APPLICATION> --slots <SLOTS> [--batch-size <BATCH_SIZE>]

# Updated usage
flmctl create --app <APPLICATION> --slots <SLOTS> [--batch-size <BATCH_SIZE>] [--priority <PRIORITY>]
```

**New flag:**

| Flag         | Short | Type   | Default | Description                           |
| ------------ | ----- | ------ | ------- | ------------------------------------- |
| `--priority` | `-p`  | `u32`  | `0`     | Session priority (higher = more important) |

**Usage examples:**

```bash
# Create a high-priority production session
flmctl create --app llm-inference --slots 4 --priority 100

# Create a medium-priority training session
flmctl create --app model-training --slots 8 --priority 50

# Create a low-priority batch session (default priority)
flmctl create --app data-preprocessing --slots 1

# Combine priority with batch/gang scheduling
flmctl create --app llm-inference --slots 4 --batch-size 4 --priority 100
```

**Session listing (`flmctl list -s`):**

```
ID                State   App               Slots  Batch  Priority  Pending  Running  Succeed  Failed
inference-001     Open    llm-inference     4      1      100       16       4        32       0
training-abcd     Open    model-training    8      1      50        8        2        0        0
preprocess-xyz    Open    data-preprocess   1      1      0         100      1        50       0
```

### Other Interfaces

**Python SDK (`flamepy`):**

```python
# open_session accepts priority (passed through to SessionSpec)
session = flame.open_session(
    session_id="inference-001",
    application="llm-inference",
    slots=4,
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
- Sessions with equal priority are ordered by fairshare (proportional allocation) exactly as before.
- A session with pending tasks at a given priority level blocks all sessions at lower priority levels from receiving new resources, even if those lower-priority sessions could use idle resources that the high-priority session cannot reach.

### Feature Interaction

**Related Features:**

| Feature | Interaction |
| ------- | ----------- |
| **FairShare (RFE400/RFE408)** | Priority ordering overlays FairShare's proportional ordering. Within the same priority level, FairShare determines the `deserved` distribution and session ordering by `allocated/deserved` ratio. |
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
| `session_manager/src/scheduler/plugins/mod.rs` | Register `PriorityPlugin`; ensure it is consulted before `FairShare` in `ssn_order_fn` |

**Compatibility:**

- **Fully backward compatible.** All existing sessions default to `priority = 0`. When all sessions share the same priority, PriorityPlugin returns `None` for both `ssn_order_fn` and `is_underused`, leaving behavior identical to the pre-existing fairshare scheduling.
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
│  │  │  PriorityPlugin  │  │    FairShare      │  │   GangPlugin     │  │    │
│  │  │     (NEW)        │  │                   │  │                  │  │    │
│  │  │                  │  │                   │  │                  │  │    │
│  │  │ setup():         │  │ setup():          │  │ setup():         │  │    │
│  │  │  compute         │  │  compute          │  │  track batch     │  │    │
│  │  │  max_needy_prio  │  │  deserved/        │  │  state           │  │    │
│  │  │  among sessions  │  │  allocated        │  │                  │  │    │
│  │  │  with pending    │  │                   │  │                  │  │    │
│  │  │  tasks           │  │                   │  │                  │  │    │
│  │  │                  │  │                   │  │                  │  │    │
│  │  │ ssn_order_fn():  │  │ ssn_order_fn():   │  │   (no opinion)   │  │    │
│  │  │  sort descending │  │  sort by alloc/   │  │                  │  │    │
│  │  │  by priority     │  │  deserved ratio   │  │                  │  │    │
│  │  │  (primary key)   │  │  (tiebreaker)     │  │                  │  │    │
│  │  │                  │  │                   │  │                  │  │    │
│  │  │ is_underused():  │  │ is_underused():   │  │ is_underused():  │  │    │
│  │  │  Some(false) if  │  │  check alloc      │  │  check batch     │  │    │
│  │  │  lower priority  │  │  vs deserved      │  │  capacity        │  │    │
│  │  │  than max_needy  │  │                   │  │                  │  │    │
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
│   │   FairShare ratio   │                        │   sessions blocked  │    │
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
  uint32 slots = 3;
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;
  uint32 batch_size = 7;
  uint32 priority = 8;  // NEW: default 0 (lowest priority)
}
```

**`common/src/apis/types.rs`:**

```rust
pub struct SessionAttributes {
    pub id: SessionID,
    pub application: String,
    pub slots: u32,
    pub common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,  // NEW: default 0
}

impl Default for SessionAttributes {
    fn default() -> Self {
        Self {
            id: String::new(),
            application: String::new(),
            slots: 1,
            common_data: None,
            min_instances: 0,
            max_instances: None,
            batch_size: 1,
            priority: 0,  // NEW
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
    pub slots: u32,
    pub tasks_status: HashMap<TaskState, i32>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub state: SessionState,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,  // NEW
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
        slots: spec.slots,
        common_data: spec.common_data.clone(),
        min_instances: spec.min_instances,
        max_instances: spec.max_instances,
        batch_size: spec.batch_size.max(1),
        priority: spec.priority,  // NEW
    }
}
```

#### 4. PriorityPlugin

**Location:** `session_manager/src/scheduler/plugins/priority.rs`

The `PriorityPlugin` implements the `Plugin` trait. It is stateful across a scheduling cycle: `setup()` computes the global `max_needy_priority`, which is then used by `ssn_order_fn` and `is_underused` during that cycle.

```rust
pub struct PriorityPlugin {
    /// Maximum priority among sessions that are "needy" (have pending tasks).
    /// Computed in setup(); used during ssn_order_fn and is_underused.
    max_needy_priority: u32,
    /// Priority for each open session, keyed by session ID.
    ssn_priority: HashMap<SessionID, u32>,
}

impl PriorityPlugin {
    pub fn new() -> Self {
        Self {
            max_needy_priority: 0,
            ssn_priority: HashMap::new(),
        }
    }
}

impl Plugin for PriorityPlugin {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_priority.clear();
        self.max_needy_priority = 0;

        for ssn in ss.sessions.values() {
            if ssn.state != SessionState::Open {
                continue;
            }

            let priority = ssn.priority;
            self.ssn_priority.insert(ssn.id.clone(), priority);

            // A session is "needy" if it has pending tasks.
            // A pending task implies the session can use more executors.
            let pending = ssn.tasks_status
                .get(&TaskState::Pending)
                .copied()
                .unwrap_or(0);

            if pending > 0 && priority > self.max_needy_priority {
                self.max_needy_priority = priority;
            }
        }

        Ok(())
    }

    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let p1 = self.ssn_priority.get(&s1.id).copied().unwrap_or(0);
        let p2 = self.ssn_priority.get(&s2.id).copied().unwrap_or(0);

        if p1 != p2 {
            // Higher priority sessions come first (descending order).
            // p2.cmp(&p1) reverses the natural ascending order.
            Some(p2.cmp(&p1))
        } else {
            // Equal priority: no opinion; defer to FairShare for ratio-based tiebreaking.
            None
        }
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let ssn = lock_ptr!(ssn).ok()?;
        let priority = self.ssn_priority.get(&ssn.id).copied().unwrap_or(0);

        if priority < self.max_needy_priority {
            // A higher-priority session is still needy.
            // Block this session from receiving new resources.
            Some(false)
        } else {
            // This session is at or above the highest needy priority.
            // Defer to FairShare and GangPlugin to determine underuse.
            None
        }
    }

    // All other Plugin methods return their default (None / no-op).
    // PriorityPlugin does not influence node ordering, preemptibility,
    // executor availability, allocatability, reclaimability, gang readiness,
    // or event callbacks.
}
```

#### 5. PluginManager Registration and ssn_order_fn Chain

**`session_manager/src/scheduler/plugins/mod.rs`:**

`PriorityPlugin` is registered before `FairShare` in the plugin list. The `PluginManager::ssn_order_fn` walks plugins in registration order and returns the first non-`None` result:

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
| 2 | `FairShare` | Returns `Some(ord)` based on `allocated/deserved` ratio |
| 3 | `GangPlugin` | Returns `None` (no ordering opinion) |

This chain ensures priority is the primary sort key, with FairShare providing tiebreaking within a priority level.

The `is_underused` aggregation rule is **ANY** (any plugin returning `Some(true)` makes a session underused). `PriorityPlugin` returns `Some(false)` for blocked lower-priority sessions, and `None` for sessions at or above the needy priority tier. The `None` return defers to FairShare and GangPlugin, preserving all existing underuse logic for unblocked sessions.

#### 6. flmctl Changes

**`flmctl/src/create.rs`:**

```rust
pub async fn run(
    ctx: &FlameContext,
    app: &str,
    slots: &u32,
    batch_size: &u32,
    priority: &u32,  // NEW parameter
) -> Result<(), Box<dyn Error>> {
    let conn = build_connection(ctx).await?;
    let attr = SessionAttributes {
        id: format!("{app}-{}", stdng::rand::short_name()),
        application: app.to_owned(),
        slots: *slots,
        common_data: None,
        min_instances: 0,
        max_instances: None,
        batch_size: *batch_size,
        priority: *priority,  // NEW
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

    /// Slots per executor
    #[arg(short = 's', long)]
    slots: u32,

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

| Field                | Type                      | Description                                                                 |
| -------------------- | ------------------------- | --------------------------------------------------------------------------- |
| `max_needy_priority` | `u32`                     | Highest priority among sessions with pending tasks; computed in `setup()`   |
| `ssn_priority`       | `HashMap<SessionID, u32>` | Priority for each open session; populated in `setup()`; consulted in order functions |

**`SessionInfo` extension:**

| Field      | Type   | Default | Description                              |
| ---------- | ------ | ------- | ---------------------------------------- |
| `priority` | `u32`  | `0`     | Session priority; higher = more important |

### Algorithms

#### Priority Ordering (ssn_order_fn)

```
Input: two sessions s1, s2

PriorityPlugin.ssn_order_fn(s1, s2):
  p1 = ssn_priority[s1.id]  (default 0)
  p2 = ssn_priority[s2.id]  (default 0)

  if p1 > p2 → return Some(Less)     (s1 comes first in sorted order)
  if p1 < p2 → return Some(Greater)  (s2 comes first)
  if p1 == p2 → return None          (defer to FairShare)

PluginManager chain (first non-None wins):
  PriorityPlugin → FairShare → GangPlugin → Ordering::Equal
```

#### Priority Blocking (is_underused)

```
Computed in setup():
  max_needy_priority = max{ ssn.priority | ssn has pending tasks > 0 }

For each session in is_underused(ssn):
  priority = ssn_priority[ssn.id]

  if priority < max_needy_priority:
    return Some(false)   → blocked; not considered for new allocation
  else:
    return None          → let FairShare and GangPlugin decide

Aggregation (ANY semantics):
  is_underused(ssn) = PriorityPlugin OR FairShare OR GangPlugin
  If PriorityPlugin returns Some(false), session is not underused
  regardless of what FairShare/GangPlugin return.
```

**Why `pending > 0` as the "needy" criterion:**

A session with pending tasks has work it cannot yet run — it can benefit from additional executors. A session with zero pending tasks is either idle or satisfied; it should not block lower-priority sessions even if it holds fewer executors than its `deserved` allocation.

#### FairShare `desired` Calculation (updated for priority scheduling)

With priority scheduling active, each session's `desired` equals exactly one scheduling unit — the resources it was configured to use — rather than scaling with its task backlog:

```
desired = batch_size × slots   (when task_count > 0)
desired = 0                    (when task_count == 0, no work to do)

Then apply:
  desired = min(desired, max_instances × slots)   # cap at max_instances if set
  desired = max(desired, min_instances × slots)   # floor at min_instances guarantee
```

**Old formula (removed):** `desired = floor(task_count / batch_size) × batch_size × slots`

The old formula caused sessions with large backlogs to claim proportionally more resources than sessions with small backlogs. With priority scheduling, backlog size must not influence resource allocation — that role belongs to the PriorityPlugin's ordering and blocking logic. Each session claims its one scheduling unit; priority determines which session receives it.

#### Example Walkthrough

All sessions below use `batch_size=1`, so `desired = 1 × slots = slots`.

```
Cluster: 6 slots total, all idle

Sessions:
  Session A: priority=100, slots=2, pending=8   → desired=2
  Session B: priority=100, slots=2, pending=4   → desired=2
  Session C: priority=10,  slots=2, pending=12  → desired=2

FairShare.setup():
  remaining = 6 (no min_instances, no executors yet)
  Distribute: A gets 2, B gets 2, C gets 2 (each desired=2, cluster has enough)
  A: desired=2, deserved=2, allocated=0
  B: desired=2, deserved=2, allocated=0
  C: desired=2, deserved=2, allocated=0

PriorityPlugin.setup():
  A: pending=8 > 0, priority=100 → max_needy_priority = 100
  B: pending=4 > 0, priority=100 → max_needy_priority = 100 (unchanged)
  C: pending=12 > 0, priority=10 → 10 < 100, skip
  Result: max_needy_priority = 100

ssn_order_fn():
  A vs B: same priority (100) → None → FairShare breaks tie
    FairShare: A ratio=0/2=0.0, B ratio=0/2=0.0 → equal → Ordering::Equal
  A vs C: 100 > 10 → Some(Less) → A before C
  B vs C: 100 > 10 → Some(Less) → B before C
  Final order: [A, B, C]  (A and B equal within their tier)

is_underused():
  A: priority=100 == max_needy_priority(100) → None → FairShare: 0 < 2 → true
  B: priority=100 == max_needy_priority(100) → None → FairShare: 0 < 2 → true
  C: priority=10  <  max_needy_priority(100) → Some(false)  ← BLOCKED

AllocateAction iterates [A, B, C]:
  A: underused → allocate 2 slots (1 executor) → allocated=2
  B: underused → allocate 2 slots (1 executor) → allocated=2
  C: blocked by PriorityPlugin → skip

Result:
  A: 1 executor (2 slots), processes its 8 pending tasks one at a time
  B: 1 executor (2 slots), processes its 4 pending tasks one at a time
  C: 0 executors — blocked until both A and B drain their pending queues

Key point: C has 12 pending tasks but desired=2 (= slots), the same as A and B.
The large backlog does not give C any advantage in the FairShare calculation.
Priority alone determines which sessions receive resources.
```

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

- Sessions at the same priority level retain full fairshare guarantees; priority introduces no new unfairness within a tier.
- If all sessions have `priority = 0` (default), `max_needy_priority = 0`, PriorityPlugin returns `None` for all calls, and scheduling behavior is identical to pre-existing fairshare.
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
- Rollback: removing `PriorityPlugin` from the registry reverts to pure fairshare behavior. The `priority` column in SQLite remains but is ignored.

### Dependencies

| Dependency | Type | Notes |
| ---------- | ---- | ----- |
| `common/src/apis/types.rs` | Internal | Session and SessionAttributes structs |
| `session_manager/src/model/mod.rs` | Internal | SessionInfo (scheduler snapshot type) |
| `session_manager/src/scheduler/plugins/mod.rs` | Internal | Plugin trait, PluginManager, registration |
| `session_manager/src/scheduler/plugins/fairshare.rs` | Internal | FairShare (unchanged; priority is additive) |
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
flmctl create --app llm-inference --slots 4 --priority 100

# Developer experiment (default priority)
flmctl create --app dev-experiment --slots 1
```

**Workflow:**
1. Cluster has 8 available slots
2. `llm-inference` (priority=100) is needy: `max_needy_priority = 100`
3. `dev-experiment` (priority=0) is blocked: `0 < 100`
4. All 8 slots go to `llm-inference`
5. Once `llm-inference` has no more pending tasks, `dev-experiment` can receive resources

**Outcome:**

| Session | Priority | Allocated | Status |
| ------- | -------- | --------- | ------ |
| inference-001 | 100 | 8 slots | Active — all available resources |
| experiment-xyz | 0 | 0 slots | Blocked until inference satisfied |

---

**Example 2: Multi-Tier Priority Scheduling**

**Description:** Three applications share a cluster with a tiered SLA model.

```bash
# Tier 1: SLA-bound inference (highest priority)
flmctl create --app sla-inference --slots 4 --priority 200

# Tier 2: Regular production training
flmctl create --app prod-trainin--slots 2 --priority 100

# Tier 3: Best-effort preprocessing (lowest priority)
flmctl create --app batch-preprocess --slots 1
```

```
Cluster: 16 slots total

Priority ordering:
  sla-inference(200) → prod-training(100) → batch-preprocess(0)

Allocation:
  sla-inference:   8 slots (satisfied: 2 executors × 4 slots)
  prod-training:   8 slots (satisfied with remaining capacity)
  batch-preprocess: 0 slots (blocked: both higher-priority sessions still needy)

After sla-inference tasks complete:
  max_npriority = 100 (prod-training still pending)
  batch-preprocess still blocked until prod-training satisfied
```

---

**Example 3: Equal-Priority Fair Sharing (Unchanged Behavior)**

**Description:** Two sessions with the same priority share resources via FairShare. Priority has no effect.

```bash
flmctl create --app app-a --slots 2 --priority 50
flmctl create --app app-b --slots 2 --priority 50
```

```
Cluster: 8 slots, both sessions need 8 slots

PriorityPlugin.ssn_order_fn → None (same priority)
Fairare decides: app-a deserved=4, app-b deserved=4
Both get equal resources — identical to pre-existing fairshare behavior
```

### Advanced Use Cases

**Example 4: Priority with Gang/Batch Scheduling**

**Description:** Combine priority with gang scheduling for multi-node tensor-parallel inference.

```bash
# High-priority multi-node inference (gang of 4)
flmctl create --app llm-inference --slots 4 --batch-size 4 --priority 100

# Low-priority single-node batch jobs
flmctl create --app batch-work --slots 1 priority 0
```

```
Cluster: 20 slots

PriorityPlugin: llm-inference (priority=100) is needy → max_needy_priority=100
  batch-work (priority=0) is blocked

AllocateAction:
  llm-inference: GangPlugin requires batches of 4 executors × 4 slots = 16 slots
  Statement pipelines 4 executors → is_ready() true → commit
  llm-inference gets 16 slots in one complete batch

  batch-work: is_underused → Some(false) from PriorityPlugin → skip

Remaining 4 slots idle (cannot form another batch for llm-inferenm-inference pending=0, batch-work becomes eligible
```

---

## 5. References

### Related Documents

- [RFE400 - Batch Support in Session](../RFE400-batch-session/FS.md)
- [RFE408 - Enhance Fairshare for batch_size](../RFE408-fairshare-batch-size/FS.md)
- [Scheduler Fairshare Design](../scheduler-fairshare-design.md)

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
| `session_manager/src/scheduler/plugins/fairshare.rs` | FairShare plugin (unchanged) |
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
| **PriorityPlugin as a separate plugin, not a FairShare modification** | Keeps FairShare focused on proportional allocation. Priority is an independent scheduling dimension that composes with, rather than replaces, fairshare distribution. |
| **Plugin consultation order: Priority → FairShare → Gang** | `ssn_order_fn` returns the first non-`None` result. PriorityPlugin gives a definitive answer when priorities differ; FairShare breaks ties within a priority level; GangPlugin has no ordering o. This ensures priority is the primary sort key globally. |
| **Blocking via `is_underused = Some(false)`, not preemption** | V1 focuses on controlling new resource allocation. Preemption (reclaiming executors from running low-priority sessions) requires executor lifecycle management, session state coordination, and policy decisions about partial reclaim — deferred to a future RFE. |
| **"Needy" criterion: `pending > 0`** | A session with no pending tasks cannot benefit from additional executors regardlesof `allocated vs. deserved`. Using task backlog directly avoids dependency on FairShare's internal `deserved` computation while correctly capturing whether the session needs more resources. |
| **Default `priority = 0`** | Proto3 default for `uint32` is `0`. All existing sessions automatically start at the lowest priority without any migration or explicit opt-in. Clusters that don't use the priority feature are unaffected. |
| **Global priority across applications** | Priority is a per-session attribute independent of application. Sessions from different applications compete globally. This is the natural consequence of `ssn_order_fn` acting on all open sessions in a single scheduler snapshot. |
| **No preemption in V1** | Preemption adds significant complexity: partial batch reclaim must respect gang constraints, executor teardown has latency, and priority inversion must be avoided. A dedicated follow-on RFE can add `is_preemptible` logic to `PriorityPlugin` once the simpler ordering semantics are validated in production. |

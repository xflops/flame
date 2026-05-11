# Design: Batch Support in Session

**Status:** Draft  
**Author:** TBD  
**Created:** 2026-04-13  
**Issue:** [#400](https://github.com/xflops/flame/issues/400)

---

## 1. Motivation

**Background:**

In some cases, multiple sub-tasks are grouped in a **batch** and must work together. A typical use case is **multi-node inference**: each inference request requires multiple nodes to handle it cooperatively (e.g., tensor parallelism, pipeline parallelism in LLM inference).

The current Flame architecture treats tasks independently within a session:
- Each executor pulls tasks individually via `launch_task()`
- Tasks are assigned to executors without coordination
- No concept of tasks that must run simultaneously across multiple executors

This limitation prevents Flame from efficiently supporting distributed workloads that require coordinated multi-node execution.

**Target:**

1. **Batch allocation**: The scheduler must allocate resources in batch size increments: `n, 2n, 3n, ...` where `n` is the batch size
2. **Session configuration**: Support batch configuration as a session-level attribute
3. **Dedicated task indexing**: Each executor in a batch should fetch only its dedicated index task (executor 0 gets task 0 in batch, executor 1 gets task 1, etc.)
4. **Backward compatibility**: Default batch size of 1 maintains current single-task-per-request behavior

---

## 2. Function Specification

### Configuration

**Session Attributes:**

| Field        | Type     | Default | Description                                                                              |
| ------------ | -------- | ------- | ---------------------------------------------------------------------------------------- |
| `batch_size` | `uint32` | `1`     | Number of executors required per batch. Tasks are distributed across executors by index. |

**Validation Rules:**
- `batch_size` must be >= 1
- `min_instances` must be a multiple of `batch_size`
- `max_instances` (if set) must be a multiple of `batch_size`

**Default Value Handling:**
- Proto3 `uint32` defaults to 0; treat `batch_size = 0` as `batch_size = 1` for backward compatibility
- Rust `SessionAttributes::default()` sets `batch_size = 1`

### API

#### Proto Changes

**types.proto:**

```protobuf
message SessionSpec {
  string application = 2;
  // Field number 3 (slots) reserved — removed in the slots-cleanup refactor; do not reuse.
  reserved 3;
  reserved "slots";
  optional bytes common_data = 4;
  uint32 min_instances = 5;
  optional uint32 max_instances = 6;

  // NEW: Batch configuration
  uint32 batch_size = 7;  // Number of executors per batch (default: 1)
  uint32 priority = 8;
  optional ResourceRequirement resreq = 9;
}

message ExecutorStatus {
  ExecutorState state = 1;
  optional string session_id = 2;
  
  // NEW: Batch index for the executor within its session
  optional uint32 batch_index = 3;  // 0 to batch_size-1
}
```

**backend.proto:**

```protobuf
message BindExecutorResponse {
  optional Application application = 1;
  optional Session session = 2;
  
  // NEW: Assigned batch index for this executor
  optional uint32 batch_index = 3;  // 0 to batch_size-1
}

message LaunchTaskResponse {
  optional Task task = 1;
  
  // NEW: Batch information
  optional uint32 batch_id = 2;     // Which batch this task belongs to
  optional uint32 batch_index = 3;  // Index within the batch (matches executor's batch_index)
}
```

### CLI

**Session Creation:**

```bash
# Create a session with batch_size=4 for multi-node inference
flmctl create session \
  --application llm-inference \
  --resreq cpu=8,mem=32g,gpu=1 \
  --batch-size 4 \
  --min-instances 4 \
  --max-instances 8
```

**Session Listing:**

```bash
# List sessions showing batch configuration
flmctl list -s

 ID              State   App           Resources                  Batch  Pending  Running  Succeed  Failed
 inference-001   Open    llm-inference cpu=8,mem=32g,gpu=1        4      8        4        12       0
```

### Scope

**In Scope:**
- Batch configuration in session attributes
- Batch-aware executor allocation (gang scheduling)
- Batch index assignment to executors
- Batch-indexed task fetching

**Out of Scope:**
- Batch-level failure handling (all-or-nothing semantics)
- Heterogeneous batches (different resource requirements per index)
- Batch affinity (keeping same executors for consecutive batches)
- Dynamic batch resizing for running sessions

**Limitations:**
- Tasks must be created in `batch_size` increments for optimal utilization
- All executors in a batch must be from the same session (no cross-session batching)
- Batch index is assigned at executor bind time and remains fixed

### Feature Interaction

**Related Features:**
- `resreq`: Per-executor resource request (replaces the deprecated `slots` field)
- `min_instances` / `max_instances`: Executor count bounds (must be multiples of `batch_size`)
- Distribution scheduling (DRF + Priority): Allocation calculation considers batch boundaries (the original FairShare plugin has been removed)

**Updates Required:**
- `Plugin trait`: Add `is_ready()`, `on_pipeline_executor()`, `on_discard_executor()` methods
- `GangPlugin`: New scheduler plugin implementing these methods for batch tracking
- `Statement`: New struct for accumulating pending allocations (pipeline/commit/discard)
- `AllocateAction`: Use Statement pattern - action is NOT aware of GangPlugin
- `Controller::launch_task()`: Batch-indexed task assignment
- Storage schemas: Add `batch_size` to sessions, `batch_index` to executors

**Design Principle: Actions Unaware of Gang Logic**

Actions use Statement to accumulate allocations. Statement calls Plugin trait callbacks (`on_pipeline_executor`, `on_discard_executor`). GangPlugin implements these callbacks to track pipelined count. Actions only call `ctx.is_ready()` - they don't know about GangPlugin.

**Compatibility:**
- Fully backward compatible: `batch_size=1` (default) maintains existing behavior
- Proto changes use new optional fields
- Existing sessions continue working without migration

---

## 3. Implementation Detail

### Architecture

```
┌────────────────────────────────────────────────────────────────────────┐
│                              Session                                   │
│  batch_size=3                                                          │
│                                                                        │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │ Tasks (batch-indexed assignment)                                │   │
│  │                                                                 │   │
│  │  Batch 0:  Task 0 → idx=0, Task 1 → idx=1, Task 2 → idx=2       │   │
│  │  Batch 1:  Task 3 → idx=0, Task 4 → idx=1, Task 5 → idx=2       │   │
│  │  Batch 2:  Task 6 → idx=0, Task 7 → idx=1, Task 8 → idx=2       │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         Scheduler                                       │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                      PluginManager                                │  │
│  │                                                                   │  │
│  │  plugins: HashMap<String, PluginPtr>                              │  │
│  │  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐    │  │
│  │  │ Priority + DRF  │  │   ShimPlugin    │  │   GangPlugin    │    │  │
│  │  │  (Plugin trait) │  │  (Plugin trait) │  │  (Plugin trait) │    │  │
│  │  │                 │  │                 │  │                 │    │  │
│  │  │ - is_underused  │  │ - is_available  │  │ - is_underused  │    │  │
│  │  │ - is_preemptible│  │                 │  │ - is_ready (NEW)│    │  │
│  │  │ - is_allocatable│  │                 │  │ - on_pipeline_  │    │  │
│  │  │ - ssn_order_fn  │  │                 │  │   executor (NEW)│    │  │
│  │  │ - node_order_fn │  │                 │  │ - on_discard_   │    │  │
│  │  │ - on_create_exec│  │                 │  │   executor (NEW)│    │  │
│  │  │ - on_session_*  │  │                 │  │ - on_create_exec│    │  │
│  │  └─────────────────┘  └─────────────────┘  └─────────────────┘    │  │
│  │                                                                   │  │
│  │  Aggregation:                                                     │  │
│  │  - is_underused() → ANY plugin true = underused                   │  │
│  │  - is_available() → ALL plugins true = available                  │  │
│  │  - is_ready() (NEW) → ALL plugins true = ready                    │  │
│  │  - on_pipeline_executor() (NEW) → notify all plugins              │  │
│  │  - on_discard_executor() (NEW) → notify all plugins               │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                    │                                    │
│                                    ▼                                    │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                       Statement (NEW)                             │  │
│  │                                                                   │  │
│  │  Accumulates pending allocations using Plugin callbacks:          │  │
│  │  - pipeline(node, ssn) → calls plugins.on_pipeline_executor()     │  │
│  │  - commit() → calls ctx.create_executor() for each                │  │
│  │  - discard() → calls plugins.on_discard_executor() for each       │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                    │                                    │
│            ┌───────────────────────┴───────────────────────┐            │
│            ▼                                               ▼            │
│  ┌─────────────────────┐                       ┌─────────────────────┐  │
│  │   AllocateAction    │                       │   DispatchAction    │  │
│  │  (no gang awareness)│                       │                     │  │
│  │                     │                       │                     │  │
│  │ 1. Create Statement │                       │ - ctx.is_underused()│  │
│  │ 2. stmt.pipeline()  │                       │ - ctx.is_available()│  │
│  │ 3. ctx.is_ready()?  │                       │ - ctx.is_ready()    │  │
│  │ 4a. Ready: commit() │                       │ - ctx.bind_session()│  │
│  │ 4b. Not: discard()  │                       │                     │  │
│  └─────────────────────┘                       └─────────────────────┘  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
        ┌────────────────┐  ┌────────────────┐  ┌────────────────┐
        │   Executor 0   │  │   Executor 1   │  │   Executor 2   │
        │  batch_idx=0   │  │  batch_idx=1   │  │  batch_idx=2   │
        │                │  │                │  │                │
        │ launch_task()  │  │ launch_task()  │  │ launch_task()  │
        │  → Task 0,3,6  │  │  → Task 1,4,7  │  │  → Task 2,5,8  │
        └────────────────┘  └────────────────┘  └────────────────┘
```

### Components

**Design Principle: Extend Existing Plugin Trait**

The existing Plugin trait is extended with a single new method `is_ready()` for gang readiness checks. No other changes to the plugin machinery. GangPlugin implements this method alongside existing trait methods.

**1. Plugin Trait (session_manager/src/scheduler/plugins/mod.rs)**

Extended with `is_ready()`:

```rust
pub trait Plugin: Send + Sync + 'static {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError>;

    // Ordering
    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> { None }
    fn node_order_fn(&self, s1: &NodeInfo, s2: &NodeInfo) -> Option<Ordering> { None }

    // Filters
    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> { None }
    fn is_preemptible(&self, ssn: &SessionInfoPtr) -> Option<bool> { None }
    fn is_available(&self, exec: &ExecutorInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> { None }
    fn is_allocatable(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Option<bool> { None }
    
    // NEW: Gang readiness check
    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> { None }

    // Events
    fn on_create_executor(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}
    fn on_session_bind(&mut self, ssn: SessionInfoPtr) {}
    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {}
    
    // NEW: Statement callbacks for batch allocation
    fn on_pipeline_executor(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}
    fn on_discard_executor(&mut self, node: NodeInfoPtr, ssn: SessionInfoPtr) {}
}
```

**2. PluginManager (session_manager/src/scheduler/plugins/mod.rs)**

Extended with `is_ready()`, `on_pipeline_executor()`, `on_discard_executor()`:

```rust
impl PluginManager {
    // ... existing methods unchanged ...
    
    /// Returns true if ALL plugins say session is ready (default: true)
    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        let plugins = lock_ptr!(self.plugins)?;
        Ok(plugins.values().all(|plugin| plugin.is_ready(ssn).unwrap_or(true)))
    }
    
    /// Called by Statement.pipeline() - notifies all plugins
    pub fn on_pipeline_executor(&self, node: NodeInfoPtr, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;
        for plugin in plugins.values_mut() {
            plugin.on_pipeline_executor(node.clone(), ssn.clone());
        }
        Ok(())
    }
    
    /// Called by Statement.discard() - notifies all plugins
    pub fn on_discard_executor(&self, node: NodeInfoPtr, ssn: SessionInfoPtr) -> Result<(), FlameError> {
        let mut plugins = lock_ptr!(self.plugins)?;
        for plugin in plugins.values_mut() {
            plugin.on_discard_executor(node.clone(), ssn.clone());
        }
        Ok(())
    }
}
```

**3. GangPlugin (session_manager/src/scheduler/plugins/gang.rs)**

Implements Plugin trait including `is_ready()`, `on_pipeline_executor()`, `on_discard_executor()`:

```rust
pub struct GangPlugin {
    ssn_state: HashMap<SessionID, GangState>,
}

struct GangState {
    batch_size: u32,
    allocated: u32,
    pipelined: u32,  // Count of reserved (but not committed) executors
    max_instances: Option<u32>,
}

impl Plugin for GangPlugin {
    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.ssn_state.clear();
        for ssn in ss.sessions.values() {
            let allocated = ss.executors.values()
                .filter(|e| e.ssn_id.as_ref() == Some(&ssn.id))
                .count() as u32;
            
            self.ssn_state.insert(ssn.id.clone(), GangState {
                batch_size: ssn.batch_size.max(1),
                allocated,
                pipelined: 0,
                max_instances: ssn.max_instances,
            });
        }
        Ok(())
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let state = self.ssn_state.get(&ssn.id)?;
        if state.batch_size <= 1 {
            return None;
        }
        if let Some(max) = state.max_instances {
            if state.allocated + state.batch_size > max {
                return Some(false);
            }
        }
        None
    }
    
    fn is_ready(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        let state = self.ssn_state.get(&ssn.id)?;
        if state.batch_size <= 1 {
            return None;  // Non-gang sessions: no opinion
        }
        // Ready when (allocated + pipelined) is a multiple of batch_size
        // This ensures we only commit complete batches
        let total = state.allocated + state.pipelined;
        Some(total > 0 && total % state.batch_size == 0)
    }

    fn on_pipeline_executor(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined += 1;
        }
    }
    
    fn on_discard_executor(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.pipelined = state.pipelined.saturating_sub(1);
        }
    }

    fn on_create_executor(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.allocated += 1;
            state.pipelined = 0;  // Clear pipelined after commit
        }
    }

    fn on_session_unbind(&mut self, ssn: SessionInfoPtr) {
        if let Some(state) = self.ssn_state.get_mut(&ssn.id) {
            state.allocated = state.allocated.saturating_sub(1);
        }
    }
}
```

**4. Context (session_manager/src/scheduler/ctx.rs)**

Extended with `is_ready()` and per-resource reserve/release helpers:

```rust
impl Context {
    // ... existing methods unchanged ...

    pub fn is_ready(&self, ssn: &SessionInfoPtr) -> Result<bool, FlameError> {
        self.plugins.is_ready(ssn)
    }

    /// Reserve one executor's worth of `ssn.resreq` on the node (in-memory, no executor created)
    pub fn reserve(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Result<(), FlameError> {
        self.snapshot.reserve(node, ssn)
    }

    /// Release a previously reserved executor's worth of `ssn.resreq`
    pub fn release(&self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Result<(), FlameError> {
        self.snapshot.release(node, ssn)
    }
}
```

**5. Statement (session_manager/src/scheduler/statement.rs) - NEW**

Statement accumulates pending allocations without committing them. It uses Plugin trait callbacks to notify plugins - actions are NOT aware of GangPlugin.

```rust
pub struct Statement {
    operations: Vec<Operation>,
    ctx: ContextPtr,
}

struct Operation {
    node: NodeInfoPtr,
    ssn: SessionInfoPtr,
}

impl Statement {
    pub fn new(ctx: ContextPtr) -> Self {
        Statement {
            operations: Vec::new(),
            ctx,
        }
    }
    
    /// Reserve resources for an executor (in-memory only, no actual creation)
    /// Calls on_pipeline_executor callback - GangPlugin tracks pipelined count
    pub fn pipeline(&mut self, node: &NodeInfoPtr, ssn: &SessionInfoPtr) -> Result<(), FlameError> {
        // Reserve `ssn.resreq` resources on node in snapshot
        self.ctx.snapshot.reserve(node, ssn)?;
        
        // Notify plugins via callback (GangPlugin increments pipelined count)
        self.ctx.plugins.on_pipeline_executor(node.clone(), ssn.clone())?;
        
        // Record operation for later commit/discard
        self.operations.push(Operation {
            node: node.clone(),
            ssn: ssn.clone(),
        });
        
        Ok(())
    }
    
    /// Commit all pending operations - actually create executors
    pub async fn commit(self) -> Result<(), FlameError> {
        for op in self.operations {
            self.ctx.create_executor(&op.node, &op.ssn).await?;
        }
        Ok(())
    }
    
    /// Discard all pending operations - release reserved resources
    /// Calls on_discard_executor callback - GangPlugin decrements pipelined count
    pub fn discard(self) -> Result<(), FlameError> {
        // Reverse order to properly unwind
        for op in self.operations.into_iter().rev() {
            // Release `ssn.resreq` resources on node
            self.ctx.snapshot.release(&op.node, &op.ssn)?;
            
            // Notify plugins via callback (GangPlugin decrements pipelined count)
            self.ctx.plugins.on_discard_executor(op.node, op.ssn)?;
        }
        Ok(())
    }
}
```

**6. Session (common/src/apis/types.rs)**
- Add `batch_size: u32` field to `Session` struct
- Add `batch_size: u32` field to `SessionAttributes` struct (default: 1)
- Modify `pop_pending_task()` to accept `batch_index` and `batch_size` parameters

**7. SessionInfo (session_manager/src/model/mod.rs)**
- Add `batch_size: u32` field to `SessionInfo` struct (for scheduler snapshot)

**8. Executor (session_manager/src/model/mod.rs)**
- Add `batch_index: Option<u32>` field to `Executor` and `ExecutorInfo`

**9. Controller (session_manager/src/controller/mod.rs)**
- Modify `launch_task()` to use batch-indexed task assignment

### Data Structures

**Session Extension:**

```rust
// common/src/apis/types.rs

pub struct SessionAttributes {
    pub id: SessionID,
    pub application: String,
    pub common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,  // NEW: default 1
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
            batch_size: 1,  // NEW: default to 1 for backward compatibility
            resreq: None,
        }
    }
}

pub struct Session {
    // ... existing fields ...
    pub batch_size: u32,  // NEW
}

impl Session {
    /// Pop a pending task for a specific batch index.
    /// Tasks are assigned to batch indices in round-robin order by task_id.
    /// 
    /// For batch_size=1 (default), batch_index should be 0, returning any pending task.
    /// For batch_size>1, returns the next task where task_id % batch_size == batch_index.
    pub fn pop_pending_task(&mut self, batch_index: u32, batch_size: u32) -> Option<TaskPtr> {
        let pending_tasks = self.tasks_index.get_mut(&TaskState::Pending)?;
        
        // For batch_size=1, just return any pending task (backward compatible)
        if batch_size <= 1 {
            if let Some((task_id, _)) = pending_tasks.clone().iter().next() {
                return pending_tasks.remove(task_id);
            }
            return None;
        }
        
        // For batch_size>1, find task matching this batch index
        let mut sorted_tasks: Vec<_> = pending_tasks.iter().collect();
        sorted_tasks.sort_by_key(|(id, _)| *id);
        
        for (task_id, _) in sorted_tasks {
            if (*task_id as u32) % batch_size == batch_index {
                return pending_tasks.remove(task_id);
            }
        }
        None
    }
    
    /// Check if a complete batch is ready (enough pending tasks)
    pub fn is_ready(&self, batch_size: u32) -> bool {
        let pending_count = self.tasks_index
            .get(&TaskState::Pending)
            .map(|m| m.len())
            .unwrap_or(0);
        pending_count >= batch_size as usize
    }
}
```

**SessionInfo Extension (for scheduler snapshot):**

```rust
// session_manager/src/model/mod.rs

pub struct SessionInfo {
    pub id: SessionID,
    pub application: String,
    pub tasks_status: HashMap<TaskState, i32>,
    pub creation_time: DateTime<Utc>,
    pub completion_time: Option<DateTime<Utc>>,
    pub state: SessionState,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,  // NEW
    pub resreq: Option<ResourceRequirement>,
}
```

**Executor Extension:**

```rust
// session_manager/src/model/mod.rs

pub struct Executor {
    pub id: ExecutorID,
    pub node: String,
    pub resreq: ResourceRequirement,
    pub shim: Shim,
    pub task_id: Option<TaskID>,
    pub ssn_id: Option<SessionID>,
    pub batch_index: Option<u32>,  // NEW: 0 to batch_size-1
    pub creation_time: DateTime<Utc>,
    pub state: ExecutorState,
}
```

### Algorithms

**Statement Pattern for Gang Scheduling:**

The Statement pattern ensures executors are only created when a full batch can be scheduled. Actions use Statement without knowing about GangPlugin - all gang logic is in Plugin trait callbacks.

```
┌────────────────────────────────────────────────────────────────────┐
│                    GANG SCHEDULING FLOW                            │
├────────────────────────────────────────────────────────────────────┤
│                                                                    │
│  1. Create Statement                                               │
│     stmt := Statement::new(ctx)                                    │
│                                                                    │
│  2. Accumulate Operations (for each allocatable node)              │
│     ┌─────────────────────────────────────────────────────────┐    │
│     │  stmt.pipeline(node, ssn)                               │    │
│     │    → Reserves resources on node (in-memory)             │    │
│     │    → Calls plugins.on_pipeline_executor()               │    │
│     │      → GangPlugin increments pipelined count            │    │
│     │    → NO executor created yet                            │    │
│     └─────────────────────────────────────────────────────────┘    │
│                                                                    │
│  3. Check Readiness (action doesn't know about gang)               │
│     if ctx.is_ready(&ssn) {                                        │
│         → plugins.is_ready() iterates all plugins                  │
│         → GangPlugin checks: (allocated + pipelined) % batch == 0  │
│     }                                                              │
│                                                                    │
│  4a. READY → Commit All                                            │
│      stmt.commit()                                                 │
│        → for each op: create_executor()                            │
│        → on_create_executor() → GangPlugin clears pipelined        │
│                                                                    │
│  4b. NOT READY → Discard All                                       │
│      stmt.discard()                                                │
│        → for each op (reverse): release resources                  │
│        → Calls plugins.on_discard_executor()                       │
│          → GangPlugin decrements pipelined count                   │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
```

**AllocateAction with Statement Pattern (no GangPlugin awareness):**

```rust
// session_manager/src/scheduler/actions/allocate.rs

async fn execute(&self, ctx: &mut Context) -> Result<(), FlameError> {
    let ss = ctx.snapshot.clone();
    
    for ssn in open_sessions {
        if !ctx.is_underused(&ssn)? {
            continue;
        }
        
        // Create statement for this session's allocation
        let mut stmt = Statement::new(ctx.clone());
        
        // Pipeline allocatable nodes
        for node in nodes.iter() {
            if !ctx.is_allocatable(node, &ssn)? {
                continue;
            }
            
            // Reserve resources - plugins notified via on_pipeline_executor
            stmt.pipeline(node, &ssn)?;
            
            // Check if ready - GangPlugin checks pipelined >= batch_size
            if ctx.is_ready(&ssn)? {
                // Commit: create all executors
                stmt.commit().await?;
                break;
            }
        }
        
        // If not ready after all nodes, discard reservations
        if !ctx.is_ready(&ssn)? {
            stmt.discard()?;
        }
    }
    
    Ok(())
}
**Batch-Indexed Task Assignment (pseudocode):**

```rust
// session_manager/src/controller/executors/bound.rs

async fn launch_task(&self, ssn_ptr: SessionPtr) -> Result<Option<Task>, FlameError> {
    let (batch_size, batch_index) = {
        let exe = lock_ptr!(self.executor)?;
        let ssn = lock_ptr!(ssn_ptr)?;
        (ssn.batch_size.max(1), exe.batch_index.unwrap_or(0))
    };
    
    let mut ssn = lock_ptr!(ssn_ptr)?;
    
    // For batch_size > 1, wait until a complete batch is ready
    if batch_size > 1 && !ssn.is_ready(batch_size) {
        return Ok(None);  // Retry later
    }
    
    // Pop task for this executor's batch index
    // For batch_size=1, batch_index=0 returns any pending task
    let task_ptr = ssn.pop_pending_task(batch_index, batch_size);
    
    // ... update task state and return ...
}
```

### System Considerations

**Performance:**
- Batch allocation adds minimal overhead (single atomic operation per batch)
- Task lookup by batch index is O(n) where n = pending tasks; can be optimized with index

**Scalability:**
- Batch size is bounded by available nodes
- Large batches may increase scheduling latency due to gang scheduling

**Reliability:**
- Executor failure mid-batch: remaining batch members continue with their tasks
- Future: batch-level failure handling (out of scope for initial implementation)

**Resource Usage:**
- Additional `batch_index` field per executor (~4 bytes)
- Additional `batch_size` field per session (~4 bytes)

**Observability:**
- Log batch allocation events: "Allocated batch of {batch_size} executors for session {id}"
- Metrics: `flame_batch_allocations_total`, `flame_batch_pending_count`

### Storage Schema Changes

**SQLite:**

```sql
-- Add batch_size to sessions table
ALTER TABLE sessions ADD COLUMN batch_size INTEGER NOT NULL DEFAULT 1;

-- Add batch_index to executors table  
ALTER TABLE executors ADD COLUMN batch_index INTEGER;
```

---

## 4. Use Cases

### Basic Use Cases

**Example 1: Multi-Node LLM Inference**

**Description:** Run a large language model that requires 4 GPUs across 4 nodes using tensor parallelism.

**Workflow:**
1. Register application `llm-inference` whose nodes provide 8 GPUs each.
2. Create session with `batch_size=4`, `min_instances=4`, and a per-task `resreq` requesting 1 GPU.
3. Submit inference requests (4 tasks per request, one per node).
4. Each executor gets its dedicated task based on `batch_index`.
5. All 4 tasks process the same request in parallel.

**Example:**

```python
# Create batch session for tensor-parallel inference
session = flame.open_session(
    session_id="inference-001",
    application="llm-inference",
    resreq={"cpu": 4, "memory": "16g", "gpu": 1},
    batch_size=4,
    min_instances=4,
)

# Submit an inference request (creates 4 coordinated tasks)
request_data = serialize_request("What is the meaning of life?")
for i in range(4):
    session.create_task(input=f"shard_{i}:{request_data}")

# Each executor (batch_index 0-3) receives exactly one task
# Executor 0: Task 0 (shard_0)
# Executor 1: Task 1 (shard_1)
# Executor 2: Task 2 (shard_2)
# Executor 3: Task 3 (shard_3)
```

**Example 2: Distributed Training Checkpoint**

**Description:** Save a distributed training checkpoint across 8 workers.

**Workflow:**
1. Create session with `batch_size=8`
2. Submit checkpoint tasks (8 tasks, one per worker)
3. Each worker saves its portion of the model state
4. All tasks complete together

**Example:**

```python
session = flame.open_session(
    session_id="training-checkpoint",
    application="checkpoint-saver",
    batch_size=8,
)

# Submit checkpoint (8 coordinated tasks)
for worker_id in range(8):
    session.create_task(input=f"checkpoint:worker_{worker_id}")
```

### Advanced Use Cases

**Example 3: Pipeline Parallelism with Uneven Stages**

**Description:** Future enhancement - support heterogeneous batches where different batch indices have different resource requirements.

**Note:** This is out of scope for the initial implementation but demonstrates the extensibility of the design.

---

## 5. References

**Related Documents:**
- [RFE352 - Open Session Enhancement](./RFE352-open-session-enhancement/)
- [RFE384 - Flame Recovery](./RFE384-flame-recovery/)

**External References:**
- [Gang Scheduling](https://en.wikipedia.org/wiki/Gang_scheduling)
- [MPI Collective Operations](https://www.mpi-forum.org/docs/mpi-3.1/mpi31-report.pdf)
- [PyTorch Distributed](https://pytorch.org/docs/stable/distributed.html)
- [DeepSpeed ZeRO](https://www.deepspeed.ai/tutorials/zero/) - Example of multi-node inference

**Plugin Trait (14 callbacks total, 3 new)**

| Callback | Used By | Purpose |
|----------|---------|---------|
| `setup` | All | Initialize plugin state |
| `ssn_order_fn` | Priority, DRF | Session priority/dominant-share ordering |
| `node_order_fn` | DRF | Node priority ordering |
| `is_underused` | Priority, DRF, Gang | Session needs more resources |
| `is_preemptible` | Priority, DRF | Session can be preempted |
| `is_available` | DRF, Shim | Executor compatible with session |
| `is_allocatable` | DRF | Node can host session |
| `is_reclaimable` | Priority, DRF | Executor can be reclaimed |
| `is_ready` | Gang | **NEW**: Session ready for batch commit |
| `on_create_executor` | Priority, DRF, Gang | Track new executor |
| `on_session_bind` | Priority, DRF, Gang | Track session binding |
| `on_session_unbind` | Priority, DRF, Gang | Track session unbinding |
| `on_pipeline_executor` | Gang | **NEW**: Track pipelined (reserved) executor |
| `on_discard_executor` | Gang | **NEW**: Track discarded executor |

**Implementation References:**
- `session_manager/src/scheduler/plugins/gang.rs` - GangPlugin implementation
- `session_manager/src/scheduler/plugins/mod.rs` - Plugin trait and PluginManager
- `common/src/apis/types.rs` - Session and Task definitions
- `session_manager/src/scheduler/actions/` - Scheduler actions
- `session_manager/src/controller/executors/bound.rs` - Task launching logic

---

## Design Decisions

1. **Extend Plugin trait with `is_ready`**: The existing Plugin trait is extended with a single `is_ready()` method. GangPlugin implements this to check if enough resources are pipelined for a complete batch.

2. **Statement pattern for batch allocation**: Executors are NOT created until a full batch can be scheduled. The Statement accumulates pending allocations (pipeline), checks gang readiness via `is_ready()`, then either commits all (creates executors) or discards all (releases reservations).

3. **Task creation validation**: Clients are responsible for creating tasks in `batch_size` increments. The system does not enforce this validation.

3. **Partial batch at session close**: When a session closes with fewer than `batch_size` pending tasks, all pending tasks are cancelled.

4. **Executor failure mid-batch**: If an executor crashes while other batch members are running, Flame will restart/retry the other tasks in the batch.

5. **Batch synchronization**: Executors do NOT wait for all batch members to call `launch_task()` before any task is returned. Each executor independently fetches its indexed task when ready.

6. **Task ordering guarantee**: Tasks are assigned to executors by `task_id % batch_size`. This assumes clients create tasks in batch order (task 0, 1, 2 for batch_size=3, then task 3, 4, 5, etc.). Clients must ensure task IDs within a logical batch are consecutive.

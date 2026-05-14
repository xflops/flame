# Design: GPU Support & DRF Scheduling

**Status:** Draft  
**Author:** TBD  
**Created:** 2026-05-06  
**Issue:** [#433](https://github.com/xflops/flame/issues/433)

---

## 1. Motivation

### Background

The Flame scheduler historically supported **fairshare scheduling** with CPU and memory resources. The fairshare policy distributed cluster resources proportionally across sessions based on their demand, originally converting multi-dimensional resources (CPU, memory) into an abstract "slot" count. (The fairshare plugin and the `slots` field have since been removed; sessions now describe per-task resources exclusively via `ResourceRequirement` — see RFE413 for the priority/DRF scheduler that replaced fairshare.)

However, when integrating Flame with AI/ML frameworks like SGLang, vLLM, and PyTorch, GPU resources are essential. The original scheduler had several limitations:

1. **No GPU field in Rust types**: The `ResourceRequirement` proto included a `gpu` field, but it was not used in the Rust implementation.
2. **No GPU in resource accounting**: CPU and memory were tracked, GPU was not.
3. **No multi-resource fairness**: When resources are heterogeneous (some sessions need GPUs, others don't), simple proportional fairshare can lead to unfair allocation.

### Current Limitations

```rust
// common/src/apis/types.rs - BEFORE this RFE
pub struct ResourceRequirement {
    pub cpu: u64,
    pub memory: u64,
    // gpu field MISSING - proto has it, Rust doesn't
}
```

### Problem Example

```
Cluster: 64 CPUs, 256GB memory, 8 GPUs

Session A (LLM inference):      resreq=(cpu=4,  mem=16GB, gpu=1) per task
Session B (data preprocessing): resreq=(cpu=2,  mem=8GB,  gpu=0) per task

Before this RFE:
  GPU was invisible to the scheduler
  Session B could consume all the CPU/memory, leaving no GPUs for Session A
```

### Objectives

1. **GPU resource tracking**: Add GPU field to `ResourceRequirement` in Rust and propagate through the system.
2. **GPU-aware accounting**: Include GPU in all per-resource accounting.
3. **Executor GPU reporting**: ExecutorManager detects and reports GPU capacity to SessionManager.
4. **DRF scheduling policy**: Implement Dominant Resource Fairness for multi-resource environments where different sessions have different dominant resources.
5. **SDK support**: Update Python and Rust SDKs to accept GPU requirements.

---

## 2. Function Specification

### Configuration

**Cluster default `resreq`:**

The cluster-level field `resreq` provides the **default `resreq`** when a client creates a session without specifying one. (Earlier drafts of this RFE also discussed a separate `cluster.slot` field; that field has since been removed along with the rest of the slots concept.)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cpu` | integer | `1` | CPU cores |
| `mem` / `memory` | size string | `1g` | Memory (supports k/m/g suffixes) |
| `gpu` | integer | `0` | GPUs |

**Format:** `"cpu=<n>,mem=<size>,gpu=<n>"` or `"cpu=<n>,memory=<size>,gpu=<n>"`.

- This field is **optional** and acts as the **default `resreq`** when the client does not specify one.
- If `cluster.resreq` is **unset** AND the client's `resreq` is None, the system falls back to the hardcoded default **`cpu=1,mem=1g,gpu=0`** (no error).

```yaml
# flame-cluster.yaml
cluster:
  resreq: "cpu=4,mem=16g,gpu=1"   # default resreq when client omits it
scheduler:
  policies:
    - priority
    - drf              # NEW: Dominant Resource Fairness
    - gang
```

**Examples:**

| Configuration | Description |
|---------------|-------------|
| `"cpu=1,mem=2g"` | CPU-only default (gpu=0 implicit) |
| `"cpu=4,memory=16g,gpu=1"` | 1 GPU per executor for inference (using `memory`) |
| `"cpu=8,mem=32g,gpu=2"` | 2 GPUs per executor for training (using `mem`) |

**Note:** Both `mem` and `memory` are accepted as the memory key, but use only one per definition.

**Policy selection:**

| Policy | Description |
|--------|-------------|
| `drf` | NEW: Dominant Resource Fairness for multi-resource environments |

### API

**Proto Changes (`rpc/protos/types.proto`):**

The `ResourceRequirement` message already has the `gpu` field:

```protobuf
message ResourceRequirement {
  uint64 cpu = 1;
  uint64 memory = 2;
  int32 gpu = 3;  // Already exists, needs Rust implementation
}
```

**SessionSpec: Add `resreq` field:**

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
  uint32 priority = 8;
  optional ResourceRequirement resreq = 9;  // NEW: Explicit resource request
}
```

**Resource specification:** A session describes its per-task resource needs exclusively via `resreq` (e.g., cpu=16, memory=64g, gpu=4). If `resreq` is omitted, the server applies `cluster.resreq`; if neither is set, the hardcoded fallback `cpu=1,mem=1g,gpu=0` is used.

**Resolution Rules:**

| Condition | Result |
|-----------|--------|
| `resreq` is set | Use explicit resreq |
| `resreq` is None AND `cluster.resreq` set | Use `cluster.resreq` as the session's `resreq` |
| `resreq` is None AND `cluster.resreq` unset | Fall back to hardcoded default `cpu=1,mem=1g,gpu=0` |

This three-case chain is implemented by `resolve_session_resreq` in `session_manager/src/apiserver/frontend.rs`.

**Default resolution:** Default resolution happens server-side in the session manager; SDKs/CLIs do not need to know the cluster default. This means all SDKs/CLIs benefit from the new behavior without per-language changes.

### CLI

**Modified command: `flmctl create`**

```bash
flmctl create --app <APPLICATION> [--resreq <RESREQ>] [--priority <PRIORITY>]
```

**New flag:**

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--resreq` | `-r` | `string` | `None` | Explicit resource request (format: "cpu=N,mem=SIZE,gpu=N" or "cpu=N,memory=SIZE,gpu=N"). When omitted, the server applies `cluster.resreq`. |

**Usage examples:**

```bash
# Using explicit resources (mem)
flmctl create --app llm-inference --resreq "cpu=16,mem=64g,gpu=4"

# Using explicit resources (memory)
flmctl create --app llm-inference --resreq "cpu=16,memory=64g,gpu=4"

# CPU-only workload with explicit resources
flmctl create --app data-preprocess --resreq "cpu=8,mem=32g"

# No --resreq: server applies cluster.resreq, or the hardcoded fallback
flmctl create --app myapp
```

**Node listing (`flmctl list -n`):**

```
Name         State   CPU     Memory      GPU   Allocatable CPU  Allocatable Mem  Allocatable GPU
node-001     Ready   64      256Gi       8     60               240Gi            8
node-002     Ready   64      256Gi       0     60               240Gi            0
```

### Other Interfaces

**Python SDK (`flamepy.core`):**

```python
# Using explicit resources - both "mem" and "memory" accepted
session = flame.create_session(
    application="llm-inference",
    resreq={"cpu": 16, "mem": "64g", "gpu": 4},
)
# OR
session = flame.create_session(
    application="llm-inference",
    resreq={"cpu": 16, "memory": "64g", "gpu": 4},
)

# Omitting resreq lets the server apply cluster.resreq (or the fallback)
session = flame.create_session(application="llm-inference")
```

**Python SDK (`flamepy.runner`):**

```python
from flamepy.runner import Runner, RunnerService

# RunnerService - resreq configured here (creates session internally)
service = RunnerService(
    app="llm-inference",
    execution_object=my_model,
    resreq={"cpu": 16, "memory": "64g", "gpu": 4},
)

# Runner.service() also supports resreq
runner = Runner(application="llm-inference")
service = runner.service(
    my_model,
    resreq={"cpu": 8, "memory": "32g", "gpu": 2},
)
```

**Python SDK (`flamepy.service`):**

```python
from flamepy.service import Session

session = Session(
    name="llm-agent",
    resreq={"cpu": 8, "memory": "32g", "gpu": 2},
)
```

**Rust SDK:**

```rust
use flame_sdk::{Client, SessionAttributes, ResourceRequirement};

let session = client.create_session(SessionAttributes {
    application: "llm-inference".to_string(),
    resreq: Some(ResourceRequirement {
        cpu: 16,
        memory: 64 * 1024 * 1024 * 1024,
        gpu: 4,
    }),
    ..Default::default()
}).await?;
```

**SDK Type Changes:**

```python
# flamepy/core/types.py
@dataclass
class SessionAttributes:
    application: str
    resreq: Optional[ResourceRequirement] = None  # NEW
    id: Optional[str] = None
    common_data: Any = None
    min_instances: int = 0
    max_instances: Optional[int] = None
    batch_size: int = 1

@dataclass
class ResourceRequirement:  # NEW
    cpu: int = 0
    memory: Union[int, str] = 0  # bytes or string like "16g"
    gpu: int = 0
```

**Note:** When using dict form, both `"mem"` and `"memory"` keys are accepted for memory, but use only one per definition.

```rust
// sdk/rust/src/types.rs
pub struct SessionAttributes {
    pub application: String,
    pub resreq: Option<ResourceRequirement>,  // NEW
    pub id: Option<String>,
    pub common_data: Option<Vec<u8>>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
}
```

### Scope

**In Scope:**

- `gpu` field implementation in Rust `ResourceRequirement`
- GPU-aware per-resource accounting
- `resreq` field in `SessionSpec` as the sole way to specify per-task resources
- `cluster.resreq` field as default for `resreq` (with hardcoded fallback `cpu=1,mem=1g,gpu=0`)
- Executor GPU detection and reporting
- Node GPU capacity tracking
- DRF scheduler plugin with dominant share calculation
- SQLite storage for GPU fields
- CLI `--resreq` flag
- Python SDK updates (`flamepy.core`, `flamepy.runner`, `flamepy.service`)
- Rust SDK updates

**Out of Scope:**

- **Fractional GPU allocation**: MIG, MPS, or time-sharing. V1 uses whole GPUs only.
- **GPU type differentiation**: A100 vs H100 vs V100. Use labels/node selectors externally.
- **GPU memory tracking**: Only GPU count, not VRAM.
- **GPU topology awareness**: NUMA, NVLink. Treat GPUs as fungible within a node.

**Limitations:**

- GPUs are discrete resources; cannot allocate 0.5 GPUs.
- DRF assumes all resources are comparable; exotic resources may need weighting.
- GPU detection depends on NVML availability or configuration.

### Feature Interaction

**Related Features:**

| Feature | Interaction |
|---------|-------------|
| **PriorityPlugin (RFE413)** | Orthogonal; priority ordering applies to GPU sessions equally |
| **GangPlugin (RFE400)** | Orthogonal; batch constraints apply to GPU sessions |

**Required Updates:**

| Component | Change |
|-----------|--------|
| `common/src/apis/types.rs` | Add `gpu: i32` to `ResourceRequirement` |
| `common/src/apis/from_rpc.rs` | Include GPU in proto conversion |
| `common/src/apis/to_rpc.rs` | Include GPU in proto conversion |
| `common/src/ctx.rs` | Add cluster `resreq` field (parsed from the string format above) |
| `session_manager/src/model/mod.rs` | Add `gpu` to `NodeInfo`, `ExecutorInfo` |
| `session_manager/src/storage/` | Add `gpu` columns to tables (see migration below); resolve default `resreq` from `cluster.resreq` (or hardcoded `cpu=1,mem=1g,gpu=0`) when client `resreq` is absent during session creation |
| `session_manager/src/scheduler/plugins/drf.rs` | **NEW** DRF plugin |
| `executor_manager/src/stream_handler.rs` | GPU detection in node reporting |

**Compatibility:**

- **Fully backward compatible.** GPU defaults to `0` everywhere.
- Existing sessions without GPU requirements continue to work.
- Proto already has `gpu` field; no wire format change.

**Breaking Changes:** None.

---

## 3. Implementation Detail

### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         Session Manager                                      │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                          PluginManager                              │    │
│  │                                                                     │    │
│  │  ┌──────────────┐  ┌───────────────┐  ┌────────┐                    │    │
│  │  │ PriorityPlug │  │   DRFPlugin   │  │  Gang  │                    │    │
│  │  │              │  │    (NEW)      │  │        │                    │    │
│  │  │ ssn_order:   │  │ ssn_order:    │  │  (no   │                    │    │
│  │  │  by priority │  │  by dominant  │  │  order)│                    │    │
│  │  │              │  │  share        │  │        │                    │    │
│  │  │ is_underused:│  │ is_underused: │  │        │                    │    │
│  │  │  priority    │  │  any resource │  │        │                    │    │
│  │  │  blocking    │  │  below desire │  │        │                    │    │
│  │  └──────────────┘  └───────────────┘  └────────┘                    │    │
│  │                                                                     │    │
│  │  Policy selection: [priority, drf, gang]                            │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                        Resource Tracking                            │    │
│  │                                                                     │    │
│  │   Node:     { cpu: 64, memory: 256GB, gpu: 8 }                      │    │
│  │   Executor: { resreq: { cpu: 4, memory: 16GB, gpu: 1 } }            │    │
│  │   Session:  { resreq: { cpu: 16, memory: 64GB, gpu: 4 } }           │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ RegisterNode(node_status)
                                    │
┌─────────────────────────────────────────────────────────────────────────────┐
│                        Executor Manager                                      │
│                                                                             │
│   detect_gpus() → NVML / CUDA_VISIBLE_DEVICES / config                      │
│   NodeStatus { capacity: { cpu, memory, gpu }, allocatable: { ... } }       │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Components

#### 1. ResourceRequirement Update

**`common/src/apis/types.rs`:**

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceRequirement {
    pub cpu: u64,
    pub memory: u64,
    pub gpu: i32,  // NEW: Number of GPUs (i32 for proto compatibility)
}
```

Per-resource arithmetic (`add`, `sub`, `min`, `max`, `mul`, `great_equal`) is implemented in the same file and is what the scheduler uses for all resource accounting.

#### 2. Proto Conversions

**`common/src/apis/from_rpc.rs`:**

```rust
impl From<rpc::ResourceRequirement> for ResourceRequirement {
    fn from(r: rpc::ResourceRequirement) -> Self {
        Self {
            cpu: r.cpu,
            memory: r.memory,
            gpu: r.gpu,  // NEW
        }
    }
}
```

**`common/src/apis/to_rpc.rs`:**

```rust
impl From<ResourceRequirement> for rpc::ResourceRequirement {
    fn from(r: ResourceRequirement) -> Self {
        Self {
            cpu: r.cpu,
            memory: r.memory,
            gpu: r.gpu,  // NEW
        }
    }
}
```

#### 3. GPU Detection

**`executor_manager/src/stream_handler.rs`:**

```rust
fn build_node_status(ctx: &FlameClusterContext) -> NodeStatus {
    let cpu = num_cpus::get() as u64;
    let memory = system::sysinfo().totalram;
    let gpu = detect_gpus(ctx);
    
    NodeStatus {
        state: NodeState::Ready,
        capacity: ResourceRequirement { cpu, memory, gpu },
        allocatable: ResourceRequirement { cpu, memory, gpu },
        info: NodeInfo { ... },
    }
}

fn detect_gpus(ctx: &FlameClusterContext) -> i32 {
    // Priority: config > env var > NVML detection (always enabled by default)
    
    // 1. Check configuration override
    if let Some(gpu_count) = ctx.executor.gpu_count {
        return gpu_count;
    }
    
    // 2. Check CUDA_VISIBLE_DEVICES
    if let Ok(devices) = std::env::var("CUDA_VISIBLE_DEVICES") {
        if devices.is_empty() || devices == "-1" {
            return 0;
        }
        return devices.split(',').count() as i32;
    }
    
    // 3. Try NVML (always enabled by default)
    match nvml_wrapper::Nvml::init() {
        Ok(nvml) => {
            match nvml.device_count() {
                Ok(count) => {
                    tracing::info!("Detected {} GPU(s) via NVML", count);
                    count as i32
                }
                Err(e) => {
                    tracing::warn!("NVML initialized but failed to get device count: {}", e);
                    0
                }
            }
        }
        Err(e) => {
            // NVML not available - this is fine, just log a warning
            tracing::warn!("NVML not available, GPU detection skipped: {}", e);
            0
        }
    }
}
```

**Note:** NVML detection is always enabled by default (no feature flag required). If NVML library is not installed or no GPUs are present, a warning is logged and GPU count defaults to 0.

#### 4. DRF Plugin

**`session_manager/src/scheduler/plugins/drf.rs`:**

```rust
use std::cmp::Ordering;
use std::collections::HashMap;

use crate::model::{NodeInfoPtr, SessionInfo, SessionInfoPtr, SnapShot, ALL_EXECUTOR, ALL_NODE, OPEN_SESSION};
use crate::scheduler::plugins::{Plugin, PluginPtr};
use common::apis::{ResourceRequirement, SessionID};
use common::FlameError;

/// DRF (Dominant Resource Fairness) scheduling plugin.
///
/// DRF ensures fairness across multiple resource types by equalizing each
/// session's "dominant share" - the maximum share of any resource allocated
/// to that session.
pub struct DRFPlugin {
    /// Total cluster resources (sum of all nodes' allocatable)
    total: ResourceRequirement,
    /// Per-session resource allocations
    allocations: HashMap<SessionID, ResourceRequirement>,
    /// Per-session dominant share (cached for ordering)
    dominant: HashMap<SessionID, f64>,
}

impl DRFPlugin {
    pub fn new_ptr() -> PluginPtr {
        Box::new(DRFPlugin {
            total: ResourceRequirement::default(),
            allocations: HashMap::new(),
            dominant: HashMap::new(),
        })
    }

    /// Calculate dominant share for a session.
    /// Dominant share = max(cpu_share, memory_share, gpu_share)
    fn calculate_dominant_share(&self, allocated: &ResourceRequirement) -> f64 {
        let mut dominant = 0.0_f64;

        // CPU share
        if self.total.cpu > 0 {
            let share = allocated.cpu as f64 / self.total.cpu as f64;
            dominant = dominant.max(share);
        }

        // Memory share
        if self.total.memory > 0 {
            let share = allocated.memory as f64 / self.total.memory as f64;
            dominant = dominant.max(share);
        }

        // GPU share
        if self.total.gpu > 0 {
            let share = allocated.gpu as f64 / self.total.gpu as f64;
            dominant = dominant.max(share);
        } else if allocated.gpu > 0 {
            // Demanding GPU but cluster has none - maximum penalty
            dominant = f64::MAX;
        }

        dominant
    }
}

impl Plugin for DRFPlugin {
    fn name(&self) -> &'static str {
        "drf"
    }

    fn setup(&mut self, ss: &SnapShot) -> Result<(), FlameError> {
        self.allocations.clear();
        self.dominant.clear();
        self.total = ResourceRequirement::default();

        // Sum total cluster resources from all nodes
        let nodes = ss.find_nodes(ALL_NODE)?;
        for node in nodes.values() {
            self.total.cpu += node.allocatable.cpu;
            self.total.memory += node.allocatable.memory;
            self.total.gpu += node.allocatable.gpu;
        }

        tracing::debug!(
            "[DRF] Total cluster resources: cpu={}, memory={}, gpu={}",
            self.total.cpu,
            self.total.memory,
            self.total.gpu
        );

        // Track per-session allocations from executors
        let executors = ss.find_executors(ALL_EXECUTOR)?;
        for exec in executors.values() {
            if let Some(ssn_id) = &exec.ssn_id {
                let entry = self.allocations
                    .entry(ssn_id.clone())
                    .or_insert_with(ResourceRequirement::default);
                entry.cpu += exec.resreq.cpu;
                entry.memory += exec.resreq.memory;
                entry.gpu += exec.resreq.gpu;
            }
        }

        // Pre-compute dominant shares for all sessions
        let sessions = ss.find_sessions(OPEN_SESSION)?;
        for ssn in sessions.values() {
            let allocated = self.allocations
                .get(&ssn.id)
                .cloned()
                .unwrap_or_default();
            let ds = self.calculate_dominant_share(&allocated);
            self.dominant.insert(ssn.id.clone(), ds);

            tracing::debug!(
                "[DRF] Session <{}>: allocated=({},{},{}), dominant_share={:.4}",
                ssn.id,
                allocated.cpu,
                allocated.memory,
                allocated.gpu,
                ds
            );
        }

        Ok(())
    }

    fn ssn_order_fn(&self, s1: &SessionInfo, s2: &SessionInfo) -> Option<Ordering> {
        let ds1 = self.dominant.get(&s1.id).copied().unwrap_or(0.0);
        let ds2 = self.dominant.get(&s2.id).copied().unwrap_or(0.0);

        // Lower dominant share = higher priority (gets resources first)
        // Use partial_cmp and reverse for correct ordering
        ds1.partial_cmp(&ds2).map(|o| o.reverse())
    }

    fn is_underused(&self, ssn: &SessionInfoPtr) -> Option<bool> {
        // A session is underused if:
        // 1. It has pending tasks (can use more executors)
        // 2. Its dominant share is below the fair share (1/n)
        let pending = ssn.tasks_status
            .get(&TaskState::Pending)
            .copied()
            .unwrap_or(0);
        
        if pending == 0 {
            return Some(false);  // No pending tasks = not underused
        }

        let allocated = self.allocations
            .get(&ssn.id)
            .cloned()
            .unwrap_or_default();

        let num_sessions = self.dominant.len().max(1) as f64;
        let fair_share = 1.0 / num_sessions;
        let ds = self.calculate_dominant_share(&allocated);

        Some(ds < fair_share)
    }

    fn on_executor_allocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr, exec: &ExecutorInfo) {
        // Update allocation tracking when executor is allocated
        let entry = self.allocations
            .entry(ssn.id.clone())
            .or_insert_with(ResourceRequirement::default);
        entry.cpu += exec.resreq.cpu;
        entry.memory += exec.resreq.memory;
        entry.gpu += exec.resreq.gpu;
        
        let ds = self.calculate_dominant_share(entry);
        self.dominant.insert(ssn.id.clone(), ds);
    }

    fn on_executor_unallocate(&mut self, _node: NodeInfoPtr, ssn: SessionInfoPtr, exec: &ExecutorInfo) {
        if let Some(entry) = self.allocations.get_mut(&ssn.id) {
            entry.cpu = entry.cpu.saturating_sub(exec.resreq.cpu);
            entry.memory = entry.memory.saturating_sub(exec.resreq.memory);
            entry.gpu = entry.gpu.saturating_sub(exec.resreq.gpu);
            
            let ds = self.calculate_dominant_share(entry);
            self.dominant.insert(ssn.id.clone(), ds);
        }
    }
}
```

#### 5. Plugin Registration

**`session_manager/src/scheduler/plugins/mod.rs`:**

```rust
mod drf;  // NEW

use drf::DRFPlugin;

const PLUGIN_REGISTRY: &[PluginInfo] = &[
    PluginInfo {
        name: "priority",
        constructor: PriorityPlugin::new_ptr,
        configurable: true,
    },
    PluginInfo {
        name: "drf",  // NEW
        constructor: DRFPlugin::new_ptr,
        configurable: true,
    },
    PluginInfo {
        name: "gang",
        constructor: GangPlugin::new_ptr,
        configurable: true,
    },
    PluginInfo {
        name: "shim",
        constructor: ShimPlugin::new_ptr,
        configurable: false,
    },
];
```

#### 6. Cluster Default Resreq Resolution

This sub-component implements the resolution chain spelled out in §2 (Resolution Rules table) for the `cluster.resreq` field. Resolution lives **server-side** in the session manager (at the apiserver boundary where `SessionSpec` arrives from the wire) so there is a single source of truth for default selection: SDKs and CLIs need no changes, and behavior is uniform regardless of which client populated the request. The string format is parsed by `impl From<&String> for ResourceRequirement` in `common/src/apis/types.rs`, which accepts both `mem` and `memory` keys.

**`common/src/ctx.rs`:**

```rust
// FlameClusterYaml (serde layer): hold the optional default-resreq string.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameClusterYaml {
    pub name: String,
    pub endpoint: String,
    pub resreq: Option<String>,  // cluster-wide default resreq
    pub policies: Option<Vec<String>>,
    // ... existing fields unchanged ...
}

// FlameCluster (runtime struct): parsed value, or None if unset.
#[derive(Debug, Clone)]
pub struct FlameCluster {
    pub name: String,
    pub endpoint: String,
    pub resreq: Option<ResourceRequirement>,
    // ... existing fields unchanged ...
}
```

**`session_manager/src/apiserver/frontend.rs`:**

The three-case resolution chain is implemented as a small helper called from `Frontend::create_session`, immediately after `SessionSpec` is unpacked from the gRPC request and before the `SessionAttributes` is built. The helper always returns a concrete `ResourceRequirement` (never `None`), so downstream layers — scheduler plugins included — can rely on `ssn.resreq` always being populated.

```rust
// Hardcoded safety-net fallback when neither client nor cluster specifies anything.
// 1 CPU, 1 GiB memory, 0 GPU.
const DEFAULT_FALLBACK_RESREQ: ResourceRequirement = ResourceRequirement {
    cpu: 1,
    memory: 1024 * 1024 * 1024,
    gpu: 0,
};

/// Resolve the effective ResourceRequirement for a new session.
///
/// Implements the chain documented in §2 Resolution Rules:
///   1. explicit (client) resreq is set                                          -> use it
///   2. explicit is None AND cluster.resreq set                    -> use cluster default
///   3. explicit is None AND cluster.resreq unset                  -> hardcoded fallback
fn resolve_session_resreq(
    explicit: Option<ResourceRequirement>,
    cluster_default: Option<&ResourceRequirement>,
) -> ResourceRequirement {
    if let Some(rr) = explicit {
        return rr;
    }
    if let Some(default) = cluster_default {
        tracing::info!(
            "applying cluster.resreq default to session: {:?}",
            default
        );
        return default.clone();
    }
    tracing::info!(
        "no resreq supplied and no cluster default; applying hardcoded fallback: {:?}",
        DEFAULT_FALLBACK_RESREQ
    );
    DEFAULT_FALLBACK_RESREQ
}
```

Call site inside `Frontend::create_session`:

```rust
let explicit = ssn_spec.resreq.map(apis::ResourceRequirement::from);
let resreq = resolve_session_resreq(explicit, self.cluster_default_resreq.as_ref());

let attr = SessionAttributes {
    id: ssn_id,
    application: ssn_spec.application,
    common_data: ssn_spec.common_data.map(apis::CommonData::from),
    min_instances: ssn_spec.min_instances,
    max_instances: ssn_spec.max_instances,
    batch_size: ssn_spec.batch_size.max(1),
    priority: ssn_spec.priority,
    resreq: Some(resreq),  // always populated by resolver
};
```

**Notes:**

- The resreq parser (`impl From<&String> for ResourceRequirement` in `common/src/apis/types.rs`) accepts both `mem` and `memory` keys.
- If `cluster.resreq` is malformed at startup, `ResourceRequirement::from` logs the parse error and the cluster fails to start with a clear `InvalidConfig`.
- The hardcoded `DEFAULT_FALLBACK_RESREQ` (cpu=1, memory=1 GiB, gpu=0) is a safety net only; production deployments should set `cluster.resreq` explicitly so that defaults are auditable from the cluster config.
- An `info`-level tracing log is emitted whenever case 2 or case 3 fires, so operators can observe which sessions ran with implicit defaults rather than client-supplied resources.

### Data Structures

**ResourceRequirement (updated):**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cpu` | `u64` | `0` | CPU cores |
| `memory` | `u64` | `0` | Memory in bytes |
| `gpu` | `i32` | `0` | Number of GPUs |

**DRFPlugin internal state:**

| Field | Type | Description |
|-------|------|-------------|
| `total` | `ResourceRequirement` | Sum of all nodes' allocatable resources |
| `allocations` | `HashMap<SessionID, ResourceRequirement>` | Per-session resource allocation |
| `dominant` | `HashMap<SessionID, f64>` | Cached dominant share per session |

### Algorithms

#### DRF Dominant Share Calculation

From the [UC Berkeley DRF paper](https://people.eecs.berkeley.edu/~alig/papers/drf.pdf):

```
For session s with allocation A = (cpu, memory, gpu):
  dominant_share(s) = max(
    A.cpu / total.cpu,
    A.memory / total.memory,
    A.gpu / total.gpu
  )

Allocation order: Pick session with smallest dominant_share
```

#### Node Fit Calculation (per-resource)

```
Given a node's allocatable A = (cpu, memory, gpu) and a session's per-task R = (cpu, memory, gpu):

# How many full executors of the session can fit on this node:
fit = min(
  A.cpu / R.cpu,       // if R.cpu > 0, else infinity
  A.memory / R.memory, // if R.memory > 0, else infinity
  A.gpu / R.gpu        // if R.gpu > 0, else infinity
)

Example:
  Node:     cpu=64, memory=256GB, gpu=8
  Per-task: cpu=4,  memory=16GB,  gpu=1

  fit = min(64/4, 256/16, 8/1) = min(16, 16, 8) = 8

  GPU is the limiting factor (dominant resource for this node)
```

### System Considerations

**Performance:**

| Aspect | Impact |
|--------|--------|
| `setup()` overhead | O(n + m) where n=nodes, m=executors |
| `dominant_share()` | O(1) arithmetic |
| `ssn_order_fn()` | O(1) hash lookup |

**Scalability:**

- DRF scales linearly with number of sessions
- GPU detection is done once at executor startup

**Reliability:**

- GPU detection always enabled by default; logs warning if NVML unavailable
- Fallback chain: config → CUDA_VISIBLE_DEVICES → NVML → 0 (with warning)
- Nodes without GPUs report `gpu=0`; GPU-requiring sessions won't be scheduled there

**Resource Usage:**

| Resource | Overhead |
|----------|----------|
| Memory | +4 bytes per ResourceRequirement (gpu field) |
| Storage | +4 bytes per executor/node row (gpu columns) |

**Security:**

- GPU count is reported by executor; malicious executor could lie
- Trust model same as CPU/memory reporting

**Observability:**

- Log GPU detection results at executor startup
- Log dominant share calculations in DRF plugin
- Add GPU to `flmctl list -n` output

### Dependencies

| Dependency | Type | Notes |
|------------|------|-------|
| `nvml-wrapper` | External | NVIDIA GPU detection (always enabled, warns if unavailable) |
| `common/src/apis/types.rs` | Internal | ResourceRequirement |
| `session_manager/src/scheduler/plugins/` | Internal | Plugin framework |

### Storage Migration

**SQLite schema changes:**

```sql
-- Migration: Add GPU columns to executor table
ALTER TABLE executor ADD COLUMN resreq_gpu INTEGER NOT NULL DEFAULT 0;

-- Migration: Add GPU columns to node table  
ALTER TABLE node ADD COLUMN capacity_gpu INTEGER NOT NULL DEFAULT 0;
ALTER TABLE node ADD COLUMN allocatable_gpu INTEGER NOT NULL DEFAULT 0;
```

**Note:** Existing rows receive `gpu = 0` automatically via DEFAULT. No data migration required.

---

## 4. Use Cases

### Basic Use Cases

**Example 1: GPU Inference Workload**

**Description:** Deploy LLM inference with GPU requirements.

```bash
# Create GPU inference session with explicit per-task resreq
flmctl create --app llm-inference --resreq cpu=4,mem=16g,gpu=1
```

**Workflow:**
1. Session requests per-task `cpu=4, mem=16GB, gpu=1` (one GPU per executor).
2. Scheduler finds nodes with sufficient GPU capacity.
3. Allocates executors on GPU nodes; each task uses 1 GPU.

**Outcome:** Session executors land on GPU nodes; each task uses 1 GPU.

---

**Example 2: Cluster Default `resreq` (no client-supplied `resreq`)**

**Description:** Client omits `resreq`; the cluster default applies.

```bash
# Cluster config: cluster.resreq: "cpu=2,mem=8g"
flmctl create --app data-preprocess
# No --resreq → server resolves resreq = cpu=2, mem=8g, gpu=0
```

**Workflow:**
1. Client sends SessionSpec with no `resreq`.
2. Session manager resolves the default server-side from `cluster.resreq`.
3. If `cluster.resreq` is unset, the hardcoded fallback `cpu=1,mem=1g,gpu=0` is used instead.

**Outcome:** Session is created with the cluster's default resource requirement; SDKs/CLIs require no changes.

---

**Example 3: Mixed GPU and CPU Workloads**

**Description:** Fair sharing between GPU and CPU sessions.

```bash
# GPU inference session
flmctl create --app llm-inference --resreq cpu=4,mem=16g,gpu=1

# CPU preprocessing session
flmctl create --app data-preprocess --resreq cpu=4,mem=4g
```

```
Cluster: cpu=64, memory=256GB, gpu=8

With DRF:
  llm-inference (per-task cpu=4, mem=16GB, gpu=1): dominant resource = GPU
  data-preprocess (per-task cpu=4, mem=4GB, gpu=0): dominant resource = CPU

DRF equalizes dominant shares:
  Both sessions converge toward equal share of their dominant resource
  GPU session doesn't starve CPU session and vice versa
```

---

**Example 4: GPU-Only Node Scheduling**

**Description:** Schedule GPU workloads only on GPU-capable nodes.

```
Nodes:
  node-001: cpu=64, memory=256GB, gpu=8  (GPU node)
  node-002: cpu=64, memory=256GB, gpu=0  (CPU-only node)

Session per-task resreq: (cpu=4, mem=16GB, gpu=1)

Scheduling:
  node-001: fit = min(64/4, 256/16, 8/1) = min(16, 16, 8) = 8 executors
  node-002: gpu requirement is 1 but node has gpu=0
            → is_allocatable() = false

Result: Session scheduled only on node-001
```

### Advanced Use Cases

**Example 5: DRF with Heterogeneous Demands**

```
Cluster: cpu=100, memory=100GB, gpu=10

Session A (GPU-heavy): per-task (1 cpu, 4GB,  2 gpu)
Session B (CPU-heavy): per-task (4 cpu, 1GB,  0 gpu)

With DRF:
  A's dominant resource: GPU (2/10 = 20% per task)
  B's dominant resource: CPU (4/100 = 4% per task)

  DRF allocates so that max shares are equal:
  A gets 25 tasks: gpu_share = 50/10 = 50%
  B gets 12.5 tasks: cpu_share = 50/100 = 50%

  Both sessions have equal dominant share (50%)
```

---

## 5. References

### Related Documents

- [RFE400 - Batch Support in Session](../RFE400-batch-session/FS.md)
- [RFE408 - Enhance Fairshare for batch_size](../RFE408-fairshare-batch-size/FS.md)
- [RFE413 - Priority Scheduling](../RFE413-priority-scheduling/FS.md)

### External References

- [DRF Paper: Dominant Resource Fairness (NSDI '11)](https://people.eecs.berkeley.edu/~alig/papers/drf.pdf)
- [NVIDIA KAI Scheduler - GPU-Aware DRF](https://github.com/NVIDIA/KAI-Scheduler)
- [Volcano H-DRF Implementation](https://github.com/volcano-sh/volcano/blob/master/pkg/scheduler/plugins/drf/drf.go)
- [Apache Hadoop DominantResourceCalculator](https://github.com/apache/hadoop/blob/trunk/hadoop-yarn-project/hadoop-yarn/hadoop-yarn-common/src/main/java/org/apache/hadoop/yarn/util/resource/DominantResourceCalculator.java)
- [Kubernetes GPU Scheduling](https://kubernetes.io/docs/tasks/manage-gpus/scheduling-gpus/)
- [NVIDIA NVML](https://developer.nvidia.com/nvidia-management-library-nvml)

### Implementation References

| File | Description |
|------|-------------|
| `common/src/apis/types.rs` | ResourceRequirement — add `gpu` field |
| `common/src/apis/from_rpc.rs` | Proto conversion — include GPU |
| `common/src/apis/to_rpc.rs` | Proto conversion — include GPU |
| `common/src/ctx.rs` | Cluster config — add `cluster.resreq` |
| `rpc/protos/types.proto` | SessionSpec — add `resreq` field |
| `executor_manager/src/stream_handler.rs` | GPU detection |
| `session_manager/src/model/mod.rs` | NodeInfo, ExecutorInfo — add GPU |
| `session_manager/src/storage/` | SQLite schema — add GPU columns |
| `session_manager/src/scheduler/plugins/drf.rs` | **NEW** DRF plugin |
| `session_manager/src/scheduler/plugins/mod.rs` | Plugin registration |
| `flmctl/src/create.rs` | Add `--resreq` flag |
| `sdk/python/src/flamepy/core/types.py` | Add `ResourceRequirement`, update `SessionAttributes` |
| `sdk/python/src/flamepy/core/client.py` | Add `resreq` parameter |
| `sdk/python/src/flamepy/runner/runner.py` | Add `resreq` parameter to `RunnerService` and `Runner.service()` |
| `sdk/python/src/flamepy/service/client.py` | Add `resreq` parameter to `Session` |
| `sdk/rust/src/types.rs` | Add `resreq` to `SessionAttributes` |

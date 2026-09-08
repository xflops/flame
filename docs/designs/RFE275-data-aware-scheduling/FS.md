# RFE275: Data-Aware Scheduling and Routing

## 1. Motivation

Flame schedules only from control-plane state: resources, executor state, shim,
and session demand. It does not know data a running service has already created
inside an executor, such as an LLM KV-cache index. This RFE makes that runtime
data an executor attribute and routes new work using an explicit task affinity.

The data can be entirely internal to the service or remotely cached. It is not
`common_data`, and object-cache placement is not part of this RFE.

### Goals

- A service publishes its current runtime data attributes through its session
  context.
- Task creation accepts an `option` carrying `affinity: Vec<bytes>`.
- DAS scores an executor/task pair from the task affinity and executor
  attributes, preferring a local match without making it mandatory.
- Attributes are removed with their executor/session lifecycle.

### Non-goals

- Object-cache placement, object references, common-data locality, replication,
  or migration.
- Inspecting opaque task input or published data values.
- Hard affinity, preemption, or cross-session data sharing.

## 2. Function Specification

### Executor attributes and publishing

An executor has a volatile, session-scoped `attr: Vec<bytes>` of opaque values.
A service updates that set through its `SessionContext`:

```rust
context.publish([prefix_cache_key])?;
```

Python exposes the equivalent `context.publish([...])`. Attribute values are
nonempty byte strings, at most 256 bytes each. A snapshot may contain up to
1,024 unique attributes and 64 KiB total.

`publish` replaces the complete attribute set; it never appends. Publishing an
empty set clears it. This makes eviction straightforward: a service publishes
its current KV-cache index after an insert or eviction. The service owns the
attribute format; Flame only performs exact byte matching.

The Rust SDK keeps the newest snapshot in a shared
`Arc<Mutex<Option<ExecutorAttributes>>>` session pointer. `None` means no
publication; `Some(empty)` is an explicit clear. It is carried back in the
existing `OnSessionEnter` or `OnTaskInvoke`
shim response, so a service does not connect directly to the session manager.
Executor manager then forwards it on the existing `BindExecutorCompletedRequest`
or `CompleteTaskRequest` that it already sends to session manager. The executor
manager derives the executor identity; the session manager derives and verifies
the bound session from that executor. A service cannot publish for another
executor or session.

Each publication is a complete replacement snapshot. Session manager stores the
latest received set for the bound executor; duplicate keys are eliminated by set
semantics.

### Task option and affinity

`TaskSpec` gains the requested public option:

```protobuf
message TaskSpec {
  string session_id = 2;
  optional bytes input = 3;
  optional bytes output = 4;
  repeated bytes affinity = 5;
}
```

SDKs add the explicit option type:

```rust
pub struct TaskOptions {
    pub affinity: HashSet<Vec<u8>>,
}
```

New SDK entry points accept it as `option`; the existing one-argument methods
remain unchanged for compatibility:

```rust
session.create_task_with_options(input, TaskOptions {
    affinity: HashSet::from([prefix_cache_key]),
}).await?;
```

`run_with_options` and `invoke_with_options` follow the same pattern. Python
accepts `option=TaskOptions(affinity={...})`; omitting it uses the empty default.
The option serializes to `TaskSpec.affinity`:

```python
await session.create_task(
    input=request,
    option=TaskOptions(affinity={prefix_cache_key}),
)
```

The same validation and limits apply as for executor attributes. Duplicate
values are removed before persistence. Empty or omitted affinity is valid and
means no data preference.

### gRPC and SDK compatibility

`affinity` changes the existing `TaskSpec` message; it does **not** add a new
CreateTask RPC. The frontend request already embeds `TaskSpec`, so generated
gRPC clients carry the set as a deduplicated `repeated bytes` field after the
protocol is regenerated.

The implementation updates these protocol sources in lockstep:

- `rpc/protos/types.proto` — canonical service protocol.
- `sdk/rust/protos/types.proto` — Rust SDK generated client source.
- `sdk/python/protos/types.proto` — Python SDK generated client source.

It then regenerates the Rust `prost`/`tonic` bindings and Python protobuf/gRPC
stubs. `common::apis` conversion code copies `repeated bytes affinity` between
the wire `TaskSpec` and domain `Task`.

Both SDKs add extensible `TaskOptions { affinity }` and option-aware task submission:

- Rust: `create_task_with_options`, `run_with_options`, and
  `invoke_with_options`; existing one-argument methods delegate with the empty
  default.
- Python: an optional `option: TaskOptions | None` keyword on `create_task`,
  `run`, and `invoke`; `None` uses the empty default.

Internally, the existing controller, storage, and engine `create_task` methods
also take `Option<TaskOptions>`. `None` is the ordinary no-option path; the
frontend supplies `Some(TaskOptions { affinity })` only when the task spec
contains affinity keys.

No caller needs to construct a protobuf `TaskSpec` directly.

### DAS score

For a pending task `T` and a compatible executor `E` bound to the same session:

```text
score(E, T) = | unique(T.affinity) intersection unique(E.attr) |
```

The score is a preference, not an eligibility predicate. DAS applies it only to
**task routing**: when `E` requests work, it selects the pending task in `E`'s
bound session with the largest score. It does not alter executor allocation,
idle-executor reuse, node selection, or Shuffle. An attribute is known only
after a running executor publishes it, so those earlier lifecycle decisions
cannot safely use it.

Ties use deterministic task creation time then task ID. All-zero scores take
this same normal fallback path.

### Lifecycle

Affinity only reorders work inside a session; priority, DRF, capacity, shim
compatibility, and existing allocation checks run first. For equal locality
scores, the normal FIFO task order wins. This keeps fallback deterministic while
allowing positive affinity matches to be preferred.

Attributes are removed atomically when the executor unbinds, releases, is lost,
or changes binding generation. They are not persisted across executor-manager
or session-manager restart. A reconnected service republishes its index after
binding. This intentionally favors a remote execution over routing to a cache
whose runtime ownership is uncertain.

### Compatibility

All protocol fields and RPCs are additive. Existing services never publish and
existing tasks have no affinity, yielding score zero and current eligibility
behavior. Existing callers are not required to change.

## 3. Implementation Detail

### Wire contracts

The shim response wrappers gain an optional publication batch; generic `Result`
is not overloaded:

```protobuf
message ExecutorAttributes {
  repeated bytes attr = 1;
}

message OnTaskInvokeResponse {
  TaskResult task_result = 1;
  optional ExecutorAttributes attributes = 2;
}

message BindExecutorCompletedRequest {
  // existing fields unchanged
  optional ExecutorAttributes attributes = 3;
}

message CompleteTaskRequest {
  // existing fields unchanged
  optional ExecutorAttributes attributes = 3;
}
```

Inside executor manager, the shim boundary returns a structured
`SessionEnterResponse`, not raw attributes. Its optional
`executor_attributes: Option<Arc<Mutex<ExecutorAttributes>>>` retains the
shared snapshot from session entry until binding completes.

Equivalent publication support is added to `OnSessionEnter`. Existing backend
completion requests are reachable only from executor manager. The controller
accepts an update only while its executor remains Bound. A failed or stale task
completion cannot update attributes because publication validation and task
completion both succeed before the new snapshot is indexed.

The session manager keeps the authoritative volatile index attached to the
executor identity:

```text
executor id -> { attr }
```

No task input or publication value is decoded by the controller. Attributes are
not persisted and are not added to public `ExecutorStatus`; diagnostic logging
reports only counts, so application data identifiers remain runtime-private.

`Task.affinity` is persisted with the existing task domain object and returned
on normal task reads. `common::apis` and all RPC conversions carry it. SQLite
stores a JSON byte-set column with an empty default; filesystem storage
stores affinity in task metadata; and `NoneEngine` retains it in memory.
Missing migration fields decode as an empty list.

### Routing algorithm

Under the existing session pending-task lock, the controller:

1. obtains the requesting executor's current attribute snapshot;
2. examines only pending tasks in that executor's bound session;
3. picks the greatest intersection score, then the deterministic task-ID winner
   when scores tie;
4. removes the selected task and performs the existing pending-to-running
   transition.

This is one critical section, so two executors cannot receive the same task.
The attribute snapshot may be stale by one publish response; locality remains
best effort and a service must tolerate a missing/evicted local entry.

### Failure semantics

- Invalid or oversized attributes are rejected without changing the prior
  snapshot.
- Publish before binding, after unbinding, or for an old binding is discarded.
- A repeated publication replaces the same set and is idempotent.
- A publish transport failure leaves scheduling at the last accepted snapshot;
  after lifecycle cleanup it falls back to score zero.

### Diagnostics

Attribute values are not logged. Existing executor/session lifecycle logs remain
the diagnostics surface for this initial implementation.

## 4. Use Case: vLLM KV-cache routing

This RFE uses vLLM's prefix/KV cache as the reference workload. A cache key is
an application-defined opaque byte value derived from the model revision,
tenant (when isolation requires it), and tokenized prompt prefix. It is not the
KV tensors and need not expose prompt text. For example, an application may use
the binary encoding of `SHA-256(model_revision || tenant || prefix_token_ids)`.

1. A vLLM executor runs prefill for a request whose prefix hashes to `P` and
   materializes the prefix KV blocks in its local vLLM cache.
2. Before its prefill handler returns, it calls
   `context.publish(current_kv_cache_keys)`. The snapshot includes `P` and all
   other cache keys still resident on that executor; an eviction publishes a
   new snapshot without the evicted key.
3. The client creates a later decode/continuation task with
   `option=TaskOptions(affinity={P})`. It computes `P` from the same public key
   algorithm; the task payload remains opaque to Flame.
4. When an executor requests a task, DAS intersects its published attributes
   with each pending task's affinity. The executor holding `P` gets a score of
   one and is preferred over otherwise tied decode tasks.
5. If the matching executor is busy, has unbound, or no longer publishes `P`,
   the continuation runs on another compatible executor and vLLM performs its
   ordinary cache-miss/prefill work. DAS never treats a match as a correctness
   guarantee.

This maps directly to split prefill/decode serving too: prefill publishes the
prefix keys it produced; decode requests carry those keys as affinity. The
first release keeps the attributes session-scoped, so prefill and decode must
share a Flame session. Cross-session prefix-cache routing is deliberately
future work because it needs explicit sharing and tenant-isolation rules.

With Runner, this is a stateless class service with autoscaling. Each executor
constructs its own vLLM engine and retains that engine's process-local KV cache
while it remains bound; Runner does not serialize or write the engine state back
to object cache. Each replica publishes its own `attr` set, which is exactly
what gives DAS distinct cache locations to score.

```python
from hashlib import sha256

from flamepy import ResourceRequirement, TaskOptions
from flamepy.runner import Runner


def prefix_key(model: str, token_ids: list[int]) -> bytes:
    payload = model.encode() + b"\0" + b",".join(
        str(token).encode() for token in token_ids
    )
    return sha256(payload).digest()


class VllmEngine:
    # RFE275 Runner extension: set by Runner in each executor process.
    _flame_session_context = None

    def __init__(self):
        self.llm = None

    def generate(self, model: str, prompt: str, prompt_token_ids: list[int]) -> str:
        if self.llm is None:
            self.llm = load_vllm(model)

        result = self.llm.generate(prompt)

        # `current_vllm_kv_keys` is the vLLM integration adapter. It reads the
        # engine's actual resident prefix/KV-cache index as opaque byte keys.
        self._flame_session_context.publish(current_vllm_kv_keys(self.llm))
        return result


with Runner("vllm-das") as rr:
    engine = rr.service(
        VllmEngine,
        warmup=1,  # Start one vLLM executor before the first request.
        resreq=ResourceRequirement(  # Resources required by every replica.
            cpu=8,
            memory=64 * 1024**3,
            gpu=1,
        ),
    )

    token_ids = tokenize("Explain data-aware scheduling")
    key = prefix_key("llama-3.1-8b", token_ids)

    response = engine.generate(
        "llama-3.1-8b",
        "Explain data-aware scheduling",
        token_ids,
        option=TaskOptions(affinity={key}),  # RFE275 Runner extension
    ).get()
```

Each autoscaled `VllmEngine` replica has independent vLLM state. A later
request with `TaskOptions(affinity={key})` is preferentially routed to a replica
whose published `attr` contains `key`; a miss still runs normally and then
publishes the newly resident key.

### Obtaining vLLM KV-cache keys

Flame must not use vLLM's private cache-object identifiers. The application
defines stable, opaque byte keys from the cache-relevant request identity:

```python
def kv_block_key(model_revision: str, tenant: bytes, token_ids: list[int]) -> bytes:
    payload = (
        b"flame-vllm-kv-v1\0"
        + model_revision.encode()
        + b"\0"
        + tenant
        + encode_u32s(token_ids)
    )
    return sha256(payload).digest()
```

The key namespace/version, model revision, tenant or isolation domain, adapter
identity, and every cache-affecting model setting must be included in the
payload. The exact key format is application-owned and must change whenever its
cache-compatibility assumptions change.

Before creating a task, the client or a request gateway tokenizes the prompt
with the model's tokenizer and splits the token IDs at vLLM's configured KV-cache
block boundary. It supplies a key for each reusable prefix block:

```python
TaskOptions(affinity={
    kv_block_key(model_revision, tenant, block_a),
    kv_block_key(model_revision, tenant, block_a + block_b),
    kv_block_key(model_revision, tenant, block_a + block_b + block_c),
})
```

After prefill, the vLLM integration publishes keys for the prefix blocks it can
actually reuse. DAS scores the intersection with a task's affinity set, so
an executor holding two of the three prefixes scores `2` and is preferred over
an executor holding one or none. If a caller cannot tokenize locally, its
gateway performs this step before it creates the Flame task.

The integration must update its published `attr` when vLLM evicts a block. If
vLLM does not expose eviction events, it must publish conservatively (for
example, clear keys at request completion or use a short application-managed
TTL). A stale key can only cause a soft routing preference; vLLM still handles a
cache miss correctly.

## 5. References

- [RFE275 GitHub Issue](https://github.com/xflops/flame/issues/275)
- `sdk/rust/src/service/mod.rs`
- `rpc/protos/shim.proto`
- `executor_manager/src/shims/grpc_shim.rs`
- `session_manager/src/controller/mod.rs`

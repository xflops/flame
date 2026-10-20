# RFE584: Search-R1 training on Flame

Issue: [#584](https://github.com/xflops/flame/issues/584)

Status: draft for review. Items marked **(verify)** are assumptions about
external projects that must be checked against their current source before
implementation.

## 1. Motivation

**Background:**
Search-R1 trains an LLM with reinforcement learning to interleave reasoning with
search-engine calls. During a rollout the model writes `<think>`, emits
`<search>query</search>`, receives `<information>...</information>` from a
retriever, and finally writes `<answer>...</answer>`. The reward is exact match
(EM) of the answer against the gold answer. The reference implementation is built
on veRL and Ray **(verify)**.

Flame already runs GPU workloads (FEM GPU selection, RFE565), batched vLLM
serving (`examples/vllm/serving`), and distributed rollout collection
(`examples/rl`). No example runs a full RL training loop on Flame.

**Target:**
One demo that runs Search-R1-style RL (GRPO) on a small model with **the whole
training loop executing as Flame applications**, written with the `flamepy.app` API
(`app.init`, `@app.service`, `.remote`, `app.get`) so no `flmctl deploy`, package
build, or manual registration is needed. Retrieval, rollout and learning are
`flamepy.app` services. Serving the trained model, and using it from an agent
such as Hermes, are left for a later step.

Success criteria:
- Training raises EM on a held-out set versus the untrained base model
  (the number, not the absolute level, is the check).
- Killing a rollout executor mid-step does not lose the run.
- Nothing outside Flame has to be installed on the GPU nodes (no standalone Ray
  or vLLM server).

## 2. Function Specification

**Scope:**
- **In scope:** one Python program (the training driver) that declares three
  `flamepy.app` services (rollout, trainer). One model
  size (Qwen2.5-3B-Instruct **(verify)**), single trainer node.
- **Out of scope:** reproducing the paper's final scores, multi-node training,
  online web search as the retriever, PPO with a critic (GRPO only), a new Flame
  API, serving the trained model, and an agent front end. The demo must run on the
  current Flame release; gaps found along the way become issues, not scope.

**Layout** (all under `examples/rl/search-r1/`):

```
README.md          run book
pyproject.toml     the `sr1` package and the dependencies executors install
src/               train.py (driver), rollout.py, trainer.py, and the shared
                   library: prompt and reward, BM25 index, search loop, GRPO bookkeeping
tests/             unit tests that need no GPU
```

**CLI:**

```sh
# 1. train: the driver creates its own Flame application and services (flamepy.app)
cd examples/rl/search-r1
python3 src/train.py --model Qwen/Qwen2.5-3B-Instruct --steps 200 --trainers 2 --rollouts 4
```

Training needs no deploy step: `train.py` calls `app.init(...)` and declares the
services with `@app.service`, and the SDK packages the current directory; executors install it with the
dependencies in `pyproject.toml`.

**Configuration (driver):** model, steps, prompts per step, group size, learning
rate, KL coefficient, max search turns (default 4), max new tokens per turn,
retriever top-k (default 3), eval cadence, seed, checkpoint cadence.

**Interfaces between components** are methods on `@app.service` classes. A call
such as `rollout.generate(...)` returns an `ObjectFuture`; passing that future into
another service's call hands over the **reference**, not the data, so weights and
trajectories move through the object cache without passing through the driver.

| Service method | Arguments | Returns |
| --- | --- | --- |
| `Rollout.generate` | `prompts, weights (future or ref), n, max_turns` | `list[Trajectory]` (text, loss_mask, reward, turns) |
| `Trainer.update` | `trajectories (futures), step` | `Update` (weights ref, loss, kl, grad_norm) |

**Feature Interaction:**
- **Related features:** `flamepy.app` (services, futures as references), GPU selection (RFE565), object cache,
  session priority, `flmbench`/`flame-test` for load tests.
- **Updates required:** none to Flame core. Possible gaps found during the build
  (GPU `resreq` on app services, multi-GPU launch inside an instance, passing
  futures between services across sessions, reloading weights without restarting
  the instance) are verified in Phase 0 and 1 and filed separately.
- **Compatibility:** requires a Flame release with GPU executors and the object
  cache; no breaking changes.

## 3. Implementation Detail

### 3.1 Architecture

```
            flame-console                          Flame cluster
 +-----------------------------+  calls   +----------------------------------------+
 |  train.py                   |--------->| Rollout   (N x 1 GPU, vLLM)            |
 |  - sample prompts           |<---------|    |  local BM25 searches               |
 |  - fan out rollouts         |          |    v                                   |
 |  - rewards, advantages      |          | (BM25 index in each executor)         |
 |  - call learner, eval       |--------->| Trainer   (1 executor, 4 GPUs)         |
 +-----------------------------+ weights  +----------------------------------------+
                               (object cache)
```

Every box on the right is a `flamepy.app` service. The driver is a plain Python
process; it holds no GPU and no model. State that must survive a driver restart
(step counter, latest weights reference) is checkpointed to the object cache.

The driver declares the services in one application:

```python
app.init("sr1-train", dependencies=["vllm==...", "torch", "faiss-cpu", ...], python_version="3.12")

@app.service(resreq="cpu=8,mem=64g,gpu=1", warmup=8)                    class Rollout: ...
@app.service(resreq="cpu=32,mem=256g,gpu=4", autoscale=False, warmup=1) class Trainer: ...

trainer = Trainer.remote(model)
rollout = Rollout.remote(model, corpus)
```

`Service.remote(*args)` creates a proxy with its own session; the constructor runs
once per executor and the instance is retained across calls, which is what keeps
the vLLM engine, the BM25 index, and the optimizer state warm.

### 3.2 Components

**Retrieval.** There is no separate retriever service. Each rollout executor builds a BM25
index of the corpus in its constructor and keeps it for the executor's life, so a search is
a local call and a rollout never waits on another service. (An earlier version ran the
retriever as its own service; its tasks could not be scheduled while the rollouts that
needed them held every slot.) The demo corpus is the paragraphs of the training questions;
a larger index (e5 and FAISS) would call for a service again.

**Rollout (`Rollout`).** One executor holds one vLLM engine on one GPU
(`resreq` with `gpu=1`), following `examples/vllm/serving`. A `generate` call runs the
Search-R1 loop for a batch of prompts:

1. generate until a stop string (`</search>` or `</answer>`) or token budget;
2. if `<search>` was produced, search the index, append
   `<information>...</information>`, and continue (up to `max_turns`);
3. record the full text and a `loss_mask` that is 1 on model-generated tokens
   and 0 on prompt and retrieved tokens;
4. compute the EM reward from the final `<answer>`.

Before generating, the worker compares the weights reference with the version it
holds and reloads weights when it is newer (3.4). Rollout workers scale with the
service's `warmup` and `autoscale` settings; there is no other coordination.

**Trainer (`Trainer`).** Data-parallel GRPO: every executor holds a full replica (fp32
weights, Adam state, a frozen reference) on one GPU. Replicas may be on different nodes.
They form one NCCL group, each trains on a slice of the batch, and gradients are
all-reduced. The service is declared with `autoscale=False, warmup=N` so exactly N
instances exist.

*Forming the group without addressing executors:* the driver sends `join` N times at
once. `warmup=N` makes N the session's `min_instances`, so gang scheduling (RFE591) binds
all N executors or none, and every one of the N calls gets an executor. Each call registers the executor's id and address in a
shared cache object and blocks until N have registered, so no executor takes two. Rank is
the position of the executor id in the sorted list, and rank 0 is the rendezvous. No
executor needs to be addressed. `update` is
broadcast the same way: each executor gets the whole batch and trains on its own slice. Group-normalised advantages, token-level policy-gradient loss over
`loss_mask` tokens, a KL penalty to the frozen reference model, bf16 weights, and
FSDP across its GPUs. The implementation choice is open (3.5): a small in-tree
loop on PyTorch, or veRL's trainer driven from the executor. The executor retains
model and optimizer state between `update` calls, so a step costs only the
update. It writes the new weights to the object cache and returns the reference.
Multi-GPU launch from inside one service instance (spawned workers or `torchrun`
as a child process) is **(verify under the Host shim)**.

### 3.3 Training step

```
for step in range(steps):
    prompts = sample(train_set, prompts_per_step)
    session = rollout.session(weights_ref)             # tasks fan out over executors
    trajs   = gather(session.generate(chunk, n=group_size) for chunk in split(prompts))
    drop trajectories from failed tasks (retry once), keep step if >= 90% arrive
    result  = trainer.update(trajs, step)              # advantages computed here
    weights_ref = result.weights_ref
    log(loss, kl, mean reward, searches per answer, response length)
    every eval_every: run greedy rollouts on the dev set, log EM
    every ckpt_every: persist weights_ref and the driver state
```

The step is synchronous (rollout, then update); asynchronous off-policy rollout is
a later optimisation. Per-step cost is dominated by generation, which scales with
the rollout executor count.

### 3.4 Weight handoff

The trainer publishes bf16 weights (safetensors) as cache objects, one per shard of about
512 MB, and returns the references as a plain dict (a returned `ObjectRef` is taken as the task
result, and an `ObjectRef` argument is resolved on every call). The first export is a full part
per shard. Each later export `patch_object`s one more part per shard holding only the elements
whose bf16 value changed (`name.idx` and `name.val`), or the whole tensor when more than a third
changed. Measured on the cluster, shipping the whole model cost about 19 s per step (6.2 GB: 6 s
GPU to host copy, 6 s serialization, 7 s upload) plus about 12 s for each rollout executor to
download it, and most of that was bytes that had not changed.

A rollout worker reads each shard with `get_object`, whose client cache downloads only the parts
after the version it already has, applies the new parts to a CPU copy of the weights, and loads
the changed tensors into the running vLLM engine with `apply_model(load_weights)`, then resets
the prefix cache. An executor that joins late reads every part and applies them in order. Because
a checkpoint is the first k parts, one object per shard serves every checkpoint of a run. The
replicas are identical, so only rank 0 publishes; the driver calls every replica and takes the
one answer. `--sync-every k` bounds how often this happens. Call inputs and outputs are limited
to 128 KB, so trajectories and batches also go through the cache (`put_object` in services,
`app.put` in the driver; an `ObjectRef` argument is resolved before the call).

### 3.5 Open design decisions

| Decision | Option A (default) | Option B | Deciding experiment |
| --- | --- | --- | --- |
| Trainer | in-tree GRPO, data-parallel replicas over NCCL (built) | veRL trainer inside the executor | A is smaller and fully Flame-native; B matches Search-R1 closely. Try A first; fall back to B if A's reward curve cannot reproduce the trend. |
| Weight sync | object cache | shared filesystem | Measure shard transfer time on the cluster. |
| Retriever index | e5 + FAISS on a subset | BM25 only | Subset keeps retrieval quality meaningful at demo scale. |

### 3.6 Data

Training and dev sets are NQ and HotpotQA, as in Search-R1 **(verify the exact
splits and the preprocessing script)**, converted to `{id, question, answers}`
and stored as a cache object so every component reads the same snapshot.
Corpus, index, and model weights are downloaded once to a cache object or a
shared path, not on every executor start.

### 3.7 System considerations

- **Resources.** The cluster has 2 GPU nodes with 8 B200 GPUs each (180 GB per
  GPU). Plan: trainer 4 GPUs on one node; rollout 8 to 12 GPUs across both nodes;
  the index is in each rollout executor. A 3B model fits one GPU for generation with
  large headroom, so the limit is throughput, not memory.
- **Scheduling.** The trainer and rollout sessions use different applications so
  Flame's fair sharing keeps both progressing; the trainer session gets a higher
  priority so updates are not starved.
- **Reliability.** A failed rollout task is retried once, then dropped. A failed
  trainer task aborts the step and the driver resumes from the last checkpoint.
  The driver is restartable from its object cache state.
- **Observability.** The driver logs one line per step (reward, EM, loss, KL,
  response length, searches per answer, tokens/s, step seconds) to stdout and a
  JSON lines file; no hostnames or addresses are logged.
- **Security.** No node names, IPs, or credentials are committed; model and dataset access tokens come from Secrets.
- **Operations.** Hugging Face and dataset downloads need network access from the
  executors, or the files are pre-staged. Each `app.init` makes executors install
  its `dependencies`; a cold start downloads vLLM and PyTorch, so first runs are
  slow and later ones reuse the installed environment.

**Dependencies:** vLLM (pinned as in `examples/vllm/serving`), PyTorch, a FAISS
build, `sentence-transformers` or the e5 model, `datasets`.

## 4. Phases and acceptance

| Phase | Deliverable | Acceptance |
| --- | --- | --- |
| 0 | Rollout and trainer services declared with `flamepy.app`, single-GPU rollout of the untrained model; verify the `flamepy.app` points listed under "Updates required" | Base-model EM measured on the dev set; retriever answers a query in tens of ms |
| 1 | Trainer and driver on a tiny run (50 prompts, 5 steps) | Loss and reward logged; weights round-trip through the cache; step time breakdown recorded |
| 2 | Scale rollout executors; full training run | EM above the base model on the dev set; a rollout executor killed mid-run does not stop the run |
| 3 | README, `flame-test` smoke case: a tiny training run through `train.py` | A new user can reproduce Phases 0 to 2 from the README |

## 5. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| In-tree GRPO does not learn | No improvement over base | Fall back to veRL trainer; keep the rollout component unchanged |
| Weight reload dominates step time | Slow training | Reload every k steps; shard the transfer |
| Corpus download or index build is slow | Phase 0 delayed | Subset and BM25 smoke path; stage files once |
| Executors cannot reach the model hub | Cannot load models | Pre-stage into a cache object |

## References

- Search-R1: <https://github.com/PeterGriffinJin/Search-R1>
- veRL: <https://github.com/volcengine/verl>
- `sdk/python/src/flamepy/app/client.py` (`init`, `service`, `ServiceInstance`), `examples/vllm/serving`, `examples/rl/basic`, `docs/designs/RFE565-gpu-selection/FS.md`

## 6. Notes from the implementation

- **Replay buffer.** Not needed. Rollouts are generated by the current policy and each batch
  is used for one gradient step and then discarded (synchronous, on-policy GRPO). A buffer
  would be needed for asynchronous rollouts or several update epochs per batch, which need
  off-policy corrections.
- **Dependencies.** A component's dependencies come from its `pyproject.toml`; `flamepy.app`
  ignores the `dependencies` argument of `app.init` when the packaged directory has one.

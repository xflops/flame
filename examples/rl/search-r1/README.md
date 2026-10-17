# Search-R1 on Flame

Train a small model to search while it reasons ([Search-R1](https://github.com/PeterGriffinJin/Search-R1)).
The RL training (rollouts and policy updates) runs on Flame GPU executors. Design:
[RFE584](../../../docs/designs/RFE584-search-r1-demo/FS.md).

The model learns the Search-R1 protocol: it writes `<think>`, calls `<search>query</search>`,
reads `<information>...</information>` from a retriever, and finishes with `<answer>...</answer>`.
The reward is exact match against the gold answer (HotpotQA questions; the retrieval corpus is
the paragraphs of those questions, searched with BM25).

## Layout

```
pyproject.toml     the `sr1` project and the dependencies executors install
src/               train.py (RL driver), rollout.py, trainer.py (the services),
                   data.py delta.py engine.py grpo.py index.py runtime.py (prompt and reward, weight
                   deltas, search loop, advantage maths, BM25, the Flame application)
tests/             unit tests that need no GPU
```

The driver creates the Flame application and services itself (`flamepy.app`); there is no deploy
step. Flame packages the current directory and executors install it with the dependencies in
`pyproject.toml`, so run from this directory.

## Train

Requirements: a Flame cluster with GPU executors, the Flame SDK, and `datasets` on the client.

```sh
python3 src/train.py --model Qwen/Qwen2.5-3B-Instruct --steps 100 \
    --prompts-per-step 64 --group-size 5 --trainers 2 --rollouts 4
```

What runs where, all as Flame services:

- **Rollout**: one vLLM engine per GPU executor runs the search loop for a few prompts per call.
  Each executor also holds the BM25 index of the corpus in memory, so a search is a local call.
  It is autoscaled: `--rollouts` executors start up front and more are added while calls are
  pending. `--prompts-per-task` sets the calls per step.
- **Trainer**: GRPO with fp32 master weights. `--trainers N` runs N data-parallel replicas, one GPU
  each, on any nodes; they form one NCCL group, split every batch, and all-reduce gradients.
- **Weights**: after each step (`--sync-every`) the trainer publishes its bf16 weights to the Flame
  object cache as a delta: the first export stores every tensor, each later one appends only the
  elements whose bf16 value changed. Rollout executors download the new parts, apply them to a CPU
  copy and load the changed tensors into the engine in place.

The driver logs one JSON line per step (reward, answered rate, searches per answer, loss, KL, step
time) to `sr1-out/metrics.jsonl`, evaluates exact match on held-out questions every
`--eval-every` steps, and writes `sr1-out/checkpoint.json`, which names the weight objects in
the cache.

## Limits

- One model size (3B) and one GPU per replica; a replica holds fp32 weights, Adam state and a
  frozen reference model.
- Rollout and update alternate (synchronous GRPO). Each batch is used once, so no replay buffer.
- If a trainer executor is lost, the NCCL group hangs; restart the run from the last checkpoint.

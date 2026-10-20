# Search-R1 training on Flame

## Assessment

Search-R1-style RL training (GRPO on Qwen2.5-3B-Instruct, HotpotQA questions, BM25 retrieval) runs end to end as `flamepy.app` services on a Flame Kubernetes cluster with two 8-GPU nodes: rollouts on up to 8 GPU executors, a 2-replica data-parallel trainer, and weights moved between them through the Flame object cache. The run in progress at the time of writing had completed 9 to 10 steps when these numbers were taken; it is not a finished training run, so this report makes no claim about final accuracy.

The main finding is about weight synchronisation. Shipping the whole model to the cache after every step cost about 19 s on the trainer and about 12 s of download plus 3 s of loading on every rollout executor, together about half of a 71 s step. Only 1.2 to 1.5 % of the bf16 weights change in a step, so the trainer now publishes the changed elements only, as a patch part on the cache object. In steady state a step takes about **38 s instead of 71 s (1.8 times faster)** and cache-related time falls from about 19 s to about 2 s. The change is lossless.

## Setup

- **Model and algorithm:** Qwen2.5-3B-Instruct, GRPO, bf16 weight publication, fp32 master weights on the trainer, vLLM 0.31.0 for rollouts. Learning rate 1e-6, KL coefficient 0.001.
- **Workload per step:** 64 prompts, group size 5 (320 samples), up to 4 search turns, 6 rollout executors started up front and autoscaled (8 were running), 2 trainer replicas of one GPU each.
- **Cluster:** Flame built from the current main branch images. The session manager runs on a CPU node; the two GPU nodes (8 GPUs and about 2 TiB of memory each) each run one executor manager and one object cache. Each cache keeps its data in a memory-backed volume with a 256 GiB limit and eviction set to 200 GB.
- **Retrieval:** a BM25 index of the questions' paragraphs, built inside each rollout executor.

## Weight synchronisation

Same workload, same cluster, two implementations. "Full" publishes all 6.17 GB of bf16 weights every step (12 shards of about 512 MB, one after another). "Delta" publishes the whole model once, then appends one part per step holding only the elements whose bf16 value changed. Averages over steady-state steps (full: steps 3 to 10; delta: steps 3 to 9).

| Metric | Full | Delta |
| --- | ---: | ---: |
| Step time | 70.8 s | 38.4 s |
| Rollout phase | 39.3 s | 25.6 s |
| Policy update | 12.8 s | 12.0 s |
| Export on the trainer | 18.7 s | 0.6 s |
| Rollouts: weights download | 12.0 s | 1.1 s |
| Rollouts: weights download and loading into vLLM | 15.4 s | 1.9 s |
| Bytes published per step | 6.17 GB | about 0.28 GB |
| Share of weights changed per step | not applicable | 1.2 to 1.5 % |

The first sync of a rollout executor still downloads the whole model (17 s in step 2 of the delta run). A late-joining or autoscaled executor does the same, reading every part and applying them in order.

### Where the full-weights export went

Per step, trainer side, 6.17 GB: 6.4 s GPU-to-host copy and bf16 conversion, 5.6 s safetensors serialisation, 6.7 s upload (about 0.9 GB/s, single stream). Compression was not involved. Every step also read about 49 GB out of the cache, 6.17 GB for each of 8 executors.

### How the delta works

- Tensors are assigned to shards once. The first export is one object per shard holding every tensor.
- Each later export compares the current bf16 tensors with the previously published ones on the GPU and appends one part per shard: `name.idx` (int32 flat indices) and `name.val` (new values) for the changed elements, or the whole tensor when more than a third changed, since an index plus a value costs 6 bytes against 2.
- A rollout executor keeps a CPU copy of the weights. A get through the client cache downloads only the parts after the version it already holds; the executor applies them to its copy and loads only the changed tensors into the engine.
- The replicas are identical, so only rank 0 publishes. A checkpoint is the first k parts of each shard object.

## Where a step goes

Step 2 of the full-weights run, 71.7 s, split by what the time is spent on:

| Resource | Seconds | Share |
| --- | ---: | ---: |
| GPU compute (generation 14.8 s, policy update 12.0 s) | about 27 | 37 % |
| Object cache (weights download 12.1 s, upload 6.7 s) | about 19 | 26 % |
| Host copies (GPU to host 6.4 s, serialisation 5.6 s, loading into vLLM 3.5 s) | about 15.5 | 22 % |
| Not measured: trajectory upload and download, task dispatch, waiting on the slowest call | about 10 | 14 % |

With delta sync the cache and host-copy shares nearly disappear, and generation (13 to 19 s) and the policy update (about 12 s) dominate. The unmeasured remainder is unchanged and likely includes more cache time, because every rollout call writes its trajectories to the cache and the driver reads them back.

## Training progress (interim)

Held-out exact match of the untrained model on the 200-question dev subset was 0.13 in one run and 0.135 in the other. Mean reward on the training samples, by step:

| Step | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Full weights | 0.24 | 0.33 | 0.34 | 0.31 | 0.23 | 0.33 | 0.33 | 0.33 | 0.38 | 0.42 |
| Delta | 0.30 | 0.29 | 0.34 | 0.34 | 0.31 | 0.32 | 0.29 | 0.30 | 0.40 | not yet |

The two runs use different random samples, so step-to-step values are noisy and the table only shows that the delta path does not visibly disturb training. About 93 to 98 % of samples produce an answer and the average is about one search per answer.

## Cluster findings

- **Disk pressure from the application install.** Each node that runs any service of the application installs its dependencies. For vLLM and PyTorch that is about 8.4 GB of packages plus about 8.1 GB of uv cache per node, stored twice because the installer copies packages and the cache sits on a separate volume. On the 39 GB CPU-only nodes this took disks to 81 to 83 % full; one node crossed the kubelet's eviction threshold, its executor manager pod was evicted, the node was cordoned and its executors were deleted. The cluster now schedules executors on the two large-disk GPU nodes only.
- **Releases are never pruned.** Every new package hash creates a new release directory with a full install (about 8.5 GB here), and nothing removes old ones. Two runs left two releases on each GPU node; they were removed by hand. A follow-up should prune old releases of an application when a new one installs, keeping any that a running executor uses.
- **A separate retriever service can starve.** An early version ran retrieval as its own service. Its tasks stayed pending while the rollouts that waited on them held the executors, so the run stalled. Retrieval now runs inside each rollout executor.
- **Stale executors in `Void` state.** After the executor manager pods were replaced, executors recorded in the session manager stayed in `Void` for several minutes and blocked allocation for the session that owned them, although the node's executor manager had no record of them. Restarting the node's executor manager made the session manager release them as orphans. The cause is not identified.
- **Scheduling GPU sessions works.** A plain test session requesting one GPU ran on a GPU node in 66 ms once the cluster was healthy.

## Limitations

- The training run was still in progress. Accuracy after the full 100 steps, and any comparison against the untrained model beyond the 0.13 baseline, are not reported here.
- One model size (3B), one GPU per trainer replica, synchronous on-policy GRPO.
- Delta sync was measured with a learning rate of 1e-6. A larger learning rate changes more elements per step and shrinks the saving; above a third of a tensor the whole tensor is sent.
- The unmeasured part of a step (trajectory transfers through the cache, scheduling, stragglers) is about 10 s and has not been split.
- Only one run of each variant was measured; the step-time averages come from 7 to 8 steps each.

## Reproduce

The example is `examples/rl/search-r1` and the design is `docs/designs/RFE584-search-r1-demo/FS.md`. From the example directory, with a Flame client and `datasets` installed:

```sh
python3 src/train.py --model Qwen/Qwen2.5-3B-Instruct --steps 100 \
    --prompts-per-step 64 --group-size 5 --rollouts 6 --trainers 2
```

Per-step metrics, including `export_seconds`, `export_encode_s`, `export_put_s`, `export_bytes`, `export_changed`, `weights_get_max_s`, `weights_load_max_s` and `generate_max_s`, are written to `sr1-out/metrics.jsonl`.

# Distributed Replay Buffer with patch_object

`ReplayBuffer` wraps an `ObjectRef` - collectors call `buffer.push()` which uses `patch_object` for efficient writes, while the buffer service handles state queries and sampling.

## Key Pattern: Shared Object + patch_object

```python
with Runner("replay-buffer") as rr:
    # Create ReplayBuffer (creates ObjectRef internally via runner)
    buffer = ReplayBuffer(rr)
    
    # Wrap as a fixed multi-instance service for state/sample operations
    buffer_svc = rr.service(buffer, warmup=sample_parallelism)
    collector = rr.service(Collector(env_name), autoscale=True)

    # Pass the SAME buffer object to collectors (pickled with its ObjectRef)
    collect_futures = [collector.collect(buffer, num_steps) for _ in range(num_collections)]
    rr.get(collect_futures)

    # Query state and sample from the fixed-size buffer service
    stats = buffer_svc.state().get()
    sample_futures = [buffer_svc.sample(size) for size in sample_request_sizes]
    batches = rr.get(sample_futures)
```

## Why This Pattern?

| Approach | Data Flow | Network Hops |
|----------|-----------|--------------|
| Service only | Collector → Service → Buffer | 2 per worker |
| **patch_object** | Collector → ObjectRef (cache) | 1 per worker |

- `ReplayBuffer` holds an `ObjectRef` pointing to shared data in Flame cache
- When pickled as parameter, collectors get the same `ObjectRef`
- `buffer.push()` calls `patch_object` - writes directly to cache
- `get_object` returns only new patch batches after the buffer service has a local cache entry
- The fixed-size buffer service can run sample requests on different instances
- Each buffer service instance keeps its own materialized buffer and merges only newly seen patch batches for requests that land on that instance

## Usage

### Distributed Mode

```shell
docker compose exec -it flame-console /bin/bash
cd /opt/examples/rl/replay_buffer
uv run main.py
```

### Local Mode

```shell
uv run main.py --local
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--env` | Gymnasium environment | CartPole-v1 |
| `--local` | Run without Flame cluster | Off |
| `--iterations` | Collection iterations | 50 |
| `--collections` | Collections per iteration | 20 |
| `--steps-per-collection` | Steps per collection task | 500 |
| `--batch-size` | Sample batch size | 64 |
| `--sample-parallelism` | Fixed replay-buffer service instances and distributed sample requests per iteration | 2 |
| `--metrics-json` | Write distributed-mode metrics to a JSON file | Off |
| `--merge-every` | Override replay-buffer patch merge cadence | Auto |
| `--no-merge` | Disable patch merging, including forced-full runs | Off |
| `--force-full-get` | Force replay-buffer reads to request full objects with version 0 | Off |

## Performance Comparison

Run the baseline and incremental cases on the same cluster, code revision, and workload shape. The baseline forces every replay-buffer read to request version `0`, so the cache returns the full base object plus all patches and the example keeps the default merge cadence of every 5 iterations. The incremental case uses the cached nonzero version after the first read, disables merge by default, and each buffer service instance applies only newly seen patch batches to its own local materialized buffer.

```shell
docker compose exec -it flame-console /bin/bash
cd /opt/examples/rl/replay_buffer
mkdir -p /tmp/replay-buffer-metrics

uv run main.py \
  --force-full-get \
  --metrics-json /tmp/replay-buffer-metrics/full.json \
  --iterations 50 \
  --collections 20 \
  --steps-per-collection 500 \
  --batch-size 64

uv run main.py \
  --metrics-json /tmp/replay-buffer-metrics/incremental.json \
  --iterations 50 \
  --collections 20 \
  --steps-per-collection 500 \
  --batch-size 64

uv run main.py \
  --local \
  --iterations 50 \
  --collections 20 \
  --steps-per-collection 500 \
  --batch-size 64
```

Compare total throughput and read-path latency:

```shell
python - <<'PY'
import json
import statistics

full = json.load(open("/tmp/replay-buffer-metrics/full.json"))
incremental = json.load(open("/tmp/replay-buffer-metrics/incremental.json"))


def median_metric(report, key):
    values = [row[key] for row in report["iterations"] if row[key] > 0]
    return statistics.median(values) if values else 0.0


def pct_improvement(old, new):
    return 0.0 if old == 0 else (old - new) / old * 100.0


for label, report in [("full", full), ("incremental", incremental)]:
    print(
        f"{label:12} throughput={report['summary']['throughput']:.1f}/s "
        f"median_state={median_metric(report, 'state_secs'):.4f}s "
        f"median_sample={median_metric(report, 'sample_secs'):.4f}s"
    )

print(
    "state improvement: "
    f"{pct_improvement(median_metric(full, 'state_secs'), median_metric(incremental, 'state_secs')):.1f}%"
)
print(
    "sample improvement: "
    f"{pct_improvement(median_metric(full, 'sample_secs'), median_metric(incremental, 'sample_secs')):.1f}%"
)
PY
```

This default comparison intentionally keeps merge enabled for the forced-full baseline and disabled for incremental reads. Use `--merge-every` only when you want to override that policy.

Use `--sample-parallelism N` when you want the learner to issue `N` replay-buffer sample requests in parallel. `ReplayBuffer` is passed as an instance, so `Runner.service()` defaults to a fixed-size service; `warmup=N` creates `N` service instances without adding in-instance threading.

Example result from a 500,000-transition run after disabling merge for the incremental case:

| Mode | Total Time | Throughput |
|------|------------|------------|
| Local baseline | 2.24s | 223,376.4 transitions/sec |
| Incremental reads | 6.58s | 76,016.1 transitions/sec |
| Forced full reads | 71.84s | 6,959.5 transitions/sec |

| Comparison | Runtime | Throughput |
|------------|---------|------------|
| Incremental vs forced full | 90.8% lower | 10.9x higher |
| Local vs incremental distributed | 66.0% lower | 2.9x higher |
| Local vs forced full | 96.9% lower | 32.1x higher |

The local baseline remains faster than incremental distributed reads because it avoids Flame service, scheduler, network, and cache overhead. Exact numbers depend on cluster size, cache placement, and environment stepping cost, so compare runs from the same cluster and workload shape.

## Example Output

```
============================================================
Distributed Replay Buffer (patch_object)
============================================================

Configuration:
  Environment: CartPole-v1
  Collections per iteration: 20
  Steps per collection: 500
  Iterations: 50
  Batch size: 64
  Sample parallelism: 2
  Merge every: disabled
  Force full get: False

Starting distributed collection...
Iteration  0 | Buffer:  10000 | Total added:  10000 | Avg Reward:    21.4
             | Sampled 64 transitions across 2 request(s)
Iteration  1 | Buffer:  20000 | Total added:  20000 | Avg Reward:    22.0
             | Sampled 64 transitions across 2 request(s)
...

============================================================
Collection Complete!
  Total time: 6.58s
  Total transitions: 500000
  Throughput: 76016.1 transitions/sec
============================================================
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Main (Learner)                         │
│                                                             │
│  buffer_svc.state()              buffer_svc.sample(size) x N│
│       │                                 │                   │
└───────┼─────────────────────────────────┼───────────────────┘
        │                                 │
        ▼                                 ▼
┌─────────────────────────────────────────────────────────────┐
│            Fixed-Size ReplayBuffer Service                  │
│                                                             │
│  Wraps ObjectRef - handles push/state/sample                │
│                                                             │
│  push(transitions) - patch_object to ObjectRef              │
│  state() - get buffer stats (size, total_added)             │
│  sample(batch_size) - random sample from buffer             │
│                                                             │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                  ObjectRef (Flame Cache)                    │
│                                                             │
│  {"transitions": [...], "total_added": N}                   │
│                                                             │
└──────────────────────────┬──────────────────────────────────┘
                           ▲
        ┌──────────────────┼──────────────────┐
        │                  │                  │
        │ buffer.push()    │ buffer.push()    │ buffer.push()
        │                  │                  │
┌───────┴─────┐    ┌───────┴─────┐    ┌───────┴─────┐
│ Collector 1 │    │ Collector 2 │    │ Collector N │
│    (env)    │    │    (env)    │    │    (env)    │
└─────────────┘    └─────────────┘    └─────────────┘
                    Flame Cluster
```

# Distributed Replay Buffer with patch_object

`ReplayBuffer` wraps an `ObjectRef` - collectors call `buffer.push()` which uses `patch_object` for efficient writes, while the buffer service handles state queries and sampling.

## Key Pattern: Shared Object + patch_object

```python
with Runner("replay-buffer") as rr:
    # Create ReplayBuffer (creates ObjectRef internally via runner)
    buffer = ReplayBuffer(rr)
    
    # Wrap as service for state/sample operations
    buffer_svc = rr.service(buffer, autoscale=False)
    collector = rr.service(Collector(env_name), autoscale=True)

    # Pass the SAME buffer object to collectors (pickled with its ObjectRef)
    collect_futures = [collector.collect(buffer, num_steps) for _ in range(num_collections)]
    rr.get(collect_futures)

    # Query state and sample from service
    stats = buffer_svc.state().get()
    batch = buffer_svc.sample(batch_size).get()
```

## Why This Pattern?

| Approach | Data Flow | Network Hops |
|----------|-----------|--------------|
| Service only | Collector → Service → Buffer | 2 per worker |
| **patch_object** | Collector → ObjectRef (cache) | 1 per worker |

- `ReplayBuffer` holds an `ObjectRef` pointing to shared data in Flame cache
- When pickled as parameter, collectors get the same `ObjectRef`
- `buffer.push()` calls `patch_object` - writes directly to cache
- `get_object` with deserializer consolidates patches when reading

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
| `--metrics-json` | Write distributed-mode metrics to a JSON file | Off |
| `--merge-every` | Merge replay-buffer patches every N iterations | 5 |
| `--no-merge` | Disable patch merging to stress patch-only reads | Off |
| `--force-full-get` | Force replay-buffer reads to request full objects with version 0 | Off |

## Performance Comparison

Run the baseline and incremental cases on the same cluster, code revision, and workload shape. The baseline forces every replay-buffer read to request version `0`, so the cache returns the full base object plus all patches. The incremental case uses the cached nonzero version after the first read, so the cache can return only new patches when the base is still valid.

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

For a stronger patch-only signal, repeat both runs with `--no-merge`. That keeps a long patch history and should make the forced-full baseline pay for old patches on every read, while the incremental run downloads only the patch suffix after the cached version.

## Example Output

```
============================================================
Distributed Replay Buffer (patch_object)
============================================================

Configuration:
  Environment: CartPole-v1
  Collections per iteration: 4
  Steps per collection: 100
  Iterations: 10
  Batch size: 32

Starting distributed collection...
Iteration  0 | Buffer:    400 | Total added:    400 | Avg Reward:    22.5
             | Sampled batch of 32 transitions
Iteration  1 | Buffer:    800 | Total added:    800 | Avg Reward:    21.8
             | Sampled batch of 32 transitions
...

============================================================
Collection Complete!
  Total time: 2.45s
  Total transitions: 4000
  Throughput: 1632.7 transitions/sec
============================================================
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Main (Learner)                         │
│                                                             │
│  buffer_svc.state()              buffer_svc.sample(batch)   │
│       │                                 │                   │
└───────┼─────────────────────────────────┼───────────────────┘
        │                                 │
        ▼                                 ▼
┌─────────────────────────────────────────────────────────────┐
│                  ReplayBuffer Service                       │
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

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
| `--iterations` | Collection iterations | 10 |
| `--collections` | Collections per iteration | 4 |
| `--steps-per-collection` | Steps per collection task | 100 |
| `--batch-size` | Sample batch size | 32 |

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

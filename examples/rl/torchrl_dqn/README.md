# TorchRL DQN with Flame Runner

This example adapts the TorchRL introduction tutorial's CartPole DQN loop to Flame.
The learner stays local and uses TorchRL `QValueActor`, `TensorDict`, and `DQNLoss`.
Flame replaces the tutorial's local collector with distributed rollout workers and
keeps TorchRL's `ReplayBuffer` interface for replay. The distributed buffer is a
raw `ReplayBuffer` Runner service backed by a Flame-aware TorchRL storage.

Reference: <https://docs.pytorch.org/rl/stable/tutorials/torchrl_demo.html>

## Pattern

The tutorial loop is:

```text
Collector -> ReplayBuffer -> sample -> DQNLoss -> optimizer
```

The Flame version is:

```text
Runner collector service -> ReplayBuffer(FlameObjectStorage) service -> DQNLoss -> optimizer
```

- `FlameTorchRLCollector` is a stateful Runner worker. Each call loads the latest policy weights, steps a discrete-action Gymnasium environment, and writes transitions with `ReplayBuffer.extend()`.
- `FlameObjectStorage` is a custom TorchRL `Storage` that stores its backing state in a Flame object created by `Runner.put_object()`.
- Rollout batches stay as TorchRL `TensorDict` objects through collection, replay sampling, and `DQNLoss`.
- TorchRL `ReplayBuffer.extend()` writes call `FlameObjectStorage.set()`, which appends transition batches with `patch_object()` instead of sending them through a central service.
- TorchRL `ReplayBuffer.sample()` reads through `FlameObjectStorage.get()`, which calls `get_object(..., deserializer=...)` to incrementally materialize new Flame object patches.
- Replay state reads use a count-only deserializer, so the driver can check shard sizes without rebuilding sampled transition data.
- The driver exposes raw TorchRL `ReplayBuffer` objects as Runner services for sampling and performs the TorchRL loss and optimizer step locally.
- `--replay simple` keeps one shared `ReplayBuffer(FlameObjectStorage)` path.
- `--replay sharded` creates multiple raw `ReplayBuffer(FlameObjectStorage)` services, spreads collector writes across them, and samples shards independently.
- `--local` uses the same collector and transition-to-`TensorDict` learner path without a Flame cluster.

## Files

- `main.py`: CLI, local training loop, and distributed Flame Runner loop.
- `collector.py`: Runner-compatible rollout worker.
- `replay_buffer.py`: custom TorchRL storage backed by Flame `ObjectRef` plus local and sharded replay helpers.
- `model.py`: TorchRL policy, DQN loss, and transition batch helpers.
- `pyproject.toml`: Runtime dependencies and example package metadata.

## Usage

### Distributed Mode

Start a Flame cluster, open the console, and run:

```shell
docker compose exec -it flame-console /bin/bash
cd /opt/examples/rl/torchrl_dqn
uv run main.py
```

### Local Mode

```shell
cd examples/rl/torchrl_dqn
uv run main.py --local --env acrobot --iterations 5
```

## Options

| Flag | Description | Default |
|------|-------------|---------|
| `--env` | Gymnasium env or preset alias: `cartpole`, `acrobot`, `mountaincar`, `lunarlander` | `CartPole-v1` |
| `--local` | Run without Flame | Off |
| `--iterations` | Training iterations | 20 |
| `--collections` | Collection tasks per iteration | 4 |
| `--frames-per-collection` | Frames per collector call | 100 |
| `--batch-size` | Replay sample batch size | 64 |
| `--buffer-size` | Maximum replay-buffer transitions materialized | 10000 |
| `--hidden-dim` | Q-network hidden dimension | 128 |
| `--optim-steps` | Optimizer steps per iteration | 1 |
| `--warmup-frames` | Replay frames required before training | 100 |
| `--target-update-tau` | Soft target-network update coefficient | 0.05 |
| `--replay` | Replay-buffer implementation: `simple` or `sharded` | `simple` |
| `--replay-shards` | Shards for `sharded` replay | 4 |
| `--sample-work` | Optional CPU work units per sampled transition | 0 |
| `--sample-parallelism` | Replay-buffer service instances and sample requests | 1 |
| `--metrics-json` | Write timing and throughput metrics | Off |
| `--seed` | Torch and environment seed; `-1` disables explicit seeding | 0 |

## Heavier Environments

DQN requires a discrete action space. This example supports any Gymnasium env with
a `Box` observation space and `Discrete` action space. The useful built-in presets are:

| Alias | Gymnasium env | Notes |
|-------|---------------|-------|
| `cartpole` | `CartPole-v1` | Fast tutorial baseline; overhead usually dominates. |
| `acrobot` | `Acrobot-v1` | Longer classic-control episodes; better for distributed smoke tests without extra dependencies. |
| `mountaincar` | `MountainCar-v0` | Discrete but still cheap; useful as another shape check. |
| `lunarlander` | `LunarLander-v3` | Heavier discrete env; requires `uv run --extra box2d ...`. |

Continuous MuJoCo environments such as Ant or Hopper are intentionally not supported
here because DQN is a discrete-action algorithm. Use a TorchRL PPO/SAC example for
those.

## Comparison Commands

Use the same workload shape and compare the generated metrics:

```shell
mkdir -p /tmp/torchrl-flame

uv run main.py \
  --local \
  --env acrobot \
  --iterations 20 \
  --collections 8 \
  --frames-per-collection 500 \
  --metrics-json /tmp/torchrl-flame/local.json

uv run main.py \
  --env acrobot \
  --iterations 20 \
  --collections 8 \
  --frames-per-collection 500 \
  --metrics-json /tmp/torchrl-flame/flame.json
```

## Replay Buffer Modes

Single-shard replay sampling is intentionally cheap, so parallel sample calls
usually do not help in `--replay simple`. The mode is still useful as a baseline:

```shell
uv run main.py \
  --env acrobot \
  --iterations 20 \
  --collections 8 \
  --frames-per-collection 500 \
  --sample-parallelism 1 \
  --metrics-json /tmp/torchrl-flame/flame-simple.json
```

Use `sharded` to exercise a replay path where collectors write to one of several
raw `ReplayBuffer(FlameObjectStorage)` instances and the learner samples those
ReplayBuffer services in parallel. Add `--sample-work` when you want sampling
itself to be CPU-bound; this synthetic work runs inside the storage `get()` path
that TorchRL invokes during `ReplayBuffer.sample()`, modeling replay operations
such as decompression, frame stacking, sequence assembly, or augmentation.
The run prints aggregate collect, state, sample, optimize, and iteration timings
so you can see whether sampling is actually the bottleneck.

```shell
uv run main.py \
  --env acrobot \
  --iterations 20 \
  --collections 8 \
  --frames-per-collection 500 \
  --replay sharded \
  --replay-shards 4 \
  --sample-parallelism 4 \
  --sample-work 4096 \
  --metrics-json /tmp/torchrl-flame/flame-sharded.json
```

## Example Output

```text
========================================================================
TorchRL DQN with Flame Runner and Replay Buffer
========================================================================

Configuration:
  Environment: Acrobot-v1
  Iterations: 20
  Collections per iteration: 4
  Frames per collection: 100
  Total frames: 8000
  Batch size: 64
  Replay buffer size: 10000
  Replay mode: sharded
  Replay shards: 4
  Sample work: 4096
  Sample parallelism: 4
  Optim steps per iteration: 1
  Target update tau: 0.05

Starting distributed training...
Iteration   0 | epsilon=0.200 | buffer=  400 | total=   400 | reward=-134.0 | loss=0.9112
Iteration   1 | epsilon=0.191 | buffer=  800 | total=   800 | reward=-121.0 | loss=0.7446
...
```

## Notes

This is intentionally close to the TorchRL tutorial rather than a production DQN.
It keeps exploration simple with an epsilon schedule, uses TorchRL's soft target
network updater, and does one optimizer step per iteration by default. Increase
`--optim-steps`, `--collections`, or `--frames-per-collection` to put more work
behind each Runner scheduling round trip.

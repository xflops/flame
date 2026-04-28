# Reinforcement Learning Example (Python)

## Motivation

Reinforcement Learning (RL) training is computationally intensive, with episode collection (rollouts) being a major bottleneck. Each episode requires running the environment simulation, which can be slow for complex environments. Fortunately, episode collection is **embarrassingly parallel** — each episode is independent and can run on a separate worker.

By leveraging the `flamepy.Runner` API, we can distribute episode collection across multiple executors in a Flame cluster, dramatically speeding up training while keeping the policy update logic centralized. This pattern is common in distributed RL systems like IMPALA, Ape-X, and SEED.

This example illustrates:
- How to parallelize RL episode collection using Flame Runner
- The actor-learner pattern: distributed actors collect experience, centralized learner updates policy
- How to serialize and broadcast PyTorch model weights to remote workers
- Clean separation between distributed data collection and local gradient computation

## Overview

This example implements the REINFORCE (policy gradient) algorithm on environments from Gymnasium, with distributed episode collection using Flame.

### Supported Environments

| Environment | Type | Observation | Action | Episode Time |
|-------------|------|-------------|--------|--------------|
| `cartpole` | Discrete | 4 | 2 | ~1ms |
| `halfcheetah` | Continuous (MuJoCo) | 17 | 6 | ~20ms |
| `hopper` | Continuous (MuJoCo) | 11 | 3 | ~15ms |
| `walker2d` | Continuous (MuJoCo) | 17 | 6 | ~20ms |
| `ant` | Continuous (MuJoCo) | 105 | 8 | ~50ms |

### How It Works

1. **Policy Network**: Neural networks that output action distributions:
   - `DiscretePolicy`: For CartPole (categorical distribution)
   - `ContinuousPolicy`: For MuJoCo environments (Gaussian distribution with learned std)

2. **Distributed Episode Collection**: Using `flamepy.Runner`, we create a service from the `collect_episode` function. Each call to this service runs on a remote executor that:
   - Creates its own Gymnasium environment instance
   - Loads the current policy weights (serialized and sent from the learner)
   - Runs one complete episode
   - Returns the collected experience (states, actions, rewards)

3. **Centralized Policy Update**: After collecting experiences from all parallel episodes, the learner:
   - Computes discounted rewards
   - Calculates policy gradients
   - Updates the policy network locally

4. **Iteration**: The process repeats — broadcast new weights, collect more episodes, update again.

### Files

- **`main.py`**: REINFORCE training (distributed by default, use `--local` for local mode)
- **`pyproject.toml`**: Package dependencies including `torch`, `gymnasium[mujoco]`, and `flamepy`
- **`README.md`**: This documentation file

### Key Benefits

- **Linear Speedup**: Collecting N episodes in parallel takes roughly the same time as collecting 1 episode
- **Minimal Code Changes**: The episode collection function is almost identical to single-threaded code
- **Scalability**: Works with any number of executors — just change `episodes_per_iteration`
- **Flexibility**: Includes local training mode for development and testing without a cluster

## Usage

### Prerequisites

Start the Flame cluster with Docker Compose:

```shell
$ docker compose up -d
```

### Running Distributed Training

Log into the flame-console and run the example:

```shell
$ docker compose exec -it flame-console /bin/bash
root@container:/# cd /opt/examples/rl
root@container:/opt/examples/rl# uv run main.py
```

### Command Line Options

```shell
# Distributed training with CartPole (default)
uv run main.py

# Distributed training with MuJoCo environments
uv run main.py --env ant
uv run main.py --env halfcheetah
uv run main.py --env hopper
uv run main.py --env walker2d

# Local training (no Flame cluster required)
uv run main.py --local
uv run main.py --env ant --local

# Custom training configuration
uv run main.py --env ant --iterations 50 --episodes-per-iter 50

# Show training plot (requires matplotlib)
uv run main.py --plot
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--env` | Environment: cartpole, halfcheetah, hopper, walker2d, ant | cartpole |
| `--local` | Run local training (no Flame cluster) | Off |
| `--iterations` | Number of training iterations | 100 |
| `--episodes-per-iter` | Parallel episodes per iteration | 100 |
| `--plot` | Show reward plot after training | Off |

## Example Output

### Distributed Training (MuJoCo Ant)

```shell
root@container:/opt/examples/rl# uv run main.py --env ant --iterations 20
============================================================
Distributed REINFORCE on Ant-v5 using Flame Runner
============================================================

Configuration:
  Environment: Ant-v5
  Observation dim: 105
  Action dim: 8
  Continuous actions: True
  Training iterations: 20
  Episodes per iteration: 100
  Total episodes: 2000

Starting distributed training...
Iteration   0 | Mean Reward:   -431.5 | Loss: 0.7285
Iteration  10 | Mean Reward:   -138.8 | Loss: 2.4785
Iteration  19 | Mean Reward:   -122.4 | Loss: -7.4812

============================================================
Training Complete!
  Total time: 85.23s
  Episodes: 2000 (23.5 episodes/sec)
  Final Mean Reward: -122.4
============================================================
```

### Local Training

```shell
root@container:/opt/examples/rl# uv run main.py --env ant --iterations 20 --local
============================================================
Local REINFORCE on Ant-v5
============================================================

Configuration:
  Environment: Ant-v5
  Observation dim: 105
  Action dim: 8
  Continuous actions: True
  Training iterations: 20
  Episodes per iteration: 100
  Total episodes: 2000

Starting local training...
Iteration   0 | Mean Reward:   -161.2 | Loss: -7.8887
Iteration  10 | Mean Reward:   -120.5 | Loss: -2.9774
Iteration  19 | Mean Reward:    -91.6 | Loss: 0.6673

============================================================
Training Complete!
  Total time: 106.45s
  Episodes: 2000 (18.8 episodes/sec)
  Final Mean Reward: -91.6
============================================================
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Learner (Local)                      │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐  │
│  │   Policy    │───▶│  Broadcast  │───▶│   Collect   │  │
│  │   Update    │    │   Weights   │    │   Futures   │  │
│  └─────────────┘    └─────────────┘    └─────────────┘  │
│         ▲                                     │         │
│         │              ┌──────────────────────┘         │
│         │              ▼                                │
│  ┌─────────────┐    ┌─────────────┐                     │
│  │  Compute    │◀───│  Aggregate  │                     │
│  │  Gradients  │    │  Episodes   │                     │
│  └─────────────┘    └─────────────┘                     │
└─────────────────────────────────────────────────────────┘
                         │
                         │ Flame Runner API
                         ▼
┌────────────────────────────────────────────────────────┐
│                  Flame Cluster                         │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐           │
│  │ Executor 1│  │ Executor 2│  │ Executor N│   ...     │
│  │   ┌───┐   │  │   ┌───┐   │  │   ┌───┐   │           │
│  │   │Env│   │  │   │Env│   │  │   │Env│   │           │
│  │   └───┘   │  │   └───┘   │  │   └───┘   │           │
│  │   Episode │  │   Episode │  │   Episode │           │
│  │ Collection│  │ Collection│  │ Collection│           │
│  └───────────┘  └───────────┘  └───────────┘           │
└────────────────────────────────────────────────────────┘
```

## Performance

### When Distribution Helps

Distribution overhead is ~100ms per task. Speedup depends on episode duration:

| Environment | Episode Time | Distributed Benefit |
|-------------|--------------|---------------------|
| CartPole | ~1ms | ❌ Overhead dominates |
| Hopper | ~15ms | ⚠️ Marginal with high parallelism |
| HalfCheetah | ~20ms | ⚠️ Marginal with high parallelism |
| Ant | ~50ms | ✅ Benefits with 50+ episodes/iter |
| Complex sims | >100ms | ✅✅ Near-linear speedup |
| Real-world/expensive | >1s | ✅✅✅ Essential |

### Maximizing Distributed Performance

1. **Increase `--episodes-per-iter`**: More parallel episodes amortizes the per-iteration overhead (weight upload, session management)
2. **Use heavier environments**: MuJoCo environments benefit more than CartPole
3. **Scale executors**: More executors = more parallel episode collection

### Scaling Behavior

With N executors collecting episodes in parallel:

| Executors | Episodes/Iteration | Theoretical Speedup | Actual Speedup* |
|-----------|-------------------|---------------------|-----------------|
| 1 | 100 | 1x | 1x |
| 5 | 100 | 5x | ~4x |
| 10 | 100 | 10x | ~7-8x |
| 20 | 100 | 20x | ~12-15x |

*Actual speedup limited by: network latency, executor startup, gradient aggregation time.

### Best Practices

1. **Use `--episodes-per-iter 100`** (default) or higher for expensive environments
2. **Use local mode** (`--local`) for fast environments or development/debugging
3. **Profile your environment** to determine if distribution is beneficial
4. **Start with MuJoCo** (ant, halfcheetah) to see distributed benefits

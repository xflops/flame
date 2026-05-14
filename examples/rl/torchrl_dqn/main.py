"""
TorchRL-style DQN on discrete Gymnasium envs using Flame Runner and patch_object.

Use --local for a no-cluster smoke run. Distributed mode keeps the learner local,
runs rollout workers with Runner.service(), and stores transitions in Flame cache.
"""

from __future__ import annotations

import argparse
import statistics
import time

from model import ENV_ALIASES
from replay_buffer import (
    REPLAY_CHOICES,
    REPLAY_SHARDED,
    REPLAY_SIMPLE,
    create_flame_replay_buffers,
    create_local_replay_buffer,
    replay_buffer_shard_states,
    split_batch,
)


DEFAULT_ENV = "CartPole-v1"
DEFAULT_ITERATIONS = 20
DEFAULT_COLLECTIONS = 4
DEFAULT_FRAMES_PER_COLLECTION = 100
DEFAULT_BATCH_SIZE = 64
DEFAULT_BUFFER_SIZE = 10000
DEFAULT_HIDDEN_DIM = 128
DEFAULT_SAMPLE_PARALLELISM = 1
DEFAULT_REPLAY_SHARDS = 4


def _epsilon(
    iteration: int,
    total_iterations: int,
    start: float,
    end: float,
) -> float:
    if total_iterations <= 1:
        return end
    fraction = min(1.0, iteration / (total_iterations - 1))
    return start + fraction * (end - start)


def _sample_shard_plan(
    batch_size: int,
    sample_parallelism: int,
    shard_count: int,
    cursor: int,
) -> list[tuple[int, int]]:
    request_count = min(batch_size, sample_parallelism, shard_count)
    request_sizes = split_batch(batch_size, request_count)
    return [
        ((cursor + index) % shard_count, request_size)
        for index, request_size in enumerate(request_sizes)
    ]


def _select_non_empty_shard(
    shard_states: list[dict[str, int]],
    shard_index: int,
) -> int | None:
    for offset in range(len(shard_states)):
        candidate = (shard_index + offset) % len(shard_states)
        if shard_states[candidate]["size"] > 0:
            return candidate
    return None


def _sample_request_size(request_size: int, available_size: int) -> int:
    return min(request_size, available_size)


def _effective_sample_work(replay: str, sample_work: int | None) -> int:
    if sample_work is not None:
        if sample_work < 0:
            raise ValueError("sample_work must be non-negative")
        return sample_work
    return 0


def _batch_len(batch) -> int:
    if batch is None:
        return 0
    return len(batch)


def _concat_sample_batches(batches: list):
    import torch

    non_empty = [batch for batch in batches if _batch_len(batch) > 0]
    if not non_empty:
        return None
    if len(non_empty) == 1:
        return non_empty[0]
    return torch.cat(non_empty, dim=0)


def _timing_summary(metrics: list[dict]) -> dict[str, dict[str, float]]:
    fields = [
        "collect_secs",
        "state_secs",
        "sample_secs",
        "optimize_secs",
        "iteration_secs",
    ]
    summary = {}
    for field in fields:
        values = [metric[field] for metric in metrics if field in metric]
        if values:
            summary[field] = {
                "total": sum(values),
                "mean": statistics.mean(values),
            }
    return summary


def _print_timing_summary(timings: dict[str, dict[str, float]]) -> None:
    if not timings:
        return

    print("\nTiming Summary:")
    for label, key in [
        ("Collect", "collect_secs"),
        ("State", "state_secs"),
        ("Sample", "sample_secs"),
        ("Optimize", "optimize_secs"),
        ("Iteration", "iteration_secs"),
    ]:
        if key in timings:
            timing = timings[key]
            print(
                f"  {label}: {timing['total']:.3f}s total, {timing['mean']:.3f}s/iter"
            )


def _optimize(policy, loss_fn, target_updater, optimizer, transitions) -> float:
    import torch

    from model import transitions_to_tensordict

    batch = transitions_to_tensordict(transitions)
    loss_td = loss_fn(batch)
    loss = loss_td["loss"]

    optimizer.zero_grad()
    loss.backward()
    torch.nn.utils.clip_grad_norm_(policy.parameters(), max_norm=10.0)
    optimizer.step()
    target_updater.step()

    return float(loss.detach().item())


def _mean_completed_reward(results: list[dict]) -> float:
    rewards = [
        result["avg_reward"] for result in results if result.get("episode_count", 0) > 0
    ]
    return statistics.mean(rewards) if rewards else 0.0


def _write_metrics(
    path: str | None,
    *,
    mode: str,
    env_name: str,
    obs_dim: int,
    action_dim: int,
    configuration: dict,
    summary: dict,
    iterations: list[dict],
) -> None:
    if path is None:
        return

    import json

    with open(path, "w") as f:
        json.dump(
            {
                "mode": mode,
                "environment": {
                    "name": env_name,
                    "obs_dim": obs_dim,
                    "action_dim": action_dim,
                },
                "configuration": configuration,
                "summary": summary,
                "iterations": iterations,
            },
            f,
            indent=2,
        )


def train_distributed(
    env_name: str = DEFAULT_ENV,
    iterations: int = DEFAULT_ITERATIONS,
    collections: int = DEFAULT_COLLECTIONS,
    frames_per_collection: int = DEFAULT_FRAMES_PER_COLLECTION,
    batch_size: int = DEFAULT_BATCH_SIZE,
    buffer_size: int = DEFAULT_BUFFER_SIZE,
    hidden_dim: int = DEFAULT_HIDDEN_DIM,
    optim_steps: int = 1,
    warmup_frames: int = 100,
    lr: float = 1e-3,
    gamma: float = 0.99,
    target_update_tau: float = 0.05,
    epsilon_start: float = 0.2,
    epsilon_end: float = 0.02,
    replay: str = REPLAY_SIMPLE,
    replay_shards: int = DEFAULT_REPLAY_SHARDS,
    sample_work: int = 0,
    sample_parallelism: int = DEFAULT_SAMPLE_PARALLELISM,
    metrics_json: str | None = None,
    seed: int | None = 0,
):
    import torch
    from flamepy.runner import Runner

    from collector import FlameTorchRLCollector
    from model import build_loss_and_updater, build_policy, inspect_discrete_env

    env_spec = inspect_discrete_env(env_name)
    total_frames = iterations * collections * frames_per_collection

    print("=" * 72)
    print("TorchRL DQN with Flame Runner and Replay Buffer")
    print("=" * 72)
    print("\nConfiguration:")
    print(f"  Environment: {env_spec.name}")
    print(f"  Observation dim: {env_spec.obs_dim}")
    print(f"  Action dim: {env_spec.action_dim}")
    print(f"  Iterations: {iterations}")
    print(f"  Collections per iteration: {collections}")
    print(f"  Frames per collection: {frames_per_collection}")
    print(f"  Total frames: {total_frames}")
    print(f"  Batch size: {batch_size}")
    print(f"  Replay buffer size: {buffer_size}")
    print(f"  Replay mode: {replay}")
    print(f"  Replay shards: {replay_shards if replay == REPLAY_SHARDED else 1}")
    print(f"  Sample work: {sample_work}")
    print(f"  Sample parallelism: {sample_parallelism}")
    print(f"  Optim steps per iteration: {optim_steps}")
    print(f"  Target update tau: {target_update_tau}")
    print("\nStarting distributed training...")

    if seed is not None:
        torch.manual_seed(seed)

    policy = build_policy(env_spec.obs_dim, env_spec.action_dim, hidden_dim)
    loss_fn, target_updater = build_loss_and_updater(
        policy,
        gamma=gamma,
        target_update_tau=target_update_tau,
    )
    optimizer = torch.optim.Adam(policy.parameters(), lr=lr)
    start_time = time.time()
    total_added = 0
    last_loss = 0.0
    metrics = []
    sample_cursor = 0

    runner_name = f"torchrl-dqn-{env_spec.name.lower().replace('/', '-')}"
    with Runner(runner_name) as rr:
        replay_buffers = create_flame_replay_buffers(
            rr,
            replay=replay,
            buffer_size=buffer_size,
            replay_shards=replay_shards,
            sample_work=sample_work,
        )
        replay_services = [
            rr.service(
                replay_buffer,
                warmup=sample_parallelism if replay == REPLAY_SIMPLE else 1,
            )
            for replay_buffer in replay_buffers
        ]
        collector = rr.service(
            FlameTorchRLCollector(
                env_spec.name,
                env_spec.obs_dim,
                env_spec.action_dim,
                hidden_dim,
                seed=seed,
            ),
            autoscale=True,
        )

        for iteration in range(iterations):
            iteration_start = time.time()
            epsilon = _epsilon(iteration, iterations, epsilon_start, epsilon_end)
            weights_ref = rr.put_object(policy.state_dict())
            collect_start = time.time()
            collect_futures = [
                collector.collect(
                    replay_buffers[collection_index % len(replay_buffers)],
                    weights_ref,
                    frames_per_collection,
                    epsilon,
                )
                for collection_index in range(collections)
            ]
            collect_results = rr.get(collect_futures)
            collect_elapsed = time.time() - collect_start

            state_start = time.time()
            shard_states = replay_buffer_shard_states(replay_buffers)
            stats = {
                "size": sum(state["size"] for state in shard_states),
                "total_added": sum(state["total_added"] for state in shard_states),
                "shards": len(shard_states),
            }
            state_elapsed = time.time() - state_start
            total_added = stats["total_added"]
            loss_values = []
            sample_elapsed = 0.0
            optimize_elapsed = 0.0

            if stats["size"] >= max(batch_size, warmup_frames):
                for _ in range(optim_steps):
                    sample_start = time.time()
                    sample_futures = []
                    if replay == REPLAY_SIMPLE:
                        request_sizes = split_batch(batch_size, sample_parallelism)
                        available_size = shard_states[0]["size"]
                        sample_futures = [
                            replay_services[0].sample(
                                _sample_request_size(request_size, available_size)
                            )
                            for request_size in request_sizes
                            if available_size > 0
                        ]
                    else:
                        sample_plan = _sample_shard_plan(
                            batch_size,
                            sample_parallelism,
                            len(replay_buffers),
                            sample_cursor,
                        )
                        sample_cursor += len(sample_plan)
                        for shard_index, request_size in sample_plan:
                            selected = _select_non_empty_shard(
                                shard_states,
                                shard_index,
                            )
                            if selected is None:
                                continue
                            sample_size = _sample_request_size(
                                request_size,
                                shard_states[selected]["size"],
                            )
                            if sample_size > 0:
                                sample_futures.append(
                                    replay_services[selected].sample(sample_size)
                                )
                    batches = rr.get(sample_futures)
                    sample_elapsed += time.time() - sample_start
                    transitions = _concat_sample_batches(batches)
                    if _batch_len(transitions) > 0:
                        optimize_start = time.time()
                        loss_values.append(
                            _optimize(
                                policy,
                                loss_fn,
                                target_updater,
                                optimizer,
                                transitions,
                            )
                        )
                        optimize_elapsed += time.time() - optimize_start

            if loss_values:
                last_loss = statistics.mean(loss_values)
            mean_reward = _mean_completed_reward(collect_results)
            iteration_elapsed = time.time() - iteration_start
            metrics.append(
                {
                    "iteration": iteration,
                    "epsilon": epsilon,
                    "collect_secs": collect_elapsed,
                    "state_secs": state_elapsed,
                    "sample_secs": sample_elapsed,
                    "optimize_secs": optimize_elapsed,
                    "iteration_secs": iteration_elapsed,
                    "buffer_size": stats["size"],
                    "total_added": total_added,
                    "avg_reward": mean_reward,
                    "loss": last_loss,
                }
            )

            print(
                f"Iteration {iteration:3d} | "
                f"epsilon={epsilon:.3f} | "
                f"buffer={stats['size']:5d} | "
                f"total={total_added:6d} | "
                f"reward={mean_reward:6.1f} | "
                f"loss={last_loss:.4f}"
            )

    elapsed = time.time() - start_time
    print("\n" + "=" * 72)
    print("Training Complete!")
    print(f"  Total time: {elapsed:.2f}s")
    print(f"  Total frames: {total_added}")
    print(f"  Throughput: {total_added / elapsed:.1f} frames/sec")
    print(f"  Final loss: {last_loss:.4f}")
    timing_summary = _timing_summary(metrics)
    _print_timing_summary(timing_summary)
    print("=" * 72)

    _write_metrics(
        metrics_json,
        mode="flame",
        env_name=env_spec.name,
        obs_dim=env_spec.obs_dim,
        action_dim=env_spec.action_dim,
        configuration={
            "iterations": iterations,
            "collections": collections,
            "frames_per_collection": frames_per_collection,
            "batch_size": batch_size,
            "buffer_size": buffer_size,
            "hidden_dim": hidden_dim,
            "optim_steps": optim_steps,
            "warmup_frames": warmup_frames,
            "lr": lr,
            "gamma": gamma,
            "target_update_tau": target_update_tau,
            "epsilon_start": epsilon_start,
            "epsilon_end": epsilon_end,
            "sample_parallelism": sample_parallelism,
            "replay": replay,
            "replay_shards": replay_shards,
            "sample_work": sample_work,
            "seed": seed,
        },
        summary={
            "total_time_secs": elapsed,
            "total_frames": total_added,
            "throughput": total_added / elapsed,
            "final_loss": last_loss,
            "timings": timing_summary,
        },
        iterations=metrics,
    )

    return policy


def train_local(
    env_name: str = DEFAULT_ENV,
    iterations: int = DEFAULT_ITERATIONS,
    collections: int = DEFAULT_COLLECTIONS,
    frames_per_collection: int = DEFAULT_FRAMES_PER_COLLECTION,
    batch_size: int = DEFAULT_BATCH_SIZE,
    buffer_size: int = DEFAULT_BUFFER_SIZE,
    hidden_dim: int = DEFAULT_HIDDEN_DIM,
    optim_steps: int = 1,
    warmup_frames: int = 100,
    lr: float = 1e-3,
    gamma: float = 0.99,
    target_update_tau: float = 0.05,
    epsilon_start: float = 0.2,
    epsilon_end: float = 0.02,
    replay: str = REPLAY_SIMPLE,
    replay_shards: int = DEFAULT_REPLAY_SHARDS,
    sample_work: int = 0,
    metrics_json: str | None = None,
    seed: int | None = 0,
):
    import torch

    from collector import FlameTorchRLCollector
    from model import build_loss_and_updater, build_policy, inspect_discrete_env

    env_spec = inspect_discrete_env(env_name)
    total_frames = iterations * collections * frames_per_collection

    print("=" * 72)
    print("Local TorchRL DQN")
    print("=" * 72)
    print("\nConfiguration:")
    print(f"  Environment: {env_spec.name}")
    print(f"  Observation dim: {env_spec.obs_dim}")
    print(f"  Action dim: {env_spec.action_dim}")
    print(f"  Iterations: {iterations}")
    print(f"  Collections per iteration: {collections}")
    print(f"  Frames per collection: {frames_per_collection}")
    print(f"  Total frames: {total_frames}")
    print(f"  Batch size: {batch_size}")
    print(f"  Replay buffer size: {buffer_size}")
    print(f"  Replay mode: {replay}")
    print(f"  Replay shards: {replay_shards if replay == REPLAY_SHARDED else 1}")
    print(f"  Sample work: {sample_work}")
    print(f"  Optim steps per iteration: {optim_steps}")
    print(f"  Target update tau: {target_update_tau}")
    print("\nStarting local training...")

    if seed is not None:
        torch.manual_seed(seed)

    policy = build_policy(env_spec.obs_dim, env_spec.action_dim, hidden_dim)
    loss_fn, target_updater = build_loss_and_updater(
        policy,
        gamma=gamma,
        target_update_tau=target_update_tau,
    )
    optimizer = torch.optim.Adam(policy.parameters(), lr=lr)
    buffer = create_local_replay_buffer(
        replay=replay,
        buffer_size=buffer_size,
        replay_shards=replay_shards,
        sample_work=sample_work,
    )
    collector = FlameTorchRLCollector(
        env_spec.name,
        env_spec.obs_dim,
        env_spec.action_dim,
        hidden_dim,
        seed=seed,
    )
    start_time = time.time()
    last_loss = 0.0
    metrics = []

    for iteration in range(iterations):
        iteration_start = time.time()
        epsilon = _epsilon(iteration, iterations, epsilon_start, epsilon_end)
        weights = policy.state_dict()
        collect_start = time.time()
        collect_results = [
            collector.collect(buffer, weights, frames_per_collection, epsilon)
            for _ in range(collections)
        ]
        collect_elapsed = time.time() - collect_start
        stats = buffer.state()
        loss_values = []
        sample_elapsed = 0.0
        optimize_elapsed = 0.0

        if stats["size"] >= max(batch_size, warmup_frames):
            for _ in range(optim_steps):
                sample_start = time.time()
                transitions = buffer.sample(batch_size)
                sample_elapsed += time.time() - sample_start
                if _batch_len(transitions) > 0:
                    optimize_start = time.time()
                    loss_values.append(
                        _optimize(
                            policy,
                            loss_fn,
                            target_updater,
                            optimizer,
                            transitions,
                        )
                    )
                    optimize_elapsed += time.time() - optimize_start

        if loss_values:
            last_loss = statistics.mean(loss_values)
        mean_reward = _mean_completed_reward(collect_results)
        iteration_elapsed = time.time() - iteration_start
        metrics.append(
            {
                "iteration": iteration,
                "epsilon": epsilon,
                "collect_secs": collect_elapsed,
                "sample_secs": sample_elapsed,
                "optimize_secs": optimize_elapsed,
                "iteration_secs": iteration_elapsed,
                "buffer_size": stats["size"],
                "total_added": stats["total_added"],
                "avg_reward": mean_reward,
                "loss": last_loss,
            }
        )

        print(
            f"Iteration {iteration:3d} | "
            f"epsilon={epsilon:.3f} | "
            f"buffer={stats['size']:5d} | "
            f"total={stats['total_added']:6d} | "
            f"reward={mean_reward:6.1f} | "
            f"loss={last_loss:.4f}"
        )

    elapsed = time.time() - start_time
    total_added = buffer.state()["total_added"]
    print("\n" + "=" * 72)
    print("Training Complete!")
    print(f"  Total time: {elapsed:.2f}s")
    print(f"  Total frames: {total_added}")
    print(f"  Throughput: {total_added / elapsed:.1f} frames/sec")
    print(f"  Final loss: {last_loss:.4f}")
    timing_summary = _timing_summary(metrics)
    _print_timing_summary(timing_summary)
    print("=" * 72)

    _write_metrics(
        metrics_json,
        mode="local",
        env_name=env_spec.name,
        obs_dim=env_spec.obs_dim,
        action_dim=env_spec.action_dim,
        configuration={
            "iterations": iterations,
            "collections": collections,
            "frames_per_collection": frames_per_collection,
            "batch_size": batch_size,
            "buffer_size": buffer_size,
            "hidden_dim": hidden_dim,
            "optim_steps": optim_steps,
            "warmup_frames": warmup_frames,
            "lr": lr,
            "gamma": gamma,
            "target_update_tau": target_update_tau,
            "epsilon_start": epsilon_start,
            "epsilon_end": epsilon_end,
            "replay": replay,
            "replay_shards": replay_shards,
            "sample_work": sample_work,
            "seed": seed,
        },
        summary={
            "total_time_secs": elapsed,
            "total_frames": total_added,
            "throughput": total_added / elapsed,
            "final_loss": last_loss,
            "timings": timing_summary,
        },
        iterations=metrics,
    )

    return policy


def parse_args() -> argparse.Namespace:
    env_help = (
        f"Gymnasium environment or preset alias ({', '.join(sorted(ENV_ALIASES))})"
    )
    parser = argparse.ArgumentParser(
        description="TorchRL DQN on discrete Gymnasium envs with Flame Runner"
    )
    parser.add_argument(
        "--env",
        default=DEFAULT_ENV,
        help=env_help,
    )
    parser.add_argument(
        "--local",
        action="store_true",
        help="Run local mode without a Flame cluster",
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=DEFAULT_ITERATIONS,
        help="Training iterations",
    )
    parser.add_argument(
        "--collections",
        type=int,
        default=DEFAULT_COLLECTIONS,
        help="Collection tasks per iteration",
    )
    parser.add_argument(
        "--frames-per-collection",
        type=int,
        default=DEFAULT_FRAMES_PER_COLLECTION,
        help="Frames each collector gathers per call",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=DEFAULT_BATCH_SIZE,
        help="Replay sample batch size",
    )
    parser.add_argument(
        "--buffer-size",
        type=int,
        default=DEFAULT_BUFFER_SIZE,
        help="Maximum replay-buffer transitions kept materialized",
    )
    parser.add_argument(
        "--hidden-dim",
        type=int,
        default=DEFAULT_HIDDEN_DIM,
        help="Q-network hidden dimension",
    )
    parser.add_argument(
        "--optim-steps",
        type=int,
        default=1,
        help="Optimization steps per training iteration",
    )
    parser.add_argument(
        "--warmup-frames",
        type=int,
        default=100,
        help="Replay frames required before optimization starts",
    )
    parser.add_argument("--lr", type=float, default=1e-3, help="Adam learning rate")
    parser.add_argument("--gamma", type=float, default=0.99, help="Discount factor")
    parser.add_argument(
        "--target-update-tau",
        type=float,
        default=0.05,
        help="Soft target-network update coefficient",
    )
    parser.add_argument(
        "--epsilon-start",
        type=float,
        default=0.2,
        help="Initial random-action probability",
    )
    parser.add_argument(
        "--epsilon-end",
        type=float,
        default=0.02,
        help="Final random-action probability",
    )
    parser.add_argument(
        "--replay",
        choices=REPLAY_CHOICES,
        default=REPLAY_SIMPLE,
        help="Replay-buffer implementation",
    )
    parser.add_argument(
        "--replay-shards",
        type=int,
        default=DEFAULT_REPLAY_SHARDS,
        help="Shard count for --replay sharded",
    )
    parser.add_argument(
        "--sample-work",
        type=int,
        default=None,
        help=(
            "CPU work units applied per sampled transition. "
            "Default is 0 for all replay modes."
        ),
    )
    parser.add_argument(
        "--sample-parallelism",
        type=int,
        default=DEFAULT_SAMPLE_PARALLELISM,
        help="Replay-buffer service instances and sample requests",
    )
    parser.add_argument(
        "--metrics-json",
        type=str,
        default=None,
        help="Write run metrics to a JSON file",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=0,
        help="Torch and environment seed; use -1 to disable explicit seeding",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    seed = None if args.seed < 0 else args.seed
    sample_work = _effective_sample_work(args.replay, args.sample_work)
    kwargs = {
        "env_name": args.env,
        "iterations": args.iterations,
        "collections": args.collections,
        "frames_per_collection": args.frames_per_collection,
        "batch_size": args.batch_size,
        "buffer_size": args.buffer_size,
        "hidden_dim": args.hidden_dim,
        "optim_steps": args.optim_steps,
        "warmup_frames": args.warmup_frames,
        "lr": args.lr,
        "gamma": args.gamma,
        "target_update_tau": args.target_update_tau,
        "epsilon_start": args.epsilon_start,
        "epsilon_end": args.epsilon_end,
        "replay": args.replay,
        "replay_shards": args.replay_shards,
        "sample_work": sample_work,
        "metrics_json": args.metrics_json,
        "seed": seed,
    }

    if args.local:
        train_local(**kwargs)
    else:
        train_distributed(
            **kwargs,
            sample_parallelism=args.sample_parallelism,
        )


if __name__ == "__main__":
    main()

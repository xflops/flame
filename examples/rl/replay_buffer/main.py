"""
Distributed Replay Buffer using patch_object for efficient data movement.

ReplayBuffer wraps an ObjectRef - collectors patch transitions directly to it,
avoiding data transfer through the service. The service handles merging and sampling.

Use --local flag for local mode without a Flame cluster.
"""

from collector import Collector
from replay_buffer import ReplayBuffer


def run_distributed(
    env_name: str = "CartPole-v1",
    num_iterations: int = 50,
    num_collections: int = 20,
    steps_per_collection: int = 500,
    batch_size: int = 64,
    merge_every: int | None = 5,
    metrics_json: str | None = None,
    force_full_get: bool = False,
):
    import json
    import time

    from flamepy.runner import Runner

    print("=" * 60)
    print("Distributed Replay Buffer (patch_object)")
    print("=" * 60)
    print("\nConfiguration:")
    print(f"  Environment: {env_name}")
    print(f"  Collections per iteration: {num_collections}")
    print(f"  Steps per collection: {steps_per_collection}")
    print(f"  Iterations: {num_iterations}")
    print(f"  Batch size: {batch_size}")
    print(f"  Merge every: {merge_every if merge_every else 'disabled'}")
    print(f"  Force full get: {force_full_get}")
    print("\nStarting distributed collection...")

    start_time = time.time()
    metrics = []
    total_added = 0

    with Runner(f"replay-buffer-{env_name.lower()}") as rr:
        buffer = ReplayBuffer(rr, force_full_get=force_full_get)
        buffer_svc = rr.service(buffer, autoscale=False, warmup=1)
        collector = rr.service(Collector(env_name), autoscale=True)

        for iteration in range(num_iterations):
            iteration_start = time.time()
            collect_futures = [
                collector.collect(buffer, steps_per_collection)
                for _ in range(num_collections)
            ]
            collect_results = rr.get(collect_futures)
            collect_elapsed = time.time() - iteration_start

            merge_elapsed = 0.0
            if merge_every and iteration % merge_every == merge_every - 1:
                merge_start = time.time()
                buffer_svc.merge().wait()
                merge_elapsed = time.time() - merge_start

            state_start = time.time()
            stats = buffer_svc.state().get()
            state_elapsed = time.time() - state_start
            total_size = stats["size"]
            total_added = stats["total_added"]
            total_episodes = sum(r["episode_count"] for r in collect_results)
            avg_reward = sum(
                r["avg_reward"] * r["episode_count"] for r in collect_results
            ) / max(1, total_episodes)

            print(
                f"Iteration {iteration:2d} | "
                f"Buffer: {total_size:6d} | "
                f"Total added: {total_added:6d} | "
                f"Avg Reward: {avg_reward:7.1f}"
            )

            sample_elapsed = 0.0
            sampled = 0
            if total_size >= batch_size:
                sample_start = time.time()
                batch = buffer_svc.sample(batch_size).get()
                sample_elapsed = time.time() - sample_start
                sampled = len(batch)
                print(f"             | Sampled batch of {len(batch)} transitions")

            metrics.append(
                {
                    "iteration": iteration,
                    "collect_secs": collect_elapsed,
                    "merge_secs": merge_elapsed,
                    "state_secs": state_elapsed,
                    "sample_secs": sample_elapsed,
                    "buffer_size": total_size,
                    "total_added": total_added,
                    "total_episodes": total_episodes,
                    "avg_reward": avg_reward,
                    "sampled": sampled,
                }
            )

    elapsed = time.time() - start_time
    print("\n" + "=" * 60)
    print("Collection Complete!")
    print(f"  Total time: {elapsed:.2f}s")
    print(f"  Total transitions: {total_added}")
    print(f"  Throughput: {total_added / elapsed:.1f} transitions/sec")
    print("=" * 60)

    if metrics_json:
        with open(metrics_json, "w") as f:
            json.dump(
                {
                    "configuration": {
                        "env": env_name,
                        "iterations": num_iterations,
                        "collections": num_collections,
                        "steps_per_collection": steps_per_collection,
                        "batch_size": batch_size,
                        "merge_every": merge_every,
                        "force_full_get": force_full_get,
                    },
                    "summary": {
                        "total_time_secs": elapsed,
                        "total_transitions": total_added,
                        "throughput": total_added / elapsed,
                    },
                    "iterations": metrics,
                },
                f,
                indent=2,
            )


def run_local(
    env_name: str = "CartPole-v1",
    num_iterations: int = 50,
    steps_per_iteration: int = 2000,
    batch_size: int = 64,
):
    import random
    import time

    import gymnasium as gym
    from collections import deque

    print("=" * 60)
    print("Local Replay Buffer")
    print("=" * 60)
    print("\nConfiguration:")
    print(f"  Environment: {env_name}")
    print(f"  Steps per iteration: {steps_per_iteration}")
    print(f"  Iterations: {num_iterations}")
    print(f"  Batch size: {batch_size}")
    print("\nStarting local collection...")

    start_time = time.time()
    env = gym.make(env_name)
    buffer: deque = deque(maxlen=100000)
    state, _ = env.reset()
    episode_reward = 0.0
    episode_count = 0
    total_reward = 0.0
    total_added = 0

    for iteration in range(num_iterations):
        for _ in range(steps_per_iteration):
            action = env.action_space.sample()
            next_state, reward, terminated, truncated, _ = env.step(action)
            done = terminated or truncated
            episode_reward += reward

            buffer.append(
                {
                    "state": state.tolist(),
                    "action": int(action),
                    "reward": float(reward),
                    "next_state": next_state.tolist(),
                    "done": done,
                }
            )
            total_added += 1

            if done:
                state, _ = env.reset()
                total_reward += episode_reward
                episode_count += 1
                episode_reward = 0.0
            else:
                state = next_state

        avg_reward = total_reward / max(1, episode_count)
        print(
            f"Iteration {iteration:2d} | "
            f"Buffer: {len(buffer):6d} | "
            f"Total added: {total_added:6d} | "
            f"Avg Reward: {avg_reward:7.1f}"
        )

        if len(buffer) >= batch_size:
            batch = random.sample(list(buffer), batch_size)
            print(f"             | Sampled batch of {len(batch)} transitions")

    env.close()

    elapsed = time.time() - start_time
    print("\n" + "=" * 60)
    print("Collection Complete!")
    print(f"  Total time: {elapsed:.2f}s")
    print(f"  Total transitions: {total_added}")
    print(f"  Throughput: {total_added / elapsed:.1f} transitions/sec")
    print("=" * 60)


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Distributed Replay Buffer Service")
    parser.add_argument(
        "--env",
        type=str,
        default="CartPole-v1",
        help="Gymnasium environment (default: CartPole-v1)",
    )
    parser.add_argument(
        "--local", action="store_true", help="Run local mode (no Flame cluster)"
    )
    parser.add_argument(
        "--iterations", type=int, default=50, help="Number of collection iterations"
    )
    parser.add_argument(
        "--collections",
        type=int,
        default=20,
        help="Number of collections per iteration",
    )
    parser.add_argument(
        "--steps-per-collection",
        type=int,
        default=500,
        help="Steps per collection task",
    )
    parser.add_argument(
        "--batch-size", type=int, default=64, help="Batch size for sampling"
    )
    parser.add_argument(
        "--metrics-json",
        type=str,
        default=None,
        help="Write distributed-mode metrics to a JSON file",
    )
    parser.add_argument(
        "--merge-every",
        type=int,
        default=5,
        help="Merge replay-buffer patches every N iterations",
    )
    parser.add_argument(
        "--no-merge",
        action="store_true",
        help="Disable replay-buffer patch merging",
    )
    parser.add_argument(
        "--force-full-get",
        action="store_true",
        help="Force replay-buffer reads to request full objects with version 0",
    )

    args = parser.parse_args()
    merge_every = None if args.no_merge else args.merge_every

    if args.local:
        run_local(
            env_name=args.env,
            num_iterations=args.iterations,
            steps_per_iteration=args.collections * args.steps_per_collection,
            batch_size=args.batch_size,
        )
    else:
        run_distributed(
            env_name=args.env,
            num_iterations=args.iterations,
            num_collections=args.collections,
            steps_per_collection=args.steps_per_collection,
            batch_size=args.batch_size,
            merge_every=merge_every,
            metrics_json=args.metrics_json,
            force_full_get=args.force_full_get,
        )


if __name__ == "__main__":
    main()

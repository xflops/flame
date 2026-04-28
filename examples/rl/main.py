"""
Distributed REINFORCE on CartPole-v1 or MuJoCo using Flame Runner.
Use --local flag for local training without a Flame cluster.
Use --env to select environment (cartpole, halfcheetah, hopper, walker2d, ant).
"""

import time

import gymnasium as gym
import numpy as np
import torch
import torch.optim as optim

from model import ENV_CONFIGS, create_policy


def collect_episode(weights, env_name: str) -> dict:
    """Runs on distributed executors to collect one episode.

    Args:
        weights: Model state_dict (auto-resolved from ObjectRef by Runner)
        env_name: Name of the environment to use
    """
    import gymnasium as gym
    import torch

    from model import ENV_CONFIGS, create_policy

    env_config = ENV_CONFIGS[env_name]
    model = create_policy(env_config)
    model.load_state_dict(weights)
    model.eval()

    env = gym.make(env_config.name)
    states, actions, rewards = [], [], []
    state, _ = env.reset()
    done = False

    while not done:
        states.append(state)
        state_tensor = torch.FloatTensor(state).unsqueeze(0)
        with torch.no_grad():
            action, _ = model.get_action(state_tensor)
        actions.append(action)

        state, reward, terminated, truncated, _ = env.step(action)
        done = terminated or truncated
        rewards.append(reward)

    env.close()
    return {
        "states": states,
        "actions": actions,
        "rewards": rewards,
        "total_reward": sum(rewards),
    }


def compute_discounted_rewards(rewards: list, gamma: float = 0.99) -> torch.Tensor:
    discounted = [0.0] * len(rewards)
    R = 0
    for i in range(len(rewards) - 1, -1, -1):
        R = rewards[i] + gamma * R
        discounted[i] = R
    discounted = torch.tensor(discounted, dtype=torch.float32)
    if len(discounted) > 1:
        discounted = (discounted - discounted.mean()) / (discounted.std() + 1e-8)
    return discounted


def train_distributed(
    env_name: str, num_iterations: int = 100, episodes_per_iteration: int = 10
):
    from functools import partial

    from flamepy import put_object
    from flamepy.runner import Runner

    env_config = ENV_CONFIGS[env_name]
    total_episodes = num_iterations * episodes_per_iteration

    print("=" * 60)
    print(f"Distributed REINFORCE on {env_config.name} using Flame Runner")
    print("=" * 60)
    print(f"\nConfiguration:")
    print(f"  Environment: {env_config.name}")
    print(f"  Observation dim: {env_config.obs_dim}")
    print(f"  Action dim: {env_config.act_dim}")
    print(f"  Continuous actions: {env_config.continuous}")
    print(f"  Training iterations: {num_iterations}")
    print(f"  Episodes per iteration: {episodes_per_iteration}")
    print(f"  Total episodes: {total_episodes}")
    print(f"\nStarting distributed training...")

    policy = create_policy(env_config)
    lr = 3e-4 if env_config.continuous else 1e-2
    optimizer = optim.Adam(policy.parameters(), lr=lr)
    episode_rewards_history = []
    mean_reward = 0.0
    start_time = time.time()

    collect_fn = partial(collect_episode, env_name=env_name)

    with Runner(f"rl-{env_name}") as rr:
        collector = rr.service(collect_fn)

        for iteration in range(num_iterations):
            weights_ref = put_object(f"rl-weights-{iteration}", policy.state_dict())

            futures = [collector(weights_ref) for _ in range(episodes_per_iteration)]
            episodes = rr.get(futures)

            iteration_rewards = [ep["total_reward"] for ep in episodes]
            mean_reward = np.mean(iteration_rewards)
            episode_rewards_history.extend(iteration_rewards)

            policy.train()
            optimizer.zero_grad()
            total_loss = torch.tensor(0.0)

            for episode in episodes:
                discounted_rewards = compute_discounted_rewards(episode["rewards"])
                states_tensor = torch.FloatTensor(np.array(episode["states"]))
                actions_tensor = torch.tensor(
                    np.array(episode["actions"]),
                    dtype=torch.float32 if env_config.continuous else torch.long,
                )
                log_probs = policy.evaluate(states_tensor, actions_tensor)
                episode_loss = -(log_probs * discounted_rewards).sum()
                total_loss = total_loss + episode_loss

            total_loss = total_loss / len(episodes)
            total_loss.backward()
            optimizer.step()

            if iteration % 10 == 0 or iteration == num_iterations - 1:
                print(
                    f"Iteration {iteration:3d} | "
                    f"Mean Reward: {mean_reward:8.1f} | "
                    f"Loss: {total_loss.item():.4f}"
                )

    elapsed = time.time() - start_time
    print("\n" + "=" * 60)
    print("Training Complete!")
    print(f"  Total time: {elapsed:.2f}s")
    print(f"  Episodes: {total_episodes} ({total_episodes/elapsed:.1f} episodes/sec)")
    print(f"  Final Mean Reward: {mean_reward:.1f}")
    print("=" * 60)

    return policy, episode_rewards_history


def train_local(
    env_name: str, num_iterations: int = 100, episodes_per_iteration: int = 10
):
    env_config = ENV_CONFIGS[env_name]
    total_episodes = num_iterations * episodes_per_iteration

    print("=" * 60)
    print(f"Local REINFORCE on {env_config.name}")
    print("=" * 60)
    print(f"\nConfiguration:")
    print(f"  Environment: {env_config.name}")
    print(f"  Observation dim: {env_config.obs_dim}")
    print(f"  Action dim: {env_config.act_dim}")
    print(f"  Continuous actions: {env_config.continuous}")
    print(f"  Training iterations: {num_iterations}")
    print(f"  Episodes per iteration: {episodes_per_iteration}")
    print(f"  Total episodes: {total_episodes}")
    print(f"\nStarting local training...")

    start_time = time.time()
    env = gym.make(env_config.name)
    policy = create_policy(env_config)
    lr = 3e-4 if env_config.continuous else 1e-2
    optimizer = optim.Adam(policy.parameters(), lr=lr)
    episode_rewards_history = []
    mean_reward = 0.0

    for iteration in range(num_iterations):
        iteration_episodes = []

        for _ in range(episodes_per_iteration):
            state, _ = env.reset()
            log_probs = []
            rewards = []
            states = []
            actions = []
            done = False

            while not done:
                states.append(state)
                state_tensor = torch.FloatTensor(state).unsqueeze(0)
                action, log_prob = policy.get_action(state_tensor)
                actions.append(action)

                state, reward, terminated, truncated, _ = env.step(action)
                done = terminated or truncated

                log_probs.append(log_prob)
                rewards.append(reward)

            iteration_episodes.append({
                "states": states,
                "actions": actions,
                "rewards": rewards,
                "log_probs": log_probs,
                "total_reward": sum(rewards),
            })

        iteration_rewards = [ep["total_reward"] for ep in iteration_episodes]
        mean_reward = np.mean(iteration_rewards)
        episode_rewards_history.extend(iteration_rewards)

        policy.train()
        optimizer.zero_grad()
        total_loss = torch.tensor(0.0)

        for episode in iteration_episodes:
            discounted_rewards = compute_discounted_rewards(episode["rewards"])
            for log_prob, Gt in zip(episode["log_probs"], discounted_rewards):
                total_loss = total_loss - log_prob * Gt

        total_loss = total_loss / len(iteration_episodes)
        total_loss.backward()
        optimizer.step()

        if iteration % 10 == 0 or iteration == num_iterations - 1:
            print(
                f"Iteration {iteration:3d} | "
                f"Mean Reward: {mean_reward:8.1f} | "
                f"Loss: {total_loss.item():.4f}"
            )

    env.close()

    elapsed = time.time() - start_time
    print("\n" + "=" * 60)
    print("Training Complete!")
    print(f"  Total time: {elapsed:.2f}s")
    print(f"  Episodes: {total_episodes} ({total_episodes/elapsed:.1f} episodes/sec)")
    print(f"  Final Mean Reward: {mean_reward:.1f}")
    print("=" * 60)

    return policy, episode_rewards_history


def main():
    import argparse

    parser = argparse.ArgumentParser(
        description="REINFORCE RL on CartPole or MuJoCo (distributed or local)"
    )
    parser.add_argument(
        "--env",
        type=str,
        default="cartpole",
        choices=list(ENV_CONFIGS.keys()),
        help="Environment to use (default: cartpole)",
    )
    parser.add_argument(
        "--local", action="store_true", help="Run local training (no Flame cluster)"
    )
    parser.add_argument(
        "--iterations", type=int, default=100, help="Number of training iterations"
    )
    parser.add_argument(
        "--episodes-per-iter", type=int, default=100, help="Episodes per iteration"
    )
    parser.add_argument(
        "--plot", action="store_true", help="Show training reward plot"
    )

    args = parser.parse_args()

    if args.local:
        policy, rewards = train_local(
            args.env,
            num_iterations=args.iterations,
            episodes_per_iteration=args.episodes_per_iter,
        )
    else:
        policy, rewards = train_distributed(
            args.env,
            num_iterations=args.iterations,
            episodes_per_iteration=args.episodes_per_iter,
        )

    if args.plot:
        try:
            import matplotlib.pyplot as plt

            plt.figure(figsize=(10, 5))
            plt.plot(rewards, alpha=0.6, label="Episode Reward")

            window = 50
            if len(rewards) >= window:
                moving_avg = np.convolve(
                    rewards, np.ones(window) / window, mode="valid"
                )
                plt.plot(
                    range(window - 1, len(rewards)),
                    moving_avg,
                    color="red",
                    linewidth=2,
                    label=f"Moving Avg ({window})",
                )

            plt.title(f"Training Reward Over Episodes ({args.env})")
            plt.xlabel("Episode")
            plt.ylabel("Total Reward")
            plt.legend()
            plt.grid(True, alpha=0.3)
            plt.tight_layout()
            plt.show()
        except ImportError:
            print("\nNote: matplotlib not installed, skipping plot")


if __name__ == "__main__":
    main()

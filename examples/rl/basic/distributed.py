"""Distributed rollout service for the basic reinforcement-learning example."""

import gymnasium as gym
import torch

from model import ENV_CONFIGS, create_policy

import flamepy.app as app

app.init("rl-basic")


@app.service()
def collect_episode(env_name: str, weights) -> dict:
    """Collect one episode on a distributed executor."""
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

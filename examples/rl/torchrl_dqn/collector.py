from __future__ import annotations

import random

import torch
from model import build_policy, flatten_observation
from tensordict import TensorDict


class FlameTorchRLCollector:
    """Stateful rollout worker used through flamepy.runner."""

    def __init__(
        self,
        env_name: str = "CartPole-v1",
        obs_dim: int = 4,
        action_dim: int = 2,
        hidden_dim: int = 128,
        seed: int | None = None,
    ):
        self.env_name = env_name
        self.obs_dim = obs_dim
        self.action_dim = action_dim
        self.hidden_dim = hidden_dim
        self.seed = seed
        self._env = None
        self._state = None
        self._policy = None
        self._reset_count = 0
        self.episode_count = 0
        self.total_reward = 0.0
        self._episode_reward = 0.0

    def _ensure_env(self):
        if self._env is None:
            import gymnasium as gym

            self._env = gym.make(self.env_name)
            self._state, _ = self._env.reset(seed=self.seed)
        return self._env

    def _ensure_policy(self):
        if self._policy is None:
            self._policy = build_policy(self.obs_dim, self.action_dim, self.hidden_dim)
        return self._policy

    def _reset_env(self):
        seed = None
        if self.seed is not None:
            self._reset_count += 1
            seed = self.seed + self._reset_count
        self._state, _ = self._env.reset(seed=seed)

    def _select_action(self, epsilon: float) -> int:
        if random.random() < epsilon:
            return int(self._env.action_space.sample())

        observation = torch.tensor(
            [flatten_observation(self._state)],
            dtype=torch.float32,
        )
        td = TensorDict({"observation": observation}, batch_size=[1])
        with torch.no_grad():
            action_td = self._policy(td)
        return int(action_td["action"].reshape(-1)[0].item())

    def collect(
        self,
        buffer,
        weights: dict,
        num_steps: int,
        epsilon: float,
    ) -> dict:
        env = self._ensure_env()
        policy = self._ensure_policy()
        policy.load_state_dict(weights)
        policy.eval()

        observations = []
        actions = []
        rewards = []
        next_observations = []
        dones = []
        terminated_flags = []
        completed_reward = 0.0
        completed_episodes = 0

        for _ in range(num_steps):
            action = self._select_action(epsilon)
            next_state, reward, terminated, truncated, _ = env.step(action)
            done = terminated or truncated

            observations.append(flatten_observation(self._state))
            actions.append(action)
            rewards.append(float(reward))
            next_observations.append(flatten_observation(next_state))
            dones.append(bool(done))
            terminated_flags.append(bool(terminated))

            self._episode_reward += float(reward)
            if done:
                self.episode_count += 1
                completed_episodes += 1
                completed_reward += self._episode_reward
                self.total_reward += self._episode_reward
                self._episode_reward = 0.0
                self._reset_env()
            else:
                self._state = next_state

        transitions = TensorDict(
            {
                "observation": torch.tensor(observations, dtype=torch.float32),
                "action": torch.tensor(actions, dtype=torch.long),
                "next": TensorDict(
                    {
                        "observation": torch.tensor(
                            next_observations,
                            dtype=torch.float32,
                        ),
                        "reward": torch.tensor(
                            rewards,
                            dtype=torch.float32,
                        ).unsqueeze(-1),
                        "done": torch.tensor(dones, dtype=torch.bool).unsqueeze(-1),
                        "terminated": torch.tensor(
                            terminated_flags,
                            dtype=torch.bool,
                        ).unsqueeze(-1),
                    },
                    batch_size=[num_steps],
                ),
            },
            batch_size=[num_steps],
        )
        buffer.extend(transitions)

        return {
            "num_transitions": num_steps,
            "episode_count": completed_episodes,
            "avg_reward": completed_reward / max(1, completed_episodes),
            "worker_episodes": self.episode_count,
            "worker_avg_reward": self.total_reward / max(1, self.episode_count),
        }

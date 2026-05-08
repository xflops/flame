from dataclasses import dataclass
from typing import Tuple

import numpy as np
import torch
import torch.nn as nn
from torch.distributions import Categorical, Normal


@dataclass
class EnvConfig:
    name: str
    obs_dim: int
    act_dim: int
    continuous: bool
    max_episode_steps: int


ENV_CONFIGS = {
    "cartpole": EnvConfig("CartPole-v1", 4, 2, False, 500),
    "halfcheetah": EnvConfig("HalfCheetah-v5", 17, 6, True, 1000),
    "hopper": EnvConfig("Hopper-v5", 11, 3, True, 1000),
    "walker2d": EnvConfig("Walker2d-v5", 17, 6, True, 1000),
    "ant": EnvConfig("Ant-v5", 105, 8, True, 1000),
}


class DiscretePolicy(nn.Module):
    """Policy network for discrete action spaces (CartPole)."""

    def __init__(self, obs_dim: int, act_dim: int):
        super().__init__()
        self.fc = nn.Sequential(
            nn.Linear(obs_dim, 128),
            nn.ReLU(),
            nn.Linear(128, act_dim),
            nn.Softmax(dim=-1),
        )

    def forward(self, x):
        return self.fc(x)

    def get_action(self, state: torch.Tensor) -> Tuple[int, torch.Tensor]:
        probs = self(state)
        m = Categorical(probs)
        action = m.sample()
        return action.item(), m.log_prob(action)

    def evaluate(self, states: torch.Tensor, actions: torch.Tensor) -> torch.Tensor:
        probs = self(states)
        m = Categorical(probs)
        return m.log_prob(actions)


class ContinuousPolicy(nn.Module):
    """Policy network for continuous action spaces (MuJoCo)."""

    def __init__(self, obs_dim: int, act_dim: int):
        super().__init__()
        self.fc = nn.Sequential(
            nn.Linear(obs_dim, 256),
            nn.ReLU(),
            nn.Linear(256, 256),
            nn.ReLU(),
        )
        self.mean = nn.Linear(256, act_dim)
        self.log_std = nn.Parameter(torch.zeros(act_dim))

    def forward(self, x):
        x = self.fc(x)
        mean = self.mean(x)
        std = self.log_std.exp()
        return mean, std

    def get_action(self, state: torch.Tensor) -> Tuple[np.ndarray, torch.Tensor]:
        mean, std = self(state)
        m = Normal(mean, std)
        action = m.sample()
        log_prob = m.log_prob(action).sum(dim=-1)
        return action.squeeze(0).numpy(), log_prob

    def evaluate(self, states: torch.Tensor, actions: torch.Tensor) -> torch.Tensor:
        mean, std = self(states)
        m = Normal(mean, std)
        return m.log_prob(actions).sum(dim=-1)


def create_policy(env_config: EnvConfig) -> nn.Module:
    if env_config.continuous:
        return ContinuousPolicy(env_config.obs_dim, env_config.act_dim)
    return DiscretePolicy(env_config.obs_dim, env_config.act_dim)

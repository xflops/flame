from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Sequence


ENV_ALIASES = {
    "cartpole": "CartPole-v1",
    "acrobot": "Acrobot-v1",
    "mountaincar": "MountainCar-v0",
    "lunarlander": "LunarLander-v3",
}


@dataclass(frozen=True)
class DiscreteEnvSpec:
    name: str
    obs_dim: int
    action_dim: int


def resolve_env_name(env_name: str) -> str:
    return ENV_ALIASES.get(env_name.lower(), env_name)


def inspect_discrete_env(env_name: str) -> DiscreteEnvSpec:
    import numpy as np
    import gymnasium as gym
    from gymnasium import spaces

    resolved = resolve_env_name(env_name)
    env = gym.make(resolved)
    try:
        if not isinstance(env.observation_space, spaces.Box):
            raise ValueError(
                f"{resolved} has unsupported observation space "
                f"{env.observation_space!r}; expected gymnasium.spaces.Box"
            )
        if not isinstance(env.action_space, spaces.Discrete):
            raise ValueError(
                f"{resolved} has unsupported action space {env.action_space!r}; "
                "this DQN example only supports discrete actions"
            )

        return DiscreteEnvSpec(
            name=resolved,
            obs_dim=int(np.prod(env.observation_space.shape)),
            action_dim=int(env.action_space.n),
        )
    finally:
        env.close()


def flatten_observation(value) -> list[float]:
    import numpy as np

    return np.asarray(value, dtype=np.float32).reshape(-1).tolist()


def build_policy(
    obs_dim: int,
    action_dim: int,
    hidden_dim: int = 128,
):
    """Create the TorchRL Q-value policy used by collectors and learner."""
    from torch import nn
    from torchrl.data import Categorical
    from torchrl.modules import QValueActor
    from tensordict.nn import TensorDictModule

    qnet = TensorDictModule(
        nn.Sequential(
            nn.Linear(obs_dim, hidden_dim),
            nn.ReLU(),
            nn.Linear(hidden_dim, action_dim),
        ),
        in_keys=["observation"],
        out_keys=["action_value"],
    )
    return QValueActor(qnet, in_keys=["observation"], spec=Categorical(n=action_dim))


def build_loss_and_updater(
    policy,
    gamma: float = 0.99,
    target_update_tau: float = 0.05,
):
    from torchrl.objectives import DQNLoss, SoftUpdate

    loss_fn = DQNLoss(policy, action_space="categorical")
    loss_fn.make_value_estimator(gamma=gamma)
    target_updater = SoftUpdate(loss_fn, tau=target_update_tau)
    return loss_fn, target_updater


def is_tensordict_like(data: Any) -> bool:
    return hasattr(data, "batch_size") and hasattr(data, "keys")


def transitions_to_tensordict(transitions: Sequence[dict] | Any):
    if is_tensordict_like(transitions):
        return transitions

    import torch
    from tensordict import TensorDict

    batch_size = len(transitions)
    observations = torch.tensor(
        [item["observation"] for item in transitions],
        dtype=torch.float32,
    )
    actions = torch.tensor(
        [item["action"] for item in transitions],
        dtype=torch.long,
    )
    next_observations = torch.tensor(
        [item["next_observation"] for item in transitions],
        dtype=torch.float32,
    )
    rewards = torch.tensor(
        [item["reward"] for item in transitions],
        dtype=torch.float32,
    ).unsqueeze(-1)
    dones = torch.tensor(
        [item["done"] for item in transitions],
        dtype=torch.bool,
    ).unsqueeze(-1)
    terminated = torch.tensor(
        [item["terminated"] for item in transitions],
        dtype=torch.bool,
    ).unsqueeze(-1)

    return TensorDict(
        {
            "observation": observations,
            "action": actions,
            "next": TensorDict(
                {
                    "observation": next_observations,
                    "reward": rewards,
                    "done": dones,
                    "terminated": terminated,
                },
                batch_size=[batch_size],
            ),
        },
        batch_size=[batch_size],
    )

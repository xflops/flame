from replay_buffer import ReplayBuffer


class Collector:
    def __init__(self, env_name: str):
        import gymnasium as gym

        self.env = gym.make(env_name)
        self.state, _ = self.env.reset()
        self.episode_reward = 0.0
        self.episode_count = 0
        self.total_reward = 0.0

    def collect(self, buffer: ReplayBuffer, num_steps: int) -> dict:
        transitions = []

        for _ in range(num_steps):
            action = self.env.action_space.sample()
            next_state, reward, terminated, truncated, _ = self.env.step(action)
            done = terminated or truncated
            self.episode_reward += reward

            transitions.append(
                {
                    "state": self.state.tolist(),
                    "action": int(action),
                    "reward": float(reward),
                    "next_state": next_state.tolist(),
                    "done": done,
                }
            )

            if done:
                self.state, _ = self.env.reset()
                self.total_reward += self.episode_reward
                self.episode_count += 1
                self.episode_reward = 0.0
            else:
                self.state = next_state

        buffer.push(transitions)

        return {
            "num_transitions": len(transitions),
            "episode_count": self.episode_count,
            "avg_reward": self.total_reward / max(1, self.episode_count),
        }

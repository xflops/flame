"""GRPO bookkeeping shared by the driver and the trainers: advantages and metrics."""

from collections import defaultdict


def advantages(trajectories: list[dict]) -> tuple[list[dict], dict]:
    """Group-normalised advantages. Returns (samples to train on, batch metrics).

    Samples whose group all scored the same carry no signal and are left out.
    """
    groups: dict[str, list[dict]] = defaultdict(list)
    for trajectory in trajectories:
        groups[trajectory["prompt_id"]].append(trajectory)
    samples = []
    for group in groups.values():
        rewards = [t["reward"] for t in group]
        mean = sum(rewards) / len(rewards)
        spread = (sum((r - mean) ** 2 for r in rewards) / len(rewards)) ** 0.5
        if spread < 1e-6:
            continue
        samples.extend({**t, "advantage": (t["reward"] - mean) / (spread + 1e-6)} for t in group)
    count = max(1, len(trajectories))
    metrics = {
        "reward": sum(t["reward"] for t in trajectories) / count,
        "samples": len(trajectories),
        "trained": len(samples),
        "turns": sum(t["turns"] for t in trajectories) / count,
        "searches": sum(t["searches"] for t in trajectories) / count,
        "response_tokens": sum(sum(t["mask"]) for t in trajectories) / count,
        "answered": sum(t["answer"] is not None for t in trajectories) / count,
    }
    return samples, metrics

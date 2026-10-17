"""Weight deltas: what the trainer publishes after the first export and the rollouts apply.

A part is a safetensors blob. `name` holds a whole tensor; `name.idx` and `name.val` hold the
flat indices and new values of the elements that changed. Weights are bf16, and a learning rate
as small as GRPO's leaves most elements as they were.
"""

from typing import Optional


def encode(names: list[str], current: dict, previous: Optional[dict]) -> tuple[bytes, int]:
    """The part for `names`: every tensor if there is no `previous`, the changed elements after.

    Returns the blob and the number of elements it carries.
    """
    import torch
    from safetensors.torch import save

    part, count = {}, 0
    for name in names:
        tensor = current[name]
        if previous is None:
            part[name] = tensor.cpu().contiguous()
            count += tensor.numel()
            continue
        changed = (tensor != previous[name]).flatten().nonzero().flatten()
        if changed.numel() == 0:
            continue
        count += changed.numel()
        # An index and a value cost 6 bytes per element against 2 for the whole tensor.
        if changed.numel() * 3 > tensor.numel():
            part[name] = tensor.cpu().contiguous()
        else:
            part[f"{name}.idx"] = changed.to(torch.int32).cpu()
            part[f"{name}.val"] = tensor.flatten()[changed].cpu()
    return save(part), count


def apply(state: dict, part: bytes) -> set[str]:
    """Apply a part to `state` (name to CPU tensor); return the names it changed."""
    from safetensors.torch import load

    tensors = load(part)
    changed = set()
    for name, tensor in tensors.items():
        if name.endswith(".val"):
            continue
        if name.endswith(".idx"):
            name = name[: -len(".idx")]
            state[name].view(-1)[tensor.long()] = tensors[f"{name}.val"]
        else:
            state[name] = tensor.clone()
        changed.add(name)
    return changed

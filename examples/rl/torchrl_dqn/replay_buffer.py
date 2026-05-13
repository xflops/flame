from __future__ import annotations

import logging
import random
from typing import Any, Iterable, TYPE_CHECKING

if TYPE_CHECKING:
    from flamepy.runner import Runner

try:
    from torchrl.data.replay_buffers.storages import Storage as _TorchRLStorage
except ImportError as exc:  # Keep `main.py --help` dependency-light.
    _TORCHRL_STORAGE_IMPORT_ERROR: ImportError | None = exc
    _TorchRLStorage = object
else:
    _TORCHRL_STORAGE_IMPORT_ERROR = None


REPLAY_SIMPLE = "simple"
REPLAY_SHARDED = "sharded"
REPLAY_CHOICES = (REPLAY_SIMPLE, REPLAY_SHARDED)


def _validate_positive(name: str, value: int) -> None:
    if value < 1:
        raise ValueError(f"{name} must be at least 1")


def _validate_non_negative(name: str, value: int) -> None:
    if value < 0:
        raise ValueError(f"{name} must be non-negative")


def _is_tensordict_like(data: Any) -> bool:
    return (
        hasattr(data, "batch_size")
        and hasattr(data, "keys")
        and "observation" in data.keys()
    )


def _batch_len(batch: Any) -> int:
    if batch is None:
        return 0
    if _is_tensordict_like(batch):
        if len(batch.batch_size) == 0:
            return 1
        return int(batch.batch_size[0])
    return len(batch)


def _dicts_to_tensordict(transitions: Iterable[dict]):
    import torch
    from tensordict import TensorDict

    items = [dict(item) for item in transitions]
    batch_size = len(items)
    return TensorDict(
        {
            "observation": torch.tensor(
                [item["observation"] for item in items],
                dtype=torch.float32,
            ),
            "action": torch.tensor(
                [item["action"] for item in items],
                dtype=torch.long,
            ),
            "next": TensorDict(
                {
                    "observation": torch.tensor(
                        [item["next_observation"] for item in items],
                        dtype=torch.float32,
                    ),
                    "reward": torch.tensor(
                        [item["reward"] for item in items],
                        dtype=torch.float32,
                    ).unsqueeze(-1),
                    "done": torch.tensor(
                        [item["done"] for item in items],
                        dtype=torch.bool,
                    ).unsqueeze(-1),
                    "terminated": torch.tensor(
                        [item["terminated"] for item in items],
                        dtype=torch.bool,
                    ).unsqueeze(-1),
                },
                batch_size=[batch_size],
            ),
        },
        batch_size=[batch_size],
    )


def _as_tensordict_batch(data: Any):
    if data is None:
        return None

    if _is_tensordict_like(data):
        if len(data.batch_size) == 0:
            data = data.unsqueeze(0)
        return data.detach().cpu()

    if isinstance(data, dict):
        return _dicts_to_tensordict([data])

    if isinstance(data, list):
        return _dicts_to_tensordict(data)

    if isinstance(data, tuple):
        return _dicts_to_tensordict(data)

    if isinstance(data, Iterable) and not isinstance(data, (str, bytes)):
        return _dicts_to_tensordict(data)

    raise TypeError(f"unsupported replay transition batch type: {type(data)!r}")


def _is_scalar_index(index: Any) -> bool:
    if isinstance(index, int):
        return True
    ndim = getattr(index, "ndim", None)
    return ndim == 0


def _select_batch(batch: Any, index: Any) -> Any:
    if isinstance(index, tuple):
        if len(index) != 1:
            raise RuntimeError("FlameObjectStorage expects flat 1-D indices")
        index = index[0]

    if batch is None:
        raise IndexError("cannot index an empty replay storage")

    return batch[int(index)] if _is_scalar_index(index) else batch[index]


def _trim_batch(batch: Any, max_size: int) -> Any:
    overflow = _batch_len(batch) - max_size
    if overflow <= 0:
        return batch
    return batch[overflow:].contiguous()


def _concat_batches(batches: Iterable[Any], max_size: int | None = None) -> Any:
    import torch

    non_empty = [batch for batch in batches if _batch_len(batch) > 0]
    if not non_empty:
        return None
    if len(non_empty) == 1:
        result = non_empty[0]
    else:
        result = torch.cat(non_empty, dim=0)
    if max_size is not None:
        result = _trim_batch(result, max_size)
    return result


def _clone_batch(batch: Any) -> Any:
    return None if batch is None else batch.clone()


def _process_sample_batch(batch: Any, sample_work: int) -> Any:
    if sample_work <= 0:
        return batch

    if not _is_tensordict_like(batch):
        return batch

    import torch

    was_scalar = len(batch.batch_size) == 0
    working = batch.unsqueeze(0) if was_scalar else batch
    row_count = _batch_len(working)
    if row_count == 0:
        return batch

    observations = working["observation"].reshape(row_count, -1)
    next_observations = working["next", "observation"].reshape(row_count, -1)
    rewards = working["next", "reward"].reshape(row_count, -1)[:, 0]
    actions = working["action"].reshape(row_count, -1)[:, 0]

    checksums = []
    for row in range(row_count):
        checksum = 0.0
        observation = observations[row]
        next_observation = next_observations[row]
        reward = float(rewards[row])
        action = float(actions[row])
        for index in range(sample_work):
            obs_value = float(observation[index % observation.numel()])
            next_value = float(next_observation[index % next_observation.numel()])
            checksum = (checksum * 1.000001) + obs_value * 0.17 + next_value * 0.13
            checksum += reward * 0.07 + action * 0.03
        checksums.append(checksum)

    result = working.clone()
    result["sample_work_checksum"] = torch.tensor(
        checksums,
        dtype=torch.float32,
    ).unsqueeze(-1)
    return result.squeeze(0) if was_scalar else result


def _require_torchrl_storage() -> None:
    if _TORCHRL_STORAGE_IMPORT_ERROR is not None:
        raise RuntimeError(
            "TorchRL is required to construct replay-buffer storage"
        ) from _TORCHRL_STORAGE_IMPORT_ERROR


class FlameObjectStorage(_TorchRLStorage):
    """TorchRL storage backed by a Flame object plus append-only patches."""

    ndim = 1

    def __init__(self, rr: "Runner", max_size: int, sample_work: int = 0):
        _require_torchrl_storage()
        super().__init__(max_size)

        from flamepy.core import get_object, patch_object, update_object

        _validate_positive("max_size", max_size)
        _validate_non_negative("sample_work", sample_work)

        self.max_size = max_size
        self.sample_work = sample_work
        self.object_ref = rr.put_object({"batch": None, "total_added": 0})
        self._get_object = get_object
        self._patch_object = patch_object
        self._update_object = update_object
        self._materialized_base = None
        self._materialized_batch = None
        self._materialized_total_added = 0
        self._materialized_patch_count = 0

    @property
    def size(self) -> int:
        return len(self)

    @property
    def total_added(self) -> int:
        return self.state()["total_added"]

    def set(self, cursor: Any, data: Any, *, set_cursor: bool = True):
        del cursor, set_cursor
        batch = _as_tensordict_batch(data)
        if _batch_len(batch) == 0:
            return

        self.object_ref = self._patch_object(self.object_ref, batch)

    def get(self, index: Any) -> Any:
        selection = _select_batch(self._fetch_state().get("batch"), index)
        return _process_sample_batch(selection, self.sample_work)

    def __len__(self) -> int:
        return self.state()["size"]

    def state(self) -> dict[str, int]:
        return self._get_object(self.object_ref, deserializer=self._count_deserializer)

    def state_dict(self) -> dict[str, Any]:
        return self.snapshot()

    def load_state_dict(self, state_dict: dict[str, Any]) -> None:
        self.object_ref = self._update_object(self.object_ref, state_dict)
        self._reset_materialized()

    def _empty(self) -> None:
        self.object_ref = self._update_object(
            self.object_ref,
            {"batch": None, "total_added": 0},
        )
        self._reset_materialized()

    def snapshot(self) -> dict[str, Any]:
        data = self._fetch_state()
        return {
            "batch": _clone_batch(data.get("batch")),
            "total_added": data.get("total_added", 0),
        }

    def _count_deserializer(self, base: dict, deltas: list[Any]) -> dict[str, int]:
        base_size = _batch_len(base.get("batch"))
        delta_size = sum(_batch_len(delta) for delta in deltas)
        total_added = base.get("total_added", base_size) + delta_size
        return {
            "size": min(self.max_size, base_size + delta_size),
            "total_added": total_added,
        }

    def _deserializer(self, base: dict, deltas: list[Any]) -> dict:
        if (
            self._materialized_batch is None
            or self._materialized_base is not base
            or self._materialized_patch_count > len(deltas)
        ):
            self._materialized_base = base
            self._materialized_batch = _trim_batch(
                _clone_batch(base.get("batch")),
                self.max_size,
            )
            self._materialized_total_added = base.get(
                "total_added",
                _batch_len(self._materialized_batch),
            )
            self._materialized_patch_count = 0

        for delta in deltas[self._materialized_patch_count :]:
            self._materialized_batch = _concat_batches(
                [self._materialized_batch, delta],
                max_size=self.max_size,
            )
            self._materialized_total_added += _batch_len(delta)

        self._materialized_patch_count = len(deltas)
        return {
            "batch": self._materialized_batch,
            "total_added": self._materialized_total_added,
        }

    def _fetch_state(self) -> dict:
        return self._get_object(self.object_ref, deserializer=self._deserializer)

    def _reset_materialized(self) -> None:
        self._materialized_base = None
        self._materialized_batch = None
        self._materialized_total_added = 0
        self._materialized_patch_count = 0


class LocalObjectStorage(_TorchRLStorage):
    """In-process storage with the same append-log semantics as Flame storage."""

    ndim = 1

    def __init__(self, max_size: int, sample_work: int = 0):
        _require_torchrl_storage()
        super().__init__(max_size)

        _validate_positive("max_size", max_size)
        _validate_non_negative("sample_work", sample_work)

        self.sample_work = sample_work
        self.batch = None
        self.total_added = 0

    def set(self, cursor: Any, data: Any, *, set_cursor: bool = True):
        del cursor, set_cursor
        batch = _as_tensordict_batch(data)
        if _batch_len(batch) == 0:
            return

        self.batch = _concat_batches([self.batch, batch], max_size=self.max_size)
        self.total_added += _batch_len(batch)

    def get(self, index: Any) -> Any:
        selection = _select_batch(self.batch, index)
        return _process_sample_batch(selection, self.sample_work)

    def __len__(self) -> int:
        return _batch_len(self.batch)

    def state(self) -> dict[str, int]:
        return {
            "size": _batch_len(self.batch),
            "total_added": self.total_added,
        }

    def state_dict(self) -> dict[str, Any]:
        return {
            "batch": _clone_batch(self.batch),
            "total_added": self.total_added,
        }

    def load_state_dict(self, state_dict: dict[str, Any]) -> None:
        self.batch = _trim_batch(_clone_batch(state_dict.get("batch")), self.max_size)
        self.total_added = state_dict.get("total_added", _batch_len(self.batch))

    def _empty(self) -> None:
        self.batch = None
        self.total_added = 0


def identity_collate(transitions):
    return transitions


def _make_replay_buffer(storage):
    from torchrl.data import ReplayBuffer

    logging.getLogger("torchrl").setLevel(logging.WARNING)
    return ReplayBuffer(
        storage=storage,
        collate_fn=identity_collate,
    )


def _sample_replay_buffer(replay_buffer, batch_size: int):
    if batch_size <= 0 or len(replay_buffer) == 0:
        return None
    return replay_buffer.sample(min(batch_size, len(replay_buffer)))


def _storage_state(replay_buffer) -> dict[str, int]:
    storage = getattr(replay_buffer, "storage", None)
    if storage is not None and hasattr(storage, "state"):
        return storage.state()

    size = len(replay_buffer)
    return {"size": size, "total_added": size}


def replay_buffer_shard_states(replay_buffers: list) -> list[dict[str, int]]:
    return [_storage_state(replay_buffer) for replay_buffer in replay_buffers]


class LocalReplayBuffer:
    def __init__(self, max_size: int = 10000, sample_work: int = 0):
        _validate_positive("max_size", max_size)

        self.storage = LocalObjectStorage(max_size=max_size, sample_work=sample_work)
        self.replay_buffer = _make_replay_buffer(self.storage)

    @property
    def shard_count(self) -> int:
        return 1

    def extend(self, transitions: Any) -> None:
        batch = _as_tensordict_batch(transitions)
        if _batch_len(batch) > 0:
            self.replay_buffer.extend(batch)

    def push(self, transitions: Any) -> None:
        self.extend(transitions)

    def state(self) -> dict[str, int]:
        return {**self.storage.state(), "shards": 1}

    def sample(self, batch_size: int):
        return _sample_replay_buffer(self.replay_buffer, batch_size)

    def sample_shard(self, shard_index: int, batch_size: int):
        if shard_index != 0:
            raise ValueError("single replay buffer only has shard 0")
        return self.sample(batch_size)


class LocalShardedReplayBuffer:
    def __init__(
        self,
        max_size: int = 10000,
        num_shards: int = 4,
        sample_work: int = 0,
    ):
        _validate_positive("max_size", max_size)
        _validate_positive("num_shards", num_shards)

        self.num_shards = num_shards
        shard_size = max(1, (max_size + num_shards - 1) // num_shards)
        self.shards = [
            LocalReplayBuffer(max_size=shard_size, sample_work=sample_work)
            for _ in range(num_shards)
        ]

    @property
    def shard_count(self) -> int:
        return self.num_shards

    def extend(self, transitions: Any) -> None:
        batch = _as_tensordict_batch(transitions)
        if _batch_len(batch) > 0:
            self.shards[random.randrange(self.num_shards)].extend(batch)

    def push(self, transitions: Any) -> None:
        self.extend(transitions)

    def state(self) -> dict[str, int]:
        states = [shard.state() for shard in self.shards]
        return {
            "size": sum(state["size"] for state in states),
            "total_added": sum(state["total_added"] for state in states),
            "shards": self.num_shards,
        }

    def sample_shard(self, shard_index: int, batch_size: int):
        for offset in range(self.num_shards):
            candidate = (shard_index + offset) % self.num_shards
            if self.shards[candidate].state()["size"] > 0:
                return self.shards[candidate].sample(batch_size)
        return None

    def sample(self, batch_size: int):
        request_sizes = split_batch(batch_size, self.num_shards)
        batches = [
            self.sample_shard(shard_index, request_size)
            for shard_index, request_size in enumerate(request_sizes)
            if request_size > 0
        ]
        return _concat_batches(batches)


def split_batch(batch_size: int, parts: int) -> list[int]:
    _validate_positive("batch_size", batch_size)
    _validate_positive("parts", parts)

    request_count = min(batch_size, parts)
    base_size, remainder = divmod(batch_size, request_count)
    return [
        base_size + (1 if request_index < remainder else 0)
        for request_index in range(request_count)
    ]


def create_flame_replay_buffers(
    rr: "Runner",
    replay: str,
    buffer_size: int,
    replay_shards: int,
    sample_work: int,
):
    _validate_positive("buffer_size", buffer_size)

    if replay == REPLAY_SIMPLE:
        return [
            _make_replay_buffer(
                FlameObjectStorage(
                    rr,
                    max_size=buffer_size,
                    sample_work=sample_work,
                )
            )
        ]
    if replay == REPLAY_SHARDED:
        _validate_positive("replay_shards", replay_shards)
        shard_size = max(1, (buffer_size + replay_shards - 1) // replay_shards)
        return [
            _make_replay_buffer(
                FlameObjectStorage(
                    rr,
                    max_size=shard_size,
                    sample_work=sample_work,
                )
            )
            for _ in range(replay_shards)
        ]
    raise ValueError(f"unsupported replay mode: {replay}")


def create_local_replay_buffer(
    replay: str,
    buffer_size: int,
    replay_shards: int,
    sample_work: int,
):
    if replay == REPLAY_SIMPLE:
        return LocalReplayBuffer(max_size=buffer_size, sample_work=sample_work)
    if replay == REPLAY_SHARDED:
        return LocalShardedReplayBuffer(
            max_size=buffer_size,
            num_shards=replay_shards,
            sample_work=sample_work,
        )
    raise ValueError(f"unsupported replay mode: {replay}")

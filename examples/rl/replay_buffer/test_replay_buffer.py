from flamepy.core import ObjectRef
from main import DEFAULT_FULL_GET_MERGE_EVERY, _resolve_merge_every
from replay_buffer import ReplayBuffer


class FakeRunner:
    def put_object(self, obj):
        return ObjectRef(
            endpoint="grpc://host:9090", key="app/session/replay", version=1
        )


def make_replay_buffer() -> ReplayBuffer:
    return ReplayBuffer(FakeRunner())


def transition(idx: int) -> dict:
    return {"id": idx}


def test_deserializer_applies_only_new_patch_batches():
    buffer = make_replay_buffer()
    base = {"transitions": [transition(0)], "total_added": 1}
    patch_1 = [transition(1)]
    patch_2 = [transition(2)]

    materialized = buffer._deserializer(base, [patch_1])
    assert materialized == {
        "transitions": [transition(0), transition(1)],
        "total_added": 2,
    }

    materialized_again = buffer._deserializer(base, [patch_1, patch_2])

    assert materialized_again is materialized
    assert materialized_again == {
        "transitions": [transition(0), transition(1), transition(2)],
        "total_added": 3,
    }

    unchanged = buffer._deserializer(base, [patch_1, patch_2])
    assert unchanged is materialized
    assert unchanged == materialized_again


def test_deserializer_resets_after_base_replacement():
    buffer = make_replay_buffer()
    old_base = {"transitions": [transition(0)], "total_added": 1}
    new_base = {"transitions": [transition(10)], "total_added": 1}

    old_materialized = buffer._deserializer(old_base, [[transition(1)]])
    new_materialized = buffer._deserializer(new_base, [])

    assert new_materialized is not old_materialized
    assert new_materialized == {
        "transitions": [transition(10)],
        "total_added": 1,
    }


def test_deserializer_resets_when_patch_history_shrinks():
    buffer = make_replay_buffer()
    base = {"transitions": [transition(0)], "total_added": 1}
    patch_1 = [transition(1)]
    patch_2 = [transition(2)]

    buffer._deserializer(base, [patch_1, patch_2])
    materialized = buffer._deserializer(base, [patch_1])

    assert materialized == {
        "transitions": [transition(0), transition(1)],
        "total_added": 2,
    }


def test_incremental_mode_disables_merge_by_default():
    assert (
        _resolve_merge_every(
            force_full_get=False,
            requested_merge_every=None,
            no_merge=False,
        )
        is None
    )


def test_full_get_mode_keeps_merge_by_default():
    assert (
        _resolve_merge_every(
            force_full_get=True,
            requested_merge_every=None,
            no_merge=False,
        )
        == DEFAULT_FULL_GET_MERGE_EVERY
    )


def test_merge_policy_allows_explicit_override():
    assert (
        _resolve_merge_every(
            force_full_get=False,
            requested_merge_every=3,
            no_merge=False,
        )
        == 3
    )
    assert (
        _resolve_merge_every(
            force_full_get=True,
            requested_merge_every=3,
            no_merge=True,
        )
        is None
    )

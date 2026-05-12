import random
from typing import Any, List, TYPE_CHECKING

if TYPE_CHECKING:
    from flamepy.runner import Runner


class ReplayBuffer:
    """Replay-buffer service used by flamepy.runner.

    The incremental deserializer keeps process-local materialized state. This
    example relies on flamepy.runner invoking a service instance serially; add
    synchronization before using the same instance from multiple threads.
    """

    def __init__(self, rr: "Runner", force_full_get: bool = False):
        from flamepy.core import ObjectRef, get_object, patch_object, update_object

        self.buffer_ref = rr.put_object({"transitions": [], "total_added": 0})
        self.force_full_get = force_full_get
        self._object_ref = ObjectRef
        self._get_object = get_object
        self._update_object = update_object
        self._patch_object = patch_object
        self._materialized_base = None
        self._materialized_data: dict[str, Any] | None = None
        self._materialized_patch_count = 0

    def _deserializer(self, base: dict, deltas: List) -> dict:
        if (
            self._materialized_data is None
            or self._materialized_base is not base
            or self._materialized_patch_count > len(deltas)
        ):
            self._materialized_base = base
            self._materialized_data = {
                "transitions": list(base.get("transitions", [])),
                "total_added": base.get("total_added", 0),
            }
            self._materialized_patch_count = 0

        for delta in deltas[self._materialized_patch_count :]:
            self._materialized_data["transitions"].extend(delta)
            self._materialized_data["total_added"] += len(delta)

        self._materialized_patch_count = len(deltas)
        return self._materialized_data

    def _fetch(self) -> dict:
        ref = self.buffer_ref
        if self.force_full_get:
            ref = self._object_ref(endpoint=ref.endpoint, key=ref.key, version=0)
        return self._get_object(ref, deserializer=self._deserializer)

    def push(self, transitions: List[dict]) -> None:
        self._patch_object(self.buffer_ref, transitions)

    def merge(self) -> None:
        data = self._fetch()
        self.buffer_ref = self._update_object(self.buffer_ref, data)

    def get(self) -> dict:
        return self._fetch()

    def state(self) -> dict[str, Any]:
        data = self._fetch()
        return {
            "size": len(data.get("transitions", [])),
            "total_added": data.get("total_added", 0),
        }

    def sample(self, batch_size: int) -> List[dict]:
        data = self._fetch()
        items = data.get("transitions", [])
        return random.sample(items, min(batch_size, len(items)))

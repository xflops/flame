import random
from typing import Any, List, TYPE_CHECKING

if TYPE_CHECKING:
    from flamepy.runner import Runner


class ReplayBuffer:
    def __init__(self, rr: "Runner"):
        from flamepy.core import get_object, patch_object, update_object

        self.buffer_ref = rr.put_object({"transitions": [], "total_added": 0})
        self._get_object = get_object
        self._update_object = update_object
        self._patch_object = patch_object

    def _deserializer(self, base: dict, deltas: List) -> dict:
        transitions = list(base.get("transitions", []))
        for delta in deltas:
            transitions.extend(delta)
        return {
            "transitions": transitions,
            "total_added": base.get("total_added", 0) + sum(len(d) for d in deltas),
        }

    def _fetch(self) -> dict:
        return self._get_object(self.buffer_ref, deserializer=self._deserializer)

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

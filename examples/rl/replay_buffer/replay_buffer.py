import random
from typing import Any, List

import flamepy.app as app


class ReplayBuffer:
    """Replay-buffer service used by flamepy.app.

    The incremental deserializer keeps process-local materialized state per
    service instance. Parallel requests may run on different fixed service
    instances, so correctness comes from the shared ObjectRef and each instance
    maintains its own best-effort materialized cache.
    """

    def __init__(self, read_all_parts: bool = False):
        from flamepy.core import ObjectRef, get_object, patch_object

        self.buffer_ref = app.put({"transitions": [], "total_added": 0})
        self.read_all_parts = read_all_parts
        self._object_ref = ObjectRef
        self._get_object = get_object
        self._patch_object = patch_object
        self._materialized_first = None
        self._materialized_data: dict[str, Any] | None = None
        self._materialized_appended_count = 0

    def _deserializer(self, parts: List) -> dict:
        first, *appended = parts
        if (
            self._materialized_data is None
            or self._materialized_first is not first
            or self._materialized_appended_count > len(appended)
        ):
            self._materialized_first = first
            self._materialized_data = {
                "transitions": list(first.get("transitions", [])),
                "total_added": first.get("total_added", 0),
            }
            self._materialized_appended_count = 0

        for data in appended[self._materialized_appended_count :]:
            self._materialized_data["transitions"].extend(data)
            self._materialized_data["total_added"] += len(data)

        self._materialized_appended_count = len(appended)
        return self._materialized_data

    def _fetch(self) -> dict:
        ref = self.buffer_ref
        if self.read_all_parts:
            ref = self._object_ref(endpoint=ref.endpoint, key=ref.key, version=0)
        return self._get_object(ref, deserializer=self._deserializer)

    def push(self, transitions: List[dict]) -> None:
        self._patch_object(self.buffer_ref, transitions)

    def merge(self) -> None:
        # An object only grows by appended data, so the merged buffer is a new object.
        data = self._fetch()
        self.buffer_ref = app.put(data)

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

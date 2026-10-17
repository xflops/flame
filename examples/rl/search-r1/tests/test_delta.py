"""Weight deltas round-trip: applying the parts in order rebuilds the published weights."""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

try:
    import torch
    import safetensors  # noqa: F401
except ImportError:  # the example's own dependencies are installed on the executors only
    torch = None

if torch is not None:
    import delta


@unittest.skipIf(torch is None, "needs torch and safetensors")
class DeltaTest(unittest.TestCase):
    def weights(self, seed):
        generator = torch.Generator().manual_seed(seed)
        return {"a": torch.randn(64, 32, generator=generator).bfloat16(), "b": torch.randn(100, generator=generator).bfloat16()}

    def test_deltas_rebuild_every_version(self):
        names = ["a", "b"]
        versions = [self.weights(0)]
        # Few elements change at first, then most of a tensor does.
        step = {k: v.clone() for k, v in versions[0].items()}
        step["a"].view(-1)[[3, 40, 2000]] += 1
        versions.append(step)
        step = {k: v.clone() for k, v in versions[1].items()}
        step["b"] = step["b"] + 1
        versions.append(step)
        versions.append({k: v.clone() for k, v in versions[2].items()})

        parts = [delta.encode(names, versions[0], None)[0]]
        parts += [delta.encode(names, now, before)[0] for before, now in zip(versions, versions[1:])]

        state = {}
        for index, part in enumerate(parts):
            delta.apply(state, part)
            for name in names:
                self.assertTrue(torch.equal(state[name], versions[index][name]), (index, name))

    def test_unchanged_weights_make_an_empty_part(self):
        current = self.weights(1)
        part, count = delta.encode(["a", "b"], current, {k: v.clone() for k, v in current.items()})
        self.assertEqual(count, 0)
        self.assertEqual(delta.apply({}, part), set())

    def test_a_sparse_change_is_small(self):
        before = self.weights(2)
        after = {k: v.clone() for k, v in before.items()}
        after["a"].view(-1)[5] += 1
        part, count = delta.encode(["a", "b"], after, before)
        self.assertEqual(count, 1)
        self.assertLess(len(part), 200)

    def test_applied_changes_are_reported(self):
        before = self.weights(3)
        after = {k: v.clone() for k, v in before.items()}
        after["b"].view(-1)[0] += 1
        state = {}
        delta.apply(state, delta.encode(["a", "b"], before, None)[0])
        self.assertEqual(delta.apply(state, delta.encode(["a", "b"], after, before)[0]), {"b"})


if __name__ == "__main__":
    unittest.main()

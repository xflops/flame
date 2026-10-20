"""Pure-Python checks for the shared code; no GPU, vLLM or cluster needed.

    cd examples/rl/search-r1 && python3 -m unittest discover -s tests -v
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from data import exact_match, last_answer, normalize_answer  # noqa: E402
from grpo import advantages  # noqa: E402


def trajectory(prompt_id, reward, answer="x"):
    return {"prompt_id": prompt_id, "reward": reward, "turns": 2, "searches": 1, "mask": [1, 1, 0, 1], "answer": answer}


class DataTest(unittest.TestCase):
    def test_normalize_drops_case_punctuation_and_articles(self):
        self.assertEqual(normalize_answer("The Eiffel-Tower!"), "eiffeltower")

    def test_exact_match(self):
        self.assertTrue(exact_match(" An Apple ", ["apple"]))
        self.assertFalse(exact_match("pear", ["apple"]))
        self.assertFalse(exact_match(None, ["apple"]))

    def test_last_answer_takes_the_final_block(self):
        text = "<answer> a </answer> more <answer>b</answer>"
        self.assertEqual(last_answer(text), "b")
        self.assertIsNone(last_answer("no answer here"))


class GrpoTest(unittest.TestCase):
    def test_advantages_are_group_normalised(self):
        samples, metrics = advantages([trajectory("q", 1.0), trajectory("q", 0.0), trajectory("q", 1.0), trajectory("q", 0.0)])
        self.assertEqual(len(samples), 4)
        self.assertAlmostEqual(sum(s["advantage"] for s in samples), 0.0, places=6)
        self.assertAlmostEqual(metrics["reward"], 0.5)

    def test_groups_without_spread_carry_no_signal(self):
        samples, metrics = advantages([trajectory("same", 1.0), trajectory("same", 1.0), trajectory("mixed", 1.0), trajectory("mixed", 0.0)])
        self.assertEqual({s["prompt_id"] for s in samples}, {"mixed"})
        self.assertEqual(metrics["trained"], 2)
        self.assertEqual(metrics["samples"], 4)


if __name__ == "__main__":
    unittest.main()

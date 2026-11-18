# Copyright 2026 The Flame Authors.
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#     http://www.apache.org/licenses/LICENSE-2.0
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Durable output and timing aggregation checks."""

import csv
import json
import tempfile
import unittest
from pathlib import Path

from report import ScreenReport


class ReportTest(unittest.TestCase):
    def test_ranking_poses_errors_and_instance_timing(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "run"
            report = ScreenReport(output, {"status": "running"}, {"a": {"role": "active"}})
            common = {"status": "ok", "pose_pdbqt": "MODEL 1\nENDMDL\n", "setup_seconds": 4.0, "map_seconds": 3.0, "dock_seconds": 2.0, "instance_id": "worker-instance"}
            report.add({**common, "name": "a", "affinity": -7.0, "invocation": 1}, "a", 5.0)
            report.add({**common, "name": "b", "affinity": -9.0, "invocation": 2}, "b", 6.0)
            report.add({"name": "c", "status": "error", "error": "bad ligand", "instance_id": "worker-instance", "setup_seconds": 4.0, "map_seconds": 3.0, "dock_seconds": 1.0, "invocation": 3}, "c", 7.0)
            with (output / "results.csv").open() as stream:
                rows = list(csv.DictReader(stream))
            self.assertEqual([row["name"] for row in rows], ["b", "a"])
            self.assertEqual([row["rank"] for row in rows], ["1", "2"])
            self.assertEqual(rows[1]["role"], "active")
            self.assertEqual((output / rows[0]["pose_file"]).read_text(), common["pose_pdbqt"])
            with (output / "failures.csv").open() as stream:
                self.assertEqual(next(csv.DictReader(stream))["error"], "bad ligand")
            summary = json.loads((output / "run.json").read_text())["summary"]
            self.assertEqual(summary["sum_setup_seconds"], 4.0)
            self.assertEqual(summary["sum_map_seconds"], 3.0)
            self.assertEqual(summary["sum_dock_seconds"], 5.0)
            self.assertEqual(summary["warm_invocations"], 2)
            self.assertEqual(summary["succeeded"], 2)
            self.assertEqual(summary["errors"], 1)

    def test_invalid_result_does_not_enter_rankings(self):
        with tempfile.TemporaryDirectory() as directory:
            report = ScreenReport(Path(directory) / "run", {}, {})
            for result in [
                {"name": "other", "status": "ok", "affinity": -1.0, "pose_pdbqt": "pose"},
                {"name": "a", "status": "ok", "affinity": float("nan"), "pose_pdbqt": "pose"},
                {"name": "a", "status": "ok", "affinity": -1.0, "pose_pdbqt": ""},
            ]:
                with self.assertRaises(ValueError):
                    report.add(result, "a", 1.0)
            self.assertEqual(report.rows, [])

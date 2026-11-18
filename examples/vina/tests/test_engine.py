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

"""Checks that do not need AutoDock Vina or a Flame cluster."""

import io
import sys
import tempfile
import types
import unittest
from importlib.metadata import PackageNotFoundError
from pathlib import Path
from unittest.mock import patch

from engine import BOX_SIZE, CENTER, DockJob, VinaScreen, load_job, main


class FakeVina:
    def __init__(self, **kwargs):
        self.kwargs = kwargs
        self.map_calls = 0
        self.ligands = []

    def set_receptor(self, path):
        self.receptor = path

    def compute_vina_maps(self, center, box_size):
        self.map_calls += 1
        self.maps = (center, box_size)

    def info(self):
        return {"seed": self.kwargs["seed"] or 12345}

    def set_ligand_from_string(self, text):
        self.ligands.append(text)
        if text == "invalid":
            raise ValueError("cannot parse ligand")
        if text == "interrupt":
            raise KeyboardInterrupt()
        self.ligand = text

    def dock(self, exhaustiveness, n_poses):
        self.docked = (exhaustiveness, n_poses)

    def energies(self, n_poses):
        if self.ligand == "empty-pose":
            return []
        if self.ligand == "nan-affinity":
            return [[float("nan")]]
        if self.ligand == "infinite-affinity":
            return [[float("inf")]]
        return [[-7.25, -8.0, 0.5, 0.25, 0.5]]

    def poses(self, n_poses):
        self.requested_poses = n_poses
        if self.ligand == "empty-coordinates":
            return ""
        return "MODEL 1\nREMARK VINA RESULT: -7.250\nROOT\nENDROOT\nENDMDL\n"


class LoadJobTest(unittest.TestCase):
    def setUp(self):
        environment = patch.dict("os.environ", {"VINA_EXHAUSTIVENESS": "8", "VINA_SEED": "1"})
        environment.start()
        self.addCleanup(environment.stop)

    def test_bundled_1iep_inputs(self):
        job = load_job()
        self.assertTrue(job.receptor.startswith("ATOM"))
        self.assertEqual(job.center, CENTER)
        self.assertEqual([name for name, _ in job.ligands], ["1iep_ligand"])
        self.assertIn("ROOT", job.ligands[0][1])
        self.assertEqual(job.exhaustiveness, 8)

    def test_reads_every_ligand_pdbqt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data = root / "data"
            ligands = data / "ligands"
            ligands.mkdir(parents=True)
            (data / "1iep_receptor.pdbqt").write_text("ATOM\n")
            (ligands / "b.pdbqt").write_text("ROOT\nB\n")
            (ligands / "a.pdbqt").write_text("ROOT\nA\n")
            (ligands / "note.txt").write_text("skip")
            job = load_job(root)
        self.assertEqual([(name, text) for name, text in job.ligands], [("a", "ROOT\nA\n"), ("b", "ROOT\nB\n")])

    def test_missing_ligands(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "data").mkdir()
            (root / "data" / "ligands").mkdir()
            (root / "data" / "1iep_receptor.pdbqt").write_text("ATOM\n")
            with self.assertRaises(FileNotFoundError):
                load_job(root)


class VinaScreenTest(unittest.TestCase):
    def setUp(self):
        versions = patch("engine.version", side_effect={"vina": "1.2.7", "numpy": "2.4.0"}.__getitem__)
        self._versions = versions.start()
        self.addCleanup(versions.stop)
        module = types.ModuleType("vina")
        module.Vina = FakeVina
        self._previous = sys.modules.get("vina")
        sys.modules["vina"] = module

    def tearDown(self):
        if self._previous is None:
            sys.modules.pop("vina", None)
        else:
            sys.modules["vina"] = self._previous

    def screen(self, **kwargs):
        screen = VinaScreen("ATOM\n", CENTER, (20.0, 20.0, 20.0), **kwargs)
        self.addCleanup(screen._tmpdir.cleanup)
        return screen

    def test_maps_are_built_once_and_results_identify_reuse(self):
        screen = self.screen(exhaustiveness=4, seed=3)
        self.assertTrue(Path(screen._receptor_path).read_text().startswith("ATOM"))
        first = screen.dock("a", "ROOT\n")
        second = screen.dock("b", "ROOT\n")
        self.assertEqual(first["status"], "ok")
        self.assertEqual(first["affinity"], -7.25)
        self.assertIn("MODEL 1", first["pose_pdbqt"])
        self.assertIsNone(first["error"])
        self.assertIsNone(first["error_type"])
        self.assertEqual(second["name"], "b")
        self.assertEqual((first["invocation"], second["invocation"]), (1, 2))
        self.assertEqual(first["instance_id"], second["instance_id"])
        self.assertEqual(first["pid"], second["pid"])
        self.assertTrue(first["hostname"])
        self.assertGreater(first["pid"], 0)
        self.assertEqual(first["seed"], 3)
        self.assertEqual(screen._vina.maps[0], list(CENTER))
        self.assertEqual(screen._vina.map_calls, 1)
        self.assertEqual(screen._vina.kwargs["cpu"], 1)
        self.assertEqual(screen._vina.docked, (4, 1))
        self.assertEqual(screen._vina.requested_poses, 1)
        self.assertNotEqual(first["instance_id"], self.screen().dock("c", "ROOT")["instance_id"])

    def test_constructor_setup_and_map_timing_are_separate_from_docking(self):
        with patch("engine.time.perf_counter", side_effect=[1.0, 2.0, 4.0, 6.0, 10.0, 13.0]):
            screen = self.screen()
            result = screen.dock("a", "ROOT")
        self.assertEqual(result["setup_seconds"], 5.0)
        self.assertEqual(result["map_seconds"], 2.0)
        self.assertEqual(result["dock_seconds"], 3.0)

    def test_reports_actual_generated_seed(self):
        self.assertEqual(self.screen(seed=0).dock("a", "ROOT")["seed"], 12345)

    def test_runtime_versions_are_read_once_and_present_on_success_and_failure(self):
        screen = self.screen()
        failed = screen.dock("bad", "invalid")
        succeeded = screen.dock("good", "ROOT")
        self.assertEqual(failed["runtime_versions"], {"vina": "1.2.7", "numpy": "2.4.0"})
        self.assertEqual(succeeded["runtime_versions"], failed["runtime_versions"])
        self.assertEqual(self._versions.call_count, 2)
        failed["runtime_versions"]["vina"] = "changed by caller"
        self.assertEqual(screen.dock("next", "ROOT")["runtime_versions"]["vina"], "1.2.7")

    def test_missing_runtime_package_metadata_is_a_constructor_error(self):
        self._versions.side_effect = PackageNotFoundError("vina")
        with self.assertRaises(PackageNotFoundError):
            self.screen()

    def test_local_main_continues_after_ligand_error_and_reports_failure_exit(self):
        job = DockJob("ATOM", CENTER, BOX_SIZE, [("bad", "invalid"), ("good", "ROOT")], 8, 1)
        with patch("engine.load_job", return_value=job), patch("sys.stdout", new_callable=io.StringIO) as output, patch("sys.stderr", new_callable=io.StringIO) as errors:
            self.assertEqual(main(), 1)
        self.assertIn("-7.250  good", output.getvalue())
        self.assertIn("ERROR bad: ValueError: cannot parse ligand", errors.getvalue())

    def test_local_main_reports_success_exit(self):
        job = DockJob("ATOM", CENTER, BOX_SIZE, [("good", "ROOT")], 8, 1)
        with patch("engine.load_job", return_value=job), patch("sys.stdout", new_callable=io.StringIO):
            self.assertEqual(main(), 0)

    def test_ligand_failure_returns_error_and_next_ligand_reuses_maps(self):
        screen = self.screen()
        failed = screen.dock("invalid-input", "invalid")
        succeeded = screen.dock("valid-input", "ROOT")
        self.assertEqual(failed["status"], "error")
        self.assertEqual(failed["error_type"], "ValueError")
        self.assertEqual(failed["error"], "cannot parse ligand")
        self.assertIsNone(failed["affinity"])
        self.assertIsNone(failed["pose_pdbqt"])
        self.assertEqual(succeeded["status"], "ok")
        self.assertEqual(failed["instance_id"], succeeded["instance_id"])
        self.assertEqual(succeeded["invocation"], 2)
        self.assertEqual(screen._vina.map_calls, 1)
        self.assertEqual(screen._vina.ligands, ["invalid", "ROOT"])
        self.assertEqual(set(failed), set(succeeded))

    def test_invalid_results_are_errors_without_stale_affinity_or_pose(self):
        screen = self.screen()
        self.assertEqual(screen.dock("baseline", "ROOT")["status"], "ok")
        for ligand in ["", "empty-pose", "empty-coordinates", "nan-affinity", "infinite-affinity"]:
            with self.subTest(ligand=ligand):
                result = screen.dock("bad", ligand)
                self.assertEqual(result["status"], "error")
                self.assertIsNone(result["affinity"])
                self.assertIsNone(result["pose_pdbqt"])
                self.assertTrue(result["error"])
                self.assertTrue(result["error_type"])
                self.assertGreaterEqual(result["dock_seconds"], 0.0)
        self.assertEqual(screen.dock("after-errors", "ROOT")["status"], "ok")
        self.assertEqual(screen._vina.map_calls, 1)

    def test_process_interrupts_are_not_ligand_error_rows(self):
        screen = self.screen()
        with self.assertRaises(KeyboardInterrupt):
            screen.dock("interrupted", "interrupt")

    def test_constructor_failures_propagate(self):
        with patch.object(FakeVina, "compute_vina_maps", side_effect=RuntimeError("map construction failed")):
            with self.assertRaisesRegex(RuntimeError, "map construction failed"):
                self.screen()


if __name__ == "__main__":
    unittest.main()

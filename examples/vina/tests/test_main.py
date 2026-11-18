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

"""Client lifecycle and artifact checks without Flame, Vina, or a cluster."""

import builtins
import contextlib
import csv
import importlib
import io
import json
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

DEFAULT_ENV = {"VINA_EXHAUSTIVENESS": "8", "VINA_SEED": "1", "VINA_WARMUP": "1"}


def import_without_runtime():
    original_import = builtins.__import__

    def checked_import(name, *args, **kwargs):
        if name.split(".")[0] in {"flamepy", "vina"}:
            raise AssertionError(f"unexpected runtime import: {name}")
        return original_import(name, *args, **kwargs)

    with patch.dict(sys.modules):
        sys.modules.pop("main", None)
        with patch("builtins.__import__", side_effect=checked_import):
            return importlib.import_module("main")


class MainTest(unittest.TestCase):
    def setUp(self):
        self.client = import_without_runtime()
        self.screen = Mock()
        self.screen._session.id = "review-session"
        self.definition = Mock()
        self.definition.remote.return_value = self.screen
        self.decorator = Mock(return_value=self.definition)
        self.app = Mock()
        self.app.service.return_value = self.decorator
        self.app.select.side_effect = lambda futures: iter(futures)
        self.future = Mock()
        self.future.get.return_value = {"name": "1iep_ligand", "status": "ok", "affinity": -7.25, "pose_pdbqt": "MODEL 1\nENDMDL\n"}
        self.screen.dock.return_value = self.future
        self.sdk = types.ModuleType("flamepy")
        self.sdk.app = self.app
        self.output = io.StringIO()
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.runs = 0
        self.enterContext(patch.dict(os.environ, DEFAULT_ENV))
        self.enterContext(patch("sys.version_info", (3, 12, 0)))
        self.enterContext(contextlib.redirect_stdout(self.output))

    def run_with_fake_sdk(self, *extra):
        self.runs += 1
        self.result_directory = self.directory / f"run-{self.runs}"
        with patch.dict(sys.modules, {"flamepy": self.sdk}):
            return self.client.main(["--output", str(self.result_directory), *extra])

    def assert_no_sdk_import(self, error):
        original_import = builtins.__import__

        def checked_import(name, *args, **kwargs):
            if name.split(".")[0] in {"flamepy", "vina"}:
                raise AssertionError(f"validation imported runtime: {name}")
            return original_import(name, *args, **kwargs)

        with patch("builtins.__import__", side_effect=checked_import):
            with self.assertRaises(error):
                self.client.main(["--output", str(self.directory / "unused")])
        self.app.init.assert_not_called()

    def test_import_does_not_initialize_or_import_flame(self):
        self.app.init.assert_not_called()
        self.app.service.assert_not_called()

    def test_wrong_python_version_fails_before_inputs_or_sdk(self):
        with patch("sys.version_info", (3, 13, 0)):
            with patch.object(self.client, "load_job") as load_job:
                self.assert_no_sdk_import(RuntimeError)
        load_job.assert_not_called()

    def test_missing_inputs_fail_before_sdk_import(self):
        with patch.object(self.client, "load_job", side_effect=FileNotFoundError("receptor missing")):
            self.assert_no_sdk_import(FileNotFoundError)

    def test_bad_configuration_fails_before_sdk_import(self):
        for variable, value in [("VINA_EXHAUSTIVENESS", "invalid"), ("VINA_EXHAUSTIVENESS", "0"), ("VINA_SEED", "invalid"), ("VINA_WARMUP", "invalid"), ("VINA_WARMUP", "-1")]:
            with self.subTest(variable=variable, value=value):
                with patch.dict(os.environ, {variable: value}):
                    self.assert_no_sdk_import(ValueError)

    def test_failures_before_submission_destroy_runtime(self):
        for stage in ["service", "decorator", "remote"]:
            with self.subTest(stage=stage):
                target = {"service": self.app.service, "decorator": self.decorator, "remote": self.definition.remote}[stage]
                target.side_effect = RuntimeError(f"{stage} failed")
                try:
                    with self.assertRaisesRegex(RuntimeError, f"{stage} failed"):
                        self.run_with_fake_sdk()
                    self.app.destroy.assert_called_once_with()
                finally:
                    target.side_effect = None
                    self.app.reset_mock()

    def test_success_saves_pose_and_affinity_then_cleans_up(self):
        self.assertEqual(self.run_with_fake_sdk(), 0)
        self.app.init.assert_called_once_with("vina-dock", python_version="3.12")
        self.app.service.assert_called_once_with(resreq="cpu=1,mem=2g", warmup=1)
        self.app.destroy.assert_called_once_with()
        self.assertIn("-7.250", self.output.getvalue())
        with (self.result_directory / "results.csv").open() as stream:
            rows = list(csv.DictReader(stream))
        self.assertEqual(rows[0]["name"], "1iep_ligand")
        self.assertEqual(rows[0]["rank"], "1")
        self.assertEqual((self.result_directory / rows[0]["pose_file"]).read_text(), "MODEL 1\nENDMDL\n")
        run = json.loads((self.result_directory / "run.json").read_text())
        self.assertEqual(run["session_id"], "review-session")
        self.assertEqual(run["status"], "completed")
        self.assertFalse(run["retained"])

    def test_keep_preserves_runtime(self):
        self.assertEqual(self.run_with_fake_sdk("--keep", "--app-name", "screen-review"), 0)
        self.app.init.assert_called_once_with("screen-review", python_version="3.12")
        self.app.destroy.assert_not_called()
        self.assertTrue(json.loads((self.result_directory / "run.json").read_text())["retained"])

    def test_remote_ligand_error_is_written(self):
        self.future.get.return_value = {"name": "1iep_ligand", "status": "error", "affinity": None, "pose_pdbqt": None, "error": "unsupported ligand", "error_type": "ValueError"}
        self.assertEqual(self.run_with_fake_sdk(), 1)
        with (self.result_directory / "failures.csv").open() as stream:
            row = next(csv.DictReader(stream))
        self.assertEqual(row["status"], "error")
        self.assertEqual(row["error"], "unsupported ligand")
        self.app.destroy.assert_called_once_with()

    def test_client_error_preserves_runtime_and_records_uncertain_state(self):
        self.future.get.side_effect = RuntimeError("watch reset")
        self.assertEqual(self.run_with_fake_sdk(), 1)
        self.app.destroy.assert_not_called()
        with (self.result_directory / "failures.csv").open() as stream:
            row = next(csv.DictReader(stream))
        self.assertEqual(row["status"], "unknown")
        self.assertIn("watch reset", row["error"])

    def test_partial_submission_preserves_runtime(self):
        self.screen.dock.side_effect = RuntimeError("submission response lost")
        with self.assertRaisesRegex(RuntimeError, "submission response lost"):
            self.run_with_fake_sdk()
        self.app.destroy.assert_not_called()
        self.assertTrue(json.loads((self.result_directory / "run.json").read_text())["retained"])

    def test_transient_artifact_failure_is_not_a_second_ligand_outcome(self):
        original = self.client.ScreenReport.flush
        calls = 0

        def flush(report):
            nonlocal calls
            calls += 1
            if calls == 3:
                raise OSError("transient artifact write failure")
            return original(report)

        with patch.object(self.client.ScreenReport, "flush", flush):
            with self.assertRaisesRegex(OSError, "artifact write failure"):
                self.run_with_fake_sdk()
        run = json.loads((self.result_directory / "run.json").read_text())
        self.assertEqual(run["status"], "interrupted")
        self.assertEqual(run["summary"]["completed"], 1)
        self.assertEqual(run["summary"]["unknown"], 0)
        self.app.destroy.assert_not_called()

    def test_output_directory_is_never_overwritten(self):
        existing = self.directory / "existing"
        existing.mkdir()
        with patch.dict(sys.modules, {"flamepy": self.sdk}):
            with self.assertRaises(FileExistsError):
                self.client.main(["--output", str(existing)])
        self.app.init.assert_not_called()

    def test_manifest_checksum_is_verified_before_initialization(self):
        ligands = self.directory / "library" / "ligands"
        ligands.mkdir(parents=True)
        (ligands / "a.pdbqt").write_text("ROOT\n")
        (ligands.parent / "manifest.json").write_text(json.dumps({"ligands": [{"name": "a", "pdbqt": "ligands/a.pdbqt", "sha256": "wrong"}]}))
        with patch.dict(sys.modules, {"flamepy": self.sdk}):
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                self.client.main(["--ligands", str(ligands), "--output", str(self.directory / "unused")])
        self.app.init.assert_not_called()


if __name__ == "__main__":
    unittest.main()

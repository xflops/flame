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

"""Preparation invariants tested without RDKit, Meeko, or network access."""

import hashlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import prepare


class FakeChemistry:
    versions = dict(prepare.PINS)

    def reference_identity(self):
        return {"identity": "REFERENCE", "identity_smiles": "reference", "canonical_smiles": "reference"}

    def describe(self, molecule):
        if molecule is None:
            raise ValueError("unreadable SDF")
        return {"identity": molecule["identity"], "canonical_smiles": molecule.get("smiles", molecule["identity"])}

    def pdbqt(self, molecule):
        if molecule.get("fail"):
            raise ValueError("cannot parameterize")
        return "ROOT\nATOM      1  C\nENDROOT\nTORSDOF 0\n"

    def records(self, archive):
        role = Path(archive).name
        yield 1, "imatinib-alias", {"identity": "REFERENCE"}
        for index in range(prepare.QUOTAS[role]):
            yield index + 2, f"{role}-{index}", {"identity": f"{role}-{index}"}


class PrepareTest(unittest.TestCase):
    def test_cached_archive_is_verified_before_use(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "source.gz"
            path.write_bytes(b"source")
            source = {"url": "https://example.org/source.gz", "sha256": hashlib.sha256(b"source").hexdigest()}
            with patch("prepare.urllib.request.urlopen") as download:
                self.assertEqual(prepare.verified_download(source, directory), path)
                path.write_bytes(b"changed")
                with self.assertRaisesRegex(ValueError, "cached source checksum mismatch"):
                    prepare.verified_download(source, directory)
            download.assert_not_called()

    def test_changed_remote_archive_is_not_published(self):
        source = {"url": "https://example.org/source.gz", "sha256": "0" * 64}
        with tempfile.TemporaryDirectory() as directory:
            with patch("prepare.urllib.request.urlopen", return_value=io.BytesIO(b"changed")):
                with self.assertRaisesRegex(ValueError, "download checksum mismatch"):
                    prepare.verified_download(source, directory)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_deduplication_is_by_identity_and_failures_are_backfilled(self):
        records = [
            (1, "reference-alias", {"identity": "REFERENCE"}),
            (2, "bad-state", {"identity": "A", "fail": True}),
            (3, "first-valid-state", {"identity": "A"}),
            (4, "different-name-same-molecule", {"identity": "A", "smiles": "other-protonation"}),
            (5, "first-valid-state", {"identity": "B"}),
        ]
        skips = []
        with tempfile.TemporaryDirectory() as directory:
            selected = prepare.select_ligands(records, "active", 2, FakeChemistry(), {"REFERENCE": "reference"}, directory, skips)
            self.assertEqual([item["identity"] for item in selected], ["A", "B"])
            self.assertEqual(len({item["name"] for item in selected}), 2)
            self.assertEqual(len(list(Path(directory).glob("*.pdbqt"))), 2)
        self.assertEqual(len(skips), 3)
        self.assertEqual(skips[0]["duplicate_of"], "reference")
        self.assertIn("cannot parameterize", skips[1]["reason"])
        self.assertEqual(skips[2]["duplicate_of"], selected[0]["name"])

    def test_insufficient_distinct_molecules_fail_instead_of_duplicating(self):
        skips = []
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "only 1 of 2"):
                prepare.select_ligands([(1, "same", {"identity": "A"}), (2, "alias", {"identity": "A"}), (3, "broken", None)], "active", 2, FakeChemistry(), {}, directory, skips)
        self.assertEqual(len(skips), 2)

    def test_success_has_exact_quotas_reference_and_matching_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "screening"
            with patch("prepare.Chemistry", FakeChemistry), patch("prepare.verified_download", side_effect=lambda source, cache: Path(next(role for role, value in prepare.SOURCES.items() if value == source))):
                manifest = prepare.prepare_library(output)
            self.assertEqual(json.loads((output / "manifest.json").read_text()), manifest)
            entries = manifest["ligands"]
            self.assertEqual(len(entries), 101)
            for role, count in [("active", 20), ("decoy", 80), ("reference", 1)]:
                self.assertEqual(sum(item["role"] == role for item in entries), count)
            self.assertEqual(len({item["identity"] for item in entries}), 101)
            for item in entries:
                self.assertEqual(prepare.sha256(output / item["pdbqt"]), item["sha256"])
            reference = next(item for item in entries if item["role"] == "reference")
            self.assertEqual((output / reference["pdbqt"]).read_bytes(), (Path(prepare.__file__).parent / "data/ligands/1iep_ligand.pdbqt").read_bytes())
            with self.assertRaises(FileExistsError):
                prepare.prepare_library(output)

    def test_failure_records_errors_without_publishing_partial_library(self):
        class BrokenChemistry(FakeChemistry):
            def records(self, archive):
                yield 1, "broken", None

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "screening"
            with patch("prepare.Chemistry", BrokenChemistry), patch("prepare.verified_download", return_value=Path("unused")):
                with self.assertRaisesRegex(ValueError, "only 0 of 20"):
                    prepare.prepare_library(output)
            errors = json.loads((output / "preparation_errors.json").read_text())
            self.assertIn("unreadable SDF", errors["skips"][0]["reason"])
            self.assertFalse((output / "manifest.json").exists())
            self.assertFalse((output / "ligands").exists())

    def test_incompatible_tools_fail_before_native_imports(self):
        with patch("prepare.importlib.metadata.version", return_value="wrong"):
            with self.assertRaisesRegex(ValueError, "pinned tool versions"):
                prepare.Chemistry()


@unittest.skipUnless(importlib.util.find_spec("rdkit") and importlib.util.find_spec("meeko"), "install the pinned prepare extra for native chemistry checks")
class NativeChemistryTest(unittest.TestCase):
    def setUp(self):
        self.tools = prepare.Chemistry()

    def test_missing_hydrogens_and_2d_are_not_silently_repaired(self):
        from rdkit import Chem
        from rdkit.Chem import AllChem

        molecule = Chem.MolFromSmiles("CCO")
        AllChem.Compute2DCoords(molecule)
        with self.assertRaisesRegex(ValueError, "3D conformer"):
            self.tools.describe(molecule)
        molecule.GetConformer().Set3D(True)
        with self.assertRaisesRegex(ValueError, "explicit hydrogen"):
            self.tools.describe(molecule)

    def test_protonation_dedup_does_not_modify_supplied_docking_state(self):
        from rdkit import Chem
        from rdkit.Chem import AllChem

        neutral = Chem.MolFromSmiles("CCN")
        supplied = Chem.AddHs(Chem.MolFromSmiles("CC[NH3+]"))
        AllChem.EmbedMolecule(supplied, randomSeed=7)
        before = supplied.GetConformer().GetPositions().tolist()
        metadata = self.tools.describe(supplied)
        self.assertEqual(metadata["identity"], self.tools.identity(neutral)[0])
        self.assertEqual(metadata["formal_charge"], 1)
        self.assertIn("ROOT", self.tools.pdbqt(supplied))
        self.assertEqual(supplied.GetConformer().GetPositions().tolist(), before)
        self.assertEqual(Chem.GetFormalCharge(supplied), 1)


if __name__ == "__main__":
    unittest.main()

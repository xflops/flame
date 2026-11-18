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

"""Prepare a reproducible DUD-E ABL1 library without requiring Flame or Vina.

Install the ``prepare`` extra and run with Python 3.12. Source archives and
prepared inputs are generated locally, not distributed as repository assets.
"""

import argparse
import gzip
import hashlib
import importlib.metadata
import json
import math
import re
import shutil
import tempfile
import urllib.request
from pathlib import Path

SOURCES = {
    "active": {
        "url": "https://dude.docking.org/targets/abl1/actives_final.sdf.gz",
        "sha256": "151e099a1939348a427f6d263386d852f1a30e01040eed7d75dc28feaf9c9f2b",
    },
    "decoy": {
        "url": "https://dude.docking.org/targets/abl1/decoys_final.sdf.gz",
        "sha256": "be48fc1fbff68fc4fbe1a584aa6c5c8c629a879d3f2603f40f9d045502600071",
    },
}
PINS = {
    "rdkit": "2025.9.3",
    "meeko": "0.6.1",
    "numpy": "2.2.6",
    "scipy": "1.15.3",
    "gemmi": "0.7.3",
}
QUOTAS = {"active": 20, "decoy": 80}
# PubChem CID 5291 / ChEMBL941; used only to identify and exclude imatinib
# from the 100-compound library. The reference PDBQT itself is copied unchanged.
REFERENCE_SMILES = "CC1=C(C=C(C=C1)NC(=O)C2=CC=C(C=C2)CN3CCN(CC3)C)NC4=NC=CC(=N4)C5=CN=CC=C5"
REFERENCE_NAME = "reference_1iep_ligand"
MAX_ARCHIVE_BYTES = 32 * 1024 * 1024


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verified_download(source, cache):
    """Use only the pinned bytes, including when a previous download is cached."""
    cache = Path(cache)
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / source["url"].rsplit("/", 1)[-1]
    if destination.exists():
        if sha256(destination) != source["sha256"]:
            raise ValueError(f"cached source checksum mismatch: {destination}")
        return destination
    temporary = None
    try:
        request = urllib.request.Request(source["url"], headers={"User-Agent": "Flame-vina-example/1"})
        with urllib.request.urlopen(request, timeout=60) as response:
            with tempfile.NamedTemporaryFile(dir=cache, delete=False) as target:
                temporary = Path(target.name)
                size = 0
                while chunk := response.read(1024 * 1024):
                    size += len(chunk)
                    if size > MAX_ARCHIVE_BYTES:
                        raise ValueError("source archive exceeds the download size limit")
                    target.write(chunk)
        if sha256(temporary) != source["sha256"]:
            raise ValueError(f"download checksum mismatch: {source['url']}")
        temporary.replace(destination)
        return destination
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


class Chemistry:
    """Keep optional native dependencies out of ordinary example imports/tests."""

    def __init__(self):
        self.versions = {name: importlib.metadata.version(name) for name in PINS}
        if self.versions != PINS:
            raise ValueError(f"preparation requires pinned tool versions {PINS}; found {self.versions}")
        from meeko import MoleculePreparation, PDBQTWriterLegacy
        from rdkit import Chem
        from rdkit.Chem import rdMolDescriptors
        from rdkit.Chem.MolStandardize import rdMolStandardize

        self.chem = Chem
        self.standardize = rdMolStandardize
        self.formula = rdMolDescriptors.CalcMolFormula
        self.preparer = MoleculePreparation()
        self.writer = PDBQTWriterLegacy

    def records(self, archive):
        with gzip.open(archive, "rb") as source:
            supplier = self.chem.ForwardSDMolSupplier(source, removeHs=False)
            for index, molecule in enumerate(supplier, 1):
                name = molecule.GetProp("_Name") if molecule is not None and molecule.HasProp("_Name") else f"record-{index}"
                yield index, name, molecule

    def identity(self, molecule):
        """Normalize a COPY for deduplication; never change the docking molecule."""
        parent = self.standardize.FragmentParent(self.chem.RemoveHs(molecule))
        parent = self.standardize.Uncharger().uncharge(parent)
        parent = self.standardize.TautomerEnumerator().Canonicalize(parent)
        key = self.chem.MolToInchiKey(parent)
        if not key:
            raise ValueError("cannot assign a standard InChIKey")
        return key.split("-", 1)[0], self.chem.MolToSmiles(parent, canonical=True, isomericSmiles=True)

    def describe(self, molecule):
        if molecule is None:
            raise ValueError("RDKit could not read the source SDF record")
        if len(self.chem.GetMolFrags(molecule)) != 1:
            raise ValueError("multiple disconnected fragments")
        if not molecule.GetNumConformers() or not molecule.GetConformer().Is3D():
            raise ValueError("source does not contain a 3D conformer")
        if any(not math.isfinite(value) for position in molecule.GetConformer().GetPositions() for value in position):
            raise ValueError("non-finite source coordinates")
        # GetTotalNumHs() excludes bonded H atoms by default. A nonzero count
        # here means some hydrogens exist only as an implicit/atom-count field.
        if any(atom.GetTotalNumHs() for atom in molecule.GetAtoms()):
            raise ValueError("source lacks explicit hydrogen atoms; protonation was not invented")
        identity, parent_smiles = self.identity(molecule)
        return {
            "identity": identity,
            "identity_smiles": parent_smiles,
            "canonical_smiles": self.chem.MolToSmiles(self.chem.RemoveHs(molecule), canonical=True, isomericSmiles=True),
            "formal_charge": self.chem.GetFormalCharge(molecule),
            "formula": self.formula(molecule),
        }

    def pdbqt(self, molecule):
        setups = self.preparer.prepare(molecule)
        if len(setups) != 1:
            raise ValueError(f"expected one Meeko setup, got {len(setups)}")
        text, success, error = self.writer.write_string(setups[0])
        if not success:
            raise ValueError(f"Meeko: {error}")
        if "ROOT\n" not in text or not any(line.startswith("ATOM") for line in text.splitlines()):
            raise ValueError("Meeko produced no docking atoms/root")
        return text

    def reference_identity(self):
        molecule = self.chem.MolFromSmiles(REFERENCE_SMILES)
        identity, parent_smiles = self.identity(molecule)
        return {"identity": identity, "identity_smiles": parent_smiles, "canonical_smiles": self.chem.MolToSmiles(molecule)}


def select_ligands(records, role, count, chemistry, seen, destination, skips):
    """Select successful unique identities in pinned source order, backfilling failures."""
    selected = []
    for index, source_id, molecule in records:
        if len(selected) == count:
            break
        item = {"role": role, "source_id": source_id, "source_record_index": index}
        try:
            item.update(chemistry.describe(molecule))
            if item["identity"] in seen:
                skips.append({**item, "reason": "duplicate chemical identity", "duplicate_of": seen[item["identity"]]})
                continue
            text = chemistry.pdbqt(molecule)
            safe_id = re.sub(r"[^A-Za-z0-9_.-]", "_", source_id)[:80]
            name = f"{role}_{safe_id}_{item['identity']}"
            path = Path(destination) / f"{name}.pdbqt"
            path.write_text(text, encoding="utf-8")
            item.update(name=name, source="DUD-E ABL1", pdbqt=f"ligands/{path.name}", sha256=sha256(path))
            selected.append(item)
            seen[item["identity"]] = name
        except Exception as error:
            skips.append({**item, "reason": f"{type(error).__name__}: {error}"})
    if len(selected) != count:
        raise ValueError(f"prepared only {len(selected)} of {count} distinct {role} ligands")
    return selected


def prepare_library(output):
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    if (output / "ligands").exists() or (output / "manifest.json").exists():
        raise FileExistsError(f"use a fresh --output directory; prepared inputs already exist in {output}")
    chemistry = Chemistry()
    archives = {role: verified_download(source, output / "sources") for role, source in SOURCES.items()}
    skips = []
    try:
        with tempfile.TemporaryDirectory(prefix=".preparing-", dir=output) as temporary:
            staged = Path(temporary) / "ligands"
            staged.mkdir()
            reference = chemistry.reference_identity()
            seen = {reference["identity"]: REFERENCE_NAME}
            ligands = []
            for role, count in QUOTAS.items():
                ligands.extend(select_ligands(chemistry.records(archives[role]), role, count, chemistry, seen, staged, skips))
            source_reference = Path(__file__).resolve().parent / "data" / "ligands" / "1iep_ligand.pdbqt"
            reference_path = staged / f"{REFERENCE_NAME}.pdbqt"
            shutil.copyfile(source_reference, reference_path)
            ligands.append({**reference, "name": REFERENCE_NAME, "role": "reference", "source": "AutoDock Vina tutorial 1IEP", "source_id": "1iep_ligand", "pdbqt": f"ligands/{reference_path.name}", "sha256": sha256(reference_path)})
            manifest = {
                "schema_version": 1,
                "selection": {"actives": QUOTAS["active"], "decoys": QUOTAS["decoy"], "reference": 1, "library_count": 100, "identity_method": "standard InChIKey connectivity block of neutral canonical-tautomer parent; ignores stereochemistry and protonation"},
                "sources": [{"role": role, **source, "archive": f"sources/{archives[role].name}"} for role, source in SOURCES.items()],
                "preparation": {
                    "tool_versions": chemistry.versions,
                    "coordinates": "supplied DUD-E 3D conformer, unchanged",
                    "protonation": "supplied explicit hydrogen state, unchanged; no hydrogen generation or pH enumeration",
                    "charges": "Meeko default Gasteiger",
                    "selection_order": "source record order; first successfully prepared unique identity",
                    "reference_identity_source": "PubChem CID 5291 / ChEMBL941, imatinib",
                },
                "ligands": ligands,
                "skips": skips,
            }
            staged.rename(output / "ligands")
            (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        shutil.copyfile(Path(__file__).resolve().parent / "data" / "NOTICE", output / "NOTICE")
    except Exception as error:
        (output / "preparation_errors.json").write_text(json.dumps({"error": str(error), "skips": skips}, indent=2, sort_keys=True) + "\n")
        raise
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parent / "data" / "screening")
    args = parser.parse_args()
    manifest = prepare_library(args.output)
    print(f"Prepared 100 distinct library ligands (20 active, 80 decoy) + 1 reference in {args.output}")
    print(f"Recorded {len(manifest['skips'])} duplicate/preparation skips in manifest.json")


if __name__ == "__main__":
    main()

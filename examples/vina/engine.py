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

"""AutoDock Vina docking used locally and as a Flame service.

The receptor and its affinity maps stay on the executor. Each call docks one ligand.
"""

import math
import os
import socket
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from importlib.metadata import version
from pathlib import Path

# Search box from the AutoDock Vina tutorial for PDB 1IEP (c-Abl / imatinib).
CENTER = (15.190, 53.903, 16.917)
BOX_SIZE = (20.0, 20.0, 20.0)


@dataclass(frozen=True)
class DockJob:
    receptor: str
    center: tuple[float, float, float]
    box_size: tuple[float, float, float]
    ligands: list[tuple[str, str]]
    exhaustiveness: int
    seed: int


class VinaScreen:
    """One Vina engine. Maps are built once; dock() replaces only the ligand."""

    def __init__(
        self,
        receptor_pdbqt: str,
        center: tuple[float, float, float],
        box_size: tuple[float, float, float],
        exhaustiveness: int = 8,
        n_poses: int = 1,
        seed: int = 1,
        cpu: int = 1,
    ):
        if not receptor_pdbqt.strip():
            raise ValueError("receptor PDBQT is empty")
        if exhaustiveness <= 0:
            raise ValueError("exhaustiveness must be greater than 0")
        if n_poses <= 0:
            raise ValueError("n_poses must be greater than 0")
        if cpu <= 0:
            raise ValueError("cpu must be greater than 0")

        started = time.perf_counter()
        from vina import Vina

        self._runtime_versions = {package: version(package) for package in ("vina", "numpy")}
        self._hostname = socket.gethostname()
        self._pid = os.getpid()
        self._instance_id = str(uuid.uuid4())
        self._invocation = 0
        self._tmpdir = tempfile.TemporaryDirectory()
        try:
            path = os.path.join(self._tmpdir.name, "receptor.pdbqt")
            with open(path, "w") as handle:
                handle.write(receptor_pdbqt)
            self._receptor_path = path
            self._exhaustiveness = exhaustiveness
            self._n_poses = n_poses
            self._vina = Vina(sf_name="vina", cpu=cpu, seed=seed, verbosity=0)
            self._vina.set_receptor(path)
            # No ligand yet, so the maps cover every Vina atom type and later ligands reuse them.
            map_started = time.perf_counter()
            self._vina.compute_vina_maps(center=list(center), box_size=list(box_size))
            self._map_seconds = time.perf_counter() - map_started
            # Vina records the actual generated seed here when its input seed is zero.
            self._seed = int(self._vina.info()["seed"])
            self._setup_seconds = time.perf_counter() - started
        except BaseException:
            self._tmpdir.cleanup()
            raise

    def dock(self, name: str, ligand_pdbqt: str) -> dict:
        """Return the best pose and score, or an error row, without rebuilding maps.

        setup_seconds includes map_seconds and is repeated on every result for
        this instance; aggregate it once per instance_id. dock_seconds measures
        this invocation only, including ligand loading and result extraction.
        Constructor failures propagate; ordinary ligand failures become rows.
        """
        started = time.perf_counter()
        self._invocation += 1
        result = {
            "name": name,
            "status": "error",
            "affinity": None,
            "pose_pdbqt": None,
            "error": None,
            "error_type": None,
            "dock_seconds": 0.0,
            "setup_seconds": self._setup_seconds,
            "map_seconds": self._map_seconds,
            "hostname": self._hostname,
            "pid": self._pid,
            "instance_id": self._instance_id,
            "invocation": self._invocation,
            "seed": self._seed,
            "runtime_versions": dict(self._runtime_versions),
        }
        try:
            if not ligand_pdbqt.strip():
                raise ValueError(f"ligand PDBQT for {name} is empty")
            self._vina.set_ligand_from_string(ligand_pdbqt)
            self._vina.dock(exhaustiveness=self._exhaustiveness, n_poses=self._n_poses)
            energies = self._vina.energies(n_poses=1)
            if len(energies) == 0:
                raise RuntimeError(f"Vina returned no pose for {name}")
            affinity = float(energies[0][0])
            if not math.isfinite(affinity):
                raise RuntimeError(f"Vina returned a non-finite affinity for {name}")
            pose = self._vina.poses(n_poses=1)
            if not isinstance(pose, str) or not pose.strip():
                raise RuntimeError(f"Vina returned an empty best pose for {name}")
            result.update(status="ok", affinity=affinity, pose_pdbqt=pose)
        except Exception as error:
            result.update(error=str(error), error_type=type(error).__name__)
        result["dock_seconds"] = time.perf_counter() - started
        return result


def load_job(root: Path | None = None, ligand_dir: Path | None = None) -> DockJob:
    """Read the bundled receptor and every ligand PDBQT under data/ligands."""
    root = Path(__file__).resolve().parent if root is None else Path(root)
    receptor_path = root / "data" / "1iep_receptor.pdbqt"
    ligand_dir = root / "data" / "ligands" if ligand_dir is None else Path(ligand_dir)
    receptor = receptor_path.read_text()
    if not receptor.strip():
        raise ValueError("receptor PDBQT is empty")
    ligands = [(path.stem, path.read_text()) for path in sorted(ligand_dir.glob("*.pdbqt"))]
    if not ligands:
        raise FileNotFoundError(f"no PDBQT ligands in {ligand_dir}")
    for name, pdbqt in ligands:
        if not pdbqt.strip():
            raise ValueError(f"ligand PDBQT for {name} is empty")
    exhaustiveness = int(os.environ.get("VINA_EXHAUSTIVENESS", "8"))
    seed = int(os.environ.get("VINA_SEED", "1"))
    if exhaustiveness <= 0:
        raise ValueError("VINA_EXHAUSTIVENESS must be greater than 0")
    return DockJob(receptor, CENTER, BOX_SIZE, ligands, exhaustiveness, seed)


def main():
    """Dock the bundled ligands in this process. Requires the vina package."""
    job = load_job()
    screen = VinaScreen(
        job.receptor,
        job.center,
        job.box_size,
        exhaustiveness=job.exhaustiveness,
        seed=job.seed,
    )
    failed = False
    try:
        for name, pdbqt in job.ligands:
            result = screen.dock(name, pdbqt)
            if result["status"] == "ok":
                print(f"{result['affinity']:8.3f}  {result['name']}")
            else:
                failed = True
                print(f"ERROR {result['name']}: {result['error_type']}: {result['error']}", file=sys.stderr)
    finally:
        screen._tmpdir.cleanup()
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())

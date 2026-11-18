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

"""Screen a ligand library on Flame and save ranked scores and best poses."""

import argparse
import contextlib
import hashlib
import json
import os
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

from engine import VinaScreen, load_job
from report import ScreenReport

ROOT = Path(__file__).resolve().parent


def input_metadata(directory: Path, ligands: list[tuple[str, str]]) -> tuple[dict, dict | None]:
    """Verify a prepared library against its manifest before contacting Flame."""
    path = directory.parent / "manifest.json"
    if not path.exists():
        return {name: {"role": "reference" if name == "1iep_ligand" else "library", "source_id": name} for name, _ in ligands}, None
    manifest = json.loads(path.read_text())
    entries = manifest["ligands"]
    by_name = {entry["name"]: entry for entry in entries}
    if len(by_name) != len(entries) or set(by_name) != {name for name, _ in ligands}:
        raise ValueError("ligand files do not match the dataset manifest")
    for name, _ in ligands:
        ligand_path = directory / f"{name}.pdbqt"
        entry = by_name[name]
        if (path.parent / entry["pdbqt"]).resolve() != ligand_path.resolve():
            raise ValueError(f"manifest path does not match {name}")
        if hashlib.sha256(ligand_path.read_bytes()).hexdigest() != entry["sha256"]:
            raise ValueError(f"ligand checksum mismatch: {name}")
    return {name: {key: entry.get(key) for key in ("role", "source_id", "canonical_smiles", "identity")} for name, entry in by_name.items()}, manifest


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ligands", type=Path, default=ROOT / "data" / "ligands", help="Directory of prepared PDBQT ligands")
    parser.add_argument("--output", type=Path, default=ROOT / "results" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ"), help="New output directory; existing directories are never overwritten")
    parser.add_argument("--app-name", default="vina-dock", help="Use a new name when deploying changed dependencies")
    parser.add_argument("--keep", action="store_true", help="Keep the application and session open after screening")
    return parser.parse_args(argv)


def main(argv=None):
    if sys.version_info[:2] != (3, 12):
        raise RuntimeError("This example requires Python 3.12 on both the client and executors; run python3.12 main.py.")
    args = parse_args(argv)
    ligand_dir = args.ligands.resolve()
    job = load_job(ligand_dir=ligand_dir)
    metadata, manifest = input_metadata(ligand_dir, job.ligands)
    warmup = int(os.environ.get("VINA_WARMUP", "1"))
    if warmup < 0:
        raise ValueError("VINA_WARMUP must be a non-negative integer")
    if not args.app_name.strip():
        raise ValueError("application name must not be empty")
    run = {
        "started_at": datetime.now(timezone.utc).isoformat(),
        "application": args.app_name,
        "status": "prepared",
        "ligand_count": len(job.ligands),
        "input_directory": str(ligand_dir),
        "dataset": manifest,
        "inputs": [{"name": name, "sha256": hashlib.sha256((ligand_dir / f"{name}.pdbqt").read_bytes()).hexdigest()} for name, _ in job.ligands],
        "receptor_sha256": hashlib.sha256(job.receptor.encode()).hexdigest(),
        "center": job.center,
        "box_size": job.box_size,
        "exhaustiveness": job.exhaustiveness,
        "seed": job.seed,
        "warmup": warmup,
        "resources": "cpu=1,mem=2g",
        "batch_size": 1,
        "client_python": sys.version.split()[0],
    }
    report = ScreenReport(args.output.resolve(), run, metadata)

    from flamepy import app

    print("AutoDock Vina on Flame (CPU)", flush=True)
    print(f"Ligands: {len(job.ligands)}, exhaustiveness: {job.exhaustiveness}, seed: {job.seed}", flush=True)
    print(f"Output: {report.output}", flush=True)
    initialized = False
    submitted = False
    retained = args.keep
    started = time.perf_counter()
    try:
        # Package this example even when launched from another directory.
        with contextlib.chdir(ROOT):
            app.init(args.app_name, python_version="3.12")
        initialized = True
        run["initialization_seconds"] = time.perf_counter() - started
        screen_service = app.service(resreq="cpu=1,mem=2g", warmup=warmup)(VinaScreen)
        screen = screen_service.remote(job.receptor, job.center, job.box_size, exhaustiveness=job.exhaustiveness, n_poses=1, seed=job.seed, cpu=1)
        # The proxy currently exposes session identity through its core session.
        run["session_id"] = screen._session.id
        run["service_creation_seconds"] = time.perf_counter() - started - run["initialization_seconds"]
        print(f"Session: {run['session_id']}", flush=True)
        futures = {}
        docking_started = time.perf_counter()
        for name, pdbqt in job.ligands:
            submitted = True
            submitted_at = time.perf_counter()
            futures[screen.dock(name, pdbqt)] = (name, submitted_at)
        run["status"] = "running"
        report.flush()
        for future in app.select(list(futures)):
            name, submitted_at = futures[future]
            try:
                result = future.get()
            except Exception as exc:
                # A transport failure does not establish the remote task's state.
                retained = True
                result = {"name": name, "status": "unknown", "error_type": type(exc).__name__, "error": str(exc)}
            wall_seconds = time.perf_counter() - submitted_at
            row = report.add(result, name, wall_seconds)
            if "time_to_first_result_seconds" not in run:
                run["time_to_first_result_seconds"] = time.perf_counter() - docking_started
            progress = f"{len(report.rows)}/{len(job.ligands)}"
            detail = f"{row['affinity']:.3f} kcal/mol" if row["status"] == "ok" else row.get("error")
            print(f"[{progress}] {name}: {row['status']} {detail}", flush=True)
        run["screen_wall_seconds"] = time.perf_counter() - docking_started
        run["status"] = "completed" if all(row["status"] == "ok" for row in report.rows) else "completed_with_errors"
        successful = sorted((row for row in report.rows if row["status"] == "ok"), key=lambda row: (row["affinity"], row["name"]))
        print("\nTop affinities (kcal/mol):", flush=True)
        for row in successful[:10]:
            print(f"  {row['affinity']:8.3f}  {row['name']} ({row.get('role', 'library')})", flush=True)
        print(f"Results: {report.output / 'results.csv'}", flush=True)
        return 0 if run["status"] == "completed" else 1
    except BaseException as exc:
        run["status"] = "interrupted"
        run["error"] = f"{type(exc).__name__}: {exc}"
        retained = retained or submitted
        raise
    finally:
        run["retained"] = retained
        try:
            if initialized and not retained:
                app.destroy()
        except BaseException:
            run["retained"] = True
            run["status"] = "cleanup_failed"
            raise
        finally:
            run["total_wall_seconds"] = time.perf_counter() - started
            run["finished_at"] = datetime.now(timezone.utc).isoformat()
            report.flush()
            if initialized and run["retained"]:
                print(f"Application and session retained: {args.app_name}, {run.get('session_id', 'see cluster status')}", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())

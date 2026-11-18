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

"""Incremental screening artifacts, independent of Flame and native Vina."""

import csv
import json
import math
from pathlib import Path

COLUMNS = ["rank", "name", "role", "source_id", "canonical_smiles", "identity", "status", "affinity", "pose_file", "dock_seconds", "wall_seconds", "setup_seconds", "map_seconds", "hostname", "pid", "instance_id", "invocation", "seed", "error_type", "error"]


class ScreenReport:
    def __init__(self, output: Path, run: dict, metadata: dict):
        self.output = Path(output)
        self.output.mkdir(parents=True, exist_ok=False)
        (self.output / "poses").mkdir()
        self.run = run
        self.metadata = metadata
        self.rows = []
        self.flush()

    def add(self, result: dict, expected_name: str, wall_seconds: float) -> dict:
        if result.get("name") != expected_name or result.get("status") not in {"ok", "error", "unknown"}:
            raise ValueError(f"invalid result for {expected_name}")
        row = {**self.metadata.get(expected_name, {}), **result, "wall_seconds": wall_seconds}
        pose = row.pop("pose_pdbqt", None)
        if row["status"] == "ok":
            if not isinstance(row.get("affinity"), (int, float)) or not math.isfinite(row["affinity"]):
                raise ValueError(f"invalid affinity for {expected_name}")
            if not isinstance(pose, str) or not pose.strip():
                raise ValueError(f"missing docked pose for {expected_name}")
            # expected_name comes from the validated input file's stem.
            if Path(expected_name).name != expected_name:
                raise ValueError("ligand name must be a file name")
            row["pose_file"] = f"poses/{expected_name}.pdbqt"
            (self.output / row["pose_file"]).write_text(pose)
        self.rows.append(row)
        self.flush()
        return row

    def flush(self):
        successful = sorted((row for row in self.rows if row["status"] == "ok"), key=lambda row: (row["affinity"], row["name"]))
        for rank, row in enumerate(successful, 1):
            row["rank"] = rank
        failures = [row for row in self.rows if row["status"] != "ok"]
        for name, rows in [("results.csv", successful), ("failures.csv", failures)]:
            path = self.output / name
            temporary = path.with_suffix(".csv.tmp")
            with temporary.open("w", newline="") as stream:
                writer = csv.DictWriter(stream, fieldnames=COLUMNS, extrasaction="ignore")
                writer.writeheader()
                writer.writerows(rows)
            temporary.replace(path)
        instances = {row["instance_id"]: {key: row.get(key) for key in ("hostname", "pid", "setup_seconds", "map_seconds", "runtime_versions")} for row in self.rows if row.get("instance_id")}
        self.run["summary"] = {
            "completed": len(self.rows),
            "succeeded": len(successful),
            "errors": sum(row["status"] == "error" for row in failures),
            "unknown": sum(row["status"] == "unknown" for row in failures),
            "instances_observed": len(instances),
            "sum_setup_seconds": sum(row.get("setup_seconds") or 0 for row in instances.values()),
            "sum_map_seconds": sum(row.get("map_seconds") or 0 for row in instances.values()),
            "sum_dock_seconds": sum(row.get("dock_seconds") or 0 for row in self.rows),
            "warm_invocations": sum((row.get("invocation") or 0) > 1 for row in self.rows),
        }
        self.run["instances"] = instances
        temporary = self.output / "run.json.tmp"
        temporary.write_text(json.dumps(self.run, indent=2) + "\n")
        temporary.replace(self.output / "run.json")

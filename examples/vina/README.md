# Virtual screening with AutoDock Vina on Flame

Screen distinct molecules against one receptor, with one ligand per Flame task.
Each executor retains its Vina engine and affinity maps across tasks. The client
saves ranked scores, best poses, failures, and timings as results arrive.

The receptor is the AutoDock Vina tutorial's c-Abl / imatinib system, PDB 1IEP.
The preparation script builds a documented subset of the DUD-E ABL1 library:
**20 reported actives, 80 decoys, and the bundled imatinib reference**. These are
101 distinct parent identities; repeating protonation or stereochemical variants
does not inflate the library count. See `data/NOTICE` for sources and terms.

This is a small screening workflow, not a reproduction of the DUD-E benchmark.
DUD-E used a different receptor structure, decoys are not experimentally confirmed
non-binders, and a docking score is not a measured binding affinity. The reference
provides a comparison score and pose; the script does not calculate pose RMSD or
claim enrichment or predictive accuracy.

## Requirements

- Python 3.12 on the client and executors, with the Flame Python SDK on the client.
- Linux x86_64 executors that can install `vina==1.2.7`.
- CPU resources for `cpu=1,mem=2g` per executor.
- The optional preparation environment below, only where the dataset is prepared.
  RDKit and Meeko are not required on Flame executors.

`flmenv.sh` selects SDK paths; it does not select the Python interpreter.
Serialized service code requires matching client and executor Python minor
versions, which the example checks before connecting.

## Prepare the library

From `examples/vina`, use an isolated preparation environment separate from the
client SDK environment. This installs only the preparation dependencies, so
Vina is not needed on a macOS or ARM preparation machine:

```shell
uv run --no-project --python 3.12 \
  --with rdkit==2025.9.3 --with meeko==0.6.1 \
  --with numpy==2.2.6 --with scipy==1.15.3 --with gemmi==0.7.3 \
  python prepare.py --output data/screening
```

The script downloads the source-prepared 3D SDF archives directly from DUD-E and
checks their pinned SHA-256 hashes. Pinned RDKit/Meeko versions parameterize each
selected source molecule into PDBQT. Its coordinates and explicit-hydrogen state
are retained; the script does not generate conformers or assign protonation at a
chosen pH. Meeko calculates Gasteiger partial charges.

Selection is deterministic in archive record order. A normalized copy supplies
a parent-connectivity identity for deduplication across source states and the
active/decoy groups. Imatinib is excluded from the library and copied separately
from the bundled reference. Duplicates and preparation errors are recorded, with
later records used to fill the fixed quotas. Insufficient valid unique compounds
cause preparation to fail instead of producing a smaller or duplicated library.

Preparation writes:

- `data/screening/ligands/`: 100 library PDBQT files plus one reference.
- `data/screening/manifest.json`: source URLs/hashes, preparation versions,
  chemical identities, source classes/IDs, selected records, per-file hashes,
  and skipped records.
- `data/screening/sources/`: verified original archives for reproducibility.
- `data/screening/NOTICE`: provenance and use terms.

Use a fresh output directory to prepare again. Generated DUD-E data and screening
results are excluded from Git and Flame application packages.

## Run on Flame

With the client SDK available in Python 3.12:

```shell
source /usr/local/flame/sbin/flmenv.sh --python-version 3.12
cd examples/vina
python3.12 main.py \
  --ligands data/screening/ligands \
  --output results/abl1-screen-001 \
  --app-name vina-screen-001 \
  --keep
```

Use a fresh output directory for every run. A dataset manifest beside the ligand
directory is verified before registration, and its contents are copied into the
run record. Each compound is a separate task; Flame `batch_size` stays 1 because
it controls gang scheduling rather than ligand grouping.

`--keep` leaves the application and session open for inspection. Without it,
normal completion closes the runtime. Client/transport errors leave remote state
intact automatically: such rows are marked `unknown`, because a failed watch
does not prove the remote task failed. The run record includes the session ID for
reconciliation. Interrupting a run after submission also preserves remote state.

An existing application name reuses its registered package. Choose a new name
when deploying changed dependencies or files. One service instance is retained
per executor; additional ligands reuse its receptor maps.

Configuration:

- `VINA_EXHAUSTIVENESS`: search effort, default 8.
- `VINA_SEED`: Vina seed, default 1; use a nonzero seed for reproducibility.
- `VINA_WARMUP`: minimum warm executors, default 1. More tasks can scale out.
- `--ligands`: input PDBQT directory; defaults to the single bundled reference.
- `--app-name`: application name, default `vina-dock`.
- `--output`: a new results directory; defaults to a UTC timestamp under
  `results/`.

The 1IEP reference is a difficult redocking case; the Vina tutorial uses
exhaustiveness 32 for pose recovery. The default 8 is a workload setting, not a
validated pose-recovery protocol.

## Results

Files are updated as each ligand finishes:

- `results.csv`: successful ligands ranked by ascending Vina affinity
  (kcal/mol), with source class/ID, chemical identity, timing, executor identity,
  actual seed and pose path. Ties are ordered by name.
- `poses/<ligand>.pdbqt`: the best docked pose, including Meeko's mapping
  remarks where provided.
- `failures.csv`: per-ligand errors and client-side unknown outcomes.
- `run.json`: input and receptor hashes, dataset manifest, configuration,
  application/session IDs, observed worker instances/runtime versions and summary.

Ordinary ligand errors are returned as rows so the other ligands can finish.
Dependency, receptor and map-initialization failures remain infrastructure
failures. Native process crashes cannot be caught as Python ligand exceptions.
A run with errors or unknown outcomes exits nonzero.

Timing fields distinguish different costs:

- `initialization_seconds` measures client application initialization and upload.
- `service_creation_seconds` measures context upload/session creation, not
  asynchronous worker package installation.
- `time_to_first_result_seconds` includes queueing, worker startup and docking.
- Per-result `setup_seconds` measures engine initialization, including
  `map_seconds`. These repeat per instance and are summed only once per
  `instance_id` in the run summary.
- `dock_seconds` measures that ligand's Vina work and pose extraction;
  `wall_seconds` also includes queueing and result retrieval.
- `warm_invocations` counts results after the first call on an instance.

Sum of worker seconds is not end-to-end wall time: executors run in parallel.
Compare cold and warm runs using the recorded instance IDs and first-result
latency. Preparation versions are pinned; reproduce recorded docking runtime
versions when comparing scientific results.

## Quick checks

The bundled single-ligand example still works:

```shell
python3.12 main.py --output results/reference-001 --keep
```

Local docking requires Linux x86_64 and Vina:

```shell
python3.12 -m pip install vina==1.2.7
VINA_EXHAUSTIVENESS=8 python3.12 engine.py
```

Offline tests require neither a cluster nor native Vina:

```shell
python3.12 -m unittest discover -s tests -t . -v
```

# Filesystem storage performance report (#577)

Measured on 2026-10-14 with `flmping -p -t 30000`: submit 30,000 tasks
concurrently and wait for all to finish (session creation and closing are not
timed). Every sample finished 30,000/30,000 Succeeded with 0 Failed unless noted.

## Summary

- **Locking change:** with durable `fs` storage on a local podman VM, the new code
  runs a warm median of 1,120 tasks/s against 777 for old main (+44%). Cold runs
  were 1,244 to 1,310 against 860 to 922. The new code was also steadier from
  sample to sample.
- **Kubernetes:** with `fs` on the cluster's default cinder volume, throughput is
  about 200 tasks/s. The limit is the sync latency of that volume. Moving the
  session data to node-local disk raised it to about 790 tasks/s, and `none`
  storage reached about 14,500 tasks/s.
- **Implication:** on slow storage, per-write `sync_data` cost now sets the
  ceiling, not lock contention. The design already names a sync policy as the first
  follow-up (section 2 of [design.md](../design.md)).
- **Health:** no heartbeat timeouts in the final local matrix. On Kubernetes one
  GPU node missed a heartbeat once, in a cinder run, and then stayed NotReady
  because of a recovery gap (section 3). Neither result shows #577 is fixed or
  reproduced; they are single observations.

## Code under test

| | Revision |
| --- | --- |
| New code | `fix/filesystem-runtime-isolation`, merged as xflops/flame #576 (main `4b7266b1`, tree identical to branch head `f477f732`) |
| Baseline | old main `ac218db3` |

The local new-code image was built from `6db5de45` plus the working tree at the time
(only later renames differ from the merged code). Kubernetes ran the CI-published
`latest` images built from the merged main.

## 1. Local comparison: old main vs new code

**Setup.** A local podman VM (5 CPUs, 8 GiB), one worker with five 1 CPU / 1 GiB
executors, two session-manager Tokio threads, info logging, no TLS, filesystem
storage. Each variant used fresh volumes. Variants alternated old, new, old, new,
with four samples each (one cold, three warm). Node health was sampled every 3 s.

| Variant | Cold | Warm 1 | Warm 2 | Warm 3 | Warm median |
| --- | ---: | ---: | ---: | ---: | ---: |
| old, round 1 | 859.85 | 821.33 | 786.04 | 738.26 | 786.04 |
| new, round 1 | 1310.22 | 1192.70 | 1119.74 | 1089.88 | 1119.74 |
| old, round 2 | 921.97 | 847.15 | 767.68 | 578.94 | 767.68 |
| new, round 2 | 1243.68 | 1167.72 | 1120.45 | 1080.26 | 1120.45 |

Across all warm samples the median is 776.9 tasks/s for old main and 1120.1 for the
new code (+44.2%). Old main ranged from 579 to 922 tasks/s, the new code from 1080
to 1310. Throughput falls over consecutive samples in both.

An earlier, aborted attempt lost old main's worker (NotReady) during its fourth
sample, so that sample failed. It did not recur in the final matrix. Its samples
are in the data file.

## 2. Kubernetes

**Setup.** Fresh Helm install of `charts/flame` (10 executor managers, 2 of the 10
nodes with 8 GPUs, at most 10 executors per node, `fs` storage) using the
`latest` images published after the #576 merge. Only the session manager's storage
changed between rows.

| Session-manager storage | Samples (tasks/s) |
| --- | --- |
| cinder volume (network block storage) | 214.74, 203.88, 193.87, 198.98 |
| node-local `emptyDir` (ext4) | 806.17, 790.37, 764.21 |
| `storage: none` | 14,742.01, 14,224.75, 14,584.35 |

**Why.** Synchronous 4 KiB writes (`dd oflag=dsync`, 300 writes) took 0.82 s on the
cinder volume (about 2.7 ms each) and 0.15 to 0.17 s on node-local disk (about
0.5 ms each), a 5x difference. Throughput rose about 4x with it. `none` skips all
sync work, so its 14.5k figure is the ceiling with persistence removed, not a
filesystem-engine result.

**Available storage.** The cluster has only `cinder` (the default) and `vast` (a
network file system). There is no local-SSD class. A RAM-backed `emptyDir` is the
next option to try, but the chart does not expose `medium: Memory`.

## 3. Health observations on Kubernetes

- **Missed heartbeat.** In one cinder run a GPU node missed its heartbeat 14 s after
  a 30,000-task burst began. The session manager marked it NotReady and deleted its
  three executors. It did not recur in the node-local-disk or `none` runs.
- **Recovery gap.** The node stayed NotReady: its executor manager kept
  re-registering with two executors the session manager no longer knew, and the
  session manager rejects that by design ("clear them before it joins"). Nothing
  clears them automatically. Restarting the pod did. This is separate from the
  storage work and worth its own follow-up.
- **Orphan executors.** Restarting executor managers right after a session-manager
  restart left two `Void` executors the session manager still listed, and a
  session stalled at 30,000 Pending until the session manager restarted cleanly.
  State alignment does not clean up executors an executor manager no longer reports.
- The two restarts per executor-manager pod after install were startup retries
  before the session manager was ready.

## Limits

- The local comparison has two alternating rounds, no main-vs-main control, and no
  page-cache flush between variants. The 3 s health sampling adds load. Differences
  of a few percent are within the observed spread.
- The local VM's fsync latency was not measured, so the local and cluster results
  are compared by throughput only.
- There is no old-main run on Kubernetes, so the cluster numbers show no
  before/after difference.
- Heartbeat behavior was observed through NotReady transitions and logs, not
  measured as latency.

## Data

- [`samples.csv`](samples.csv): every throughput sample, including the aborted
  attempt.
- [`fsync.csv`](fsync.csv): synchronous-write latency per volume.

Logs and node snapshots are not included because they contain host names and
addresses.

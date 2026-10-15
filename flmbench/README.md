# flmbench

Burst benchmark for a running Flame cluster. It creates `--sessions` sessions at
the same time, submits `--tasks` tasks to each (all at once, like `flmping`), and
waits for every task. Each session runs the `flmping` application, so the
`flmping` worker must be registered.

A round has three steps. All tasks are created with `create_task` (timed), and each
task is watched as soon as it exists. Then the benchmark waits for every watch to
finish. Last, it reads each task's result. The watch callback records the time the
task reaches a terminal state, so every task has its own end-to-end time (from the
start of its submission to that callback), however long the other tasks take to
submit.

```sh
flmbench -s 4 -t 10000 -n 3 -o result.json
```

It is installed with the client profile (`flmadm install --client`), so it is
included in the `flame-console` image. The client connects the same way as the other Flame tools (`~/.flame/flame.yaml`,
or `FLAME_ENDPOINT`).

| Option | Meaning |
| --- | --- |
| `-s, --sessions` | Sessions run concurrently in each round (default 1) |
| `-t, --tasks` | Tasks per session (default 1000) |
| `-n, --rounds` | Rounds; the first is reported as cold, the rest as warm (default 1) |
| `-d, --duration` | Milliseconds each task sleeps in the worker |
| `-m, --memory` | Memory each task allocates, for example `64MiB` |
| `-P, --priority`, `--resreq` | Session priority and resource request |
| `--timeout` | Seconds to wait, once all tasks are created, for them to finish; unfinished tasks are reported as failed (default 600) |
| `--health-interval` | Seconds between cluster health samples, 0 to disable (default 3) |
| `-o, --output` | Write all rounds and a summary as JSON |

Per round it reports:

- successes and failures, and tasks per second over the whole round;
- **submit** time and rate (first submission until the last `create_task` returned)
  and **drain** time (from then until the last task finished); session creation and
  closing are excluded;
- latency percentiles, end to end per task and for the `create_task` call alone
  (successful tasks only);
- from the health samples, how many samples saw a node that was not Ready and the
  largest executor count. Node names are not recorded.

The exit code is non-zero if any task did not succeed.

## Example

Two sessions of 200 tasks each, two rounds, against a 10-node cluster:

```console
$ flmbench -s 2 -t 200 -n 2 --health-interval 2 -o result.json
2 session(s) x 200 task(s), 2 round(s); the first round is cold

round 1: 400/400 succeeded in 1.11s (361.7 tasks/s); submitted in 0.71s (559.9 tasks/s), drained in 0.39s
round 2: 400/400 succeeded in 1.14s (349.8 tasks/s); submitted in 0.66s (609.9 tasks/s), drained in 0.49s

| Round | Kind | Succeeded | Failed | Submit (s) | Drain (s) | Wall (s) | Tasks/s | p50 (ms) | p90 (ms) | p99 (ms) | Max (ms) | Not ready | Executors |
|-------|------|-----------|--------|------------|-----------|----------|---------|----------|----------|----------|----------|-----------|-----------|
| 1     | cold |   400/400 |      0 |       0.71 |      0.39 |     1.11 |   361.7 |   1016.1 |   1072.7 |   1096.4 |   1099.5 |         0 |        28 |
| 2     | warm |   400/400 |      0 |       0.66 |      0.49 |     1.14 |   349.8 |   1014.1 |   1130.7 |   1133.8 |   1136.1 |         0 |        29 |
warm median: 349.8 tasks/s
wrote result.json
```

Here submission took more than half of each round, so the client submitting
tasks, not the cluster running them, set the pace. `Not ready` counts the health
samples in which at least one node was not Ready, and `Executors` is the largest
executor count seen. `result.json` has the same numbers for every round, so a
report can be built from it:

```json
{
  "config": { "sessions": 2, "tasks_per_session": 200, "rounds": 2, "...": "..." },
  "rounds": [
    {
      "round": 2,
      "kind": "warm",
      "succeeded": 400,
      "failed": 0,
      "submit_s": 0.656,
      "drain_s": 0.488,
      "wall_s": 1.143,
      "tasks_per_sec": 349.8,
      "submit_latency": { "p50_ms": 575.1, "p99_ms": 646.0, "...": "..." },
      "latency": { "p50_ms": 1014.1, "p90_ms": 1130.7, "p99_ms": 1133.8, "...": "..." },
      "health": { "samples": 2, "nodes": 10, "min_ready_nodes": 10, "max_executors": 29, "...": "..." }
    }
  ],
  "summary": { "all_succeeded": true, "warm_median_tasks_per_sec": 349.8, "...": "..." }
}
```

A heavier burst across several sessions, with a 5 ms task and 1 MiB of memory
per task:

```sh
flmbench -s 8 -t 5000 -n 4 -d 5 -m 1MiB -o burst.json
```

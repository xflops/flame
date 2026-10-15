# Stress test

Pushes a running Flame cluster well past its steady load with
[`flmbench`](../../../flmbench/README.md). It runs a ladder of bursts: more
sessions at the same total, many small sessions, then large task payloads. Every
step must finish all its tasks with every node ready. A final small burst checks
that the cluster recovered.

```sh
PROFILE=default testing/system/stress/run.sh
```

Run it from a client with `flmbench` and `jq` installed and a configured Flame
client, such as `flame-console`. The JSON result of each step is kept in
`OUTPUT_DIR` (default `stress-results`).

| Profile | Steps (sessions x tasks, plus options) |
| --- | --- |
| `smoke` | 2x100, 4x250, 8x250 |
| `default` | 1x10000, 10x3000, 50x1000, 100x500, 20x2000 with 64 MiB and 100 ms per task |
| `heavy` | 1x50000, 10x10000, 100x2000, 200x1000, 50x2000 with 256 MiB and 500 ms per task |

| Variable | Default | Meaning |
| --- | --- | --- |
| `PROFILE` | `default` | `smoke`, `default` or `heavy` |
| `TIMEOUT` | `900` | Seconds to wait for each burst |
| `OUTPUT_DIR` | `stress-results` | Where the per-step JSON goes |

## Python cases

pytest cases, run from a client with `flamepy`:

```sh
flame-test stress py          # or: cd testing/system && FLAME_E2E_SYSTEM_TESTS=stress pytest stress
```

| File | API | What it does |
| --- | --- | --- |
| `test_app_stress.py` | `flamepy.app` | Fuzzed input, output and common-data payloads through an App service |
| `test_session_stress.py` | session API | Parallel sessions with a fuzzed session/task workload (`FLAME_STRESS_PROFILE=smoke\|default\|heavy`) |

Also available as `make e2e-py-system-stress` (docker compose).

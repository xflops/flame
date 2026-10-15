# Longevity test

Keeps a running Flame cluster busy with [`flmbench`](../../../flmbench/README.md)
bursts for hours, to catch problems that only show up over time: leaked file
descriptors or memory, nodes that drop out, executors that stay stuck.

```sh
DURATION=8h SESSIONS=10 TASKS=3000 testing/system/longevity/run.sh
```

Run it from a client with `flmbench` and `jq` installed and a configured Flame
client, such as `flame-console`. Each iteration is one `flmbench` burst. The test
fails on the first burst where a task fails or a health sample finds a node not
ready, and passes when `DURATION` elapses with every burst clean. Each burst's JSON
result is kept in `OUTPUT_DIR` (default `longevity-results`).

| Variable | Default | Meaning |
| --- | --- | --- |
| `DURATION` | `1h` | Total run time (`30m`, `8h`, or seconds) |
| `SESSIONS` | `10` | Sessions per burst |
| `TASKS` | `3000` | Tasks per session |
| `PAUSE` | `10` | Seconds between bursts |
| `OUTPUT_DIR` | `longevity-results` | Where the per-burst JSON goes |

## Python cases

pytest cases, run from a client with `flamepy`:

```sh
FLAME_LONGEVITY_DURATION_SECONDS=3600 flame-test longevity py
```

| File | API | What it does |
| --- | --- | --- |
| `test_app_longevity.py` | `flamepy.app` | Batches of App tasks (`FLAME_LONGEVITY_BATCH`, default 100) for the duration; every result is checked and batch time must not grow more than 5x |
| `test_session_longevity.py` | session API | One session kept active and checked at an interval |

Also available as `make e2e-py-system-longevity` (docker compose).

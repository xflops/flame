# flamepy.app benchmark

The standalone benchmark in `benchmarks/app.py` exercises the App API against
a running Flame cluster. Its cold-start, scale-out, and steady-state cases use
the same session and task counts as the Rust benchmark. Each timed App sample
creates and closes its own service sessions; app registration is excluded.
Every result is checked. The Markdown report shows wall time and throughput
ranges without imposing a performance threshold.

From a configured client environment, run:

```sh
python3 e2e/benchmarks/app.py
```

The Host Shim job in `.github/workflows/e2e-bench.yaml` runs this benchmark
inside `flame-console`; the CRI Shim job runs it with the installed Python
client. Both add the report to the GitHub Actions summary.

Before each Rust and App benchmark suite, CI waits for every session to close
and every executor to reach `Released`, so a prior suite's retained executors
do not consume the next suite's capacity. The wait uses `flmctl list -s -o json`
and `flmctl list -e -o json`, parsed with `jq`. JSON output is available for
every `flmctl list` and `view` resource.

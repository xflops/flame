# Flame benchmarks

The Rust, `core.py`, and `app.py` suites use the same
cold-start, scale-out, and steady-state session and task counts. Rust and
flamepy.core call the `flmping` worker. flamepy.app calls a Python `echo`
service, so the tables compare the end-to-end paths rather than identical
worker code. Each timed sample creates and closes its sessions; App
registration is excluded. Every result is checked. The Markdown reports show
wall time and throughput ranges without imposing a performance threshold.

From a configured client environment, run from the repository root:

```sh
cd benchmarks
python3 core.py
python3 app.py
```

This is a separate Python project with no declared dependencies. The App
benchmark packages this directory, so its cold-start result does not include
installing the `e2e` project's test dependencies. The Flame Python SDK must
already be installed in the client and worker environments. The worker builds
and installs this project's source package on its first use; App registration
happens before timed samples.

The Host Shim job in `.github/workflows/e2e-bench.yaml` runs both Python
benchmarks inside `flame-console`; the CRI Shim job runs them with the
installed Python client. Both add the reports to the GitHub Actions summary.

Before each benchmark suite, CI waits for every session to close
and every executor to reach `Released`, so a prior suite's retained executors
do not consume the next suite's capacity. The wait uses `flmctl list -s -o json`
and `flmctl list -e -o json`, parsed with `jq`. JSON output is available for
every `flmctl list` and `view` resource.

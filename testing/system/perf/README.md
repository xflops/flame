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
cd testing/system/perf
python3 core.py
python3 app.py
```

`first_use.py` times the first command in a new sandbox on a new executor and on a retained one, for the
latency of creating a sandbox with a cached image. See its docstring for the run, and for the
`flame::perf` log points that show where the first command spends its time.

This is a separate Python project with no declared dependencies. The App
benchmark packages this directory, so its cold-start result does not include
installing the `e2e` project's test dependencies. The Flame Python SDK must
already be installed in the client and worker environments. The worker builds
and installs this project's source package on its first use; App registration
happens before timed samples.

The Host Shim job in `.github/workflows/e2e.yaml` runs both Python
benchmarks inside `flame-console`; the CRI Shim job runs them with the
installed Python client. Both add the reports to the GitHub Actions summary.

`agent.py` times `flamepy.agent` with the same cold-start, scale-out, warm-up and
steady-state cases and the same report as `core.py` and `app.py`, once for
`Sandbox` and once for the agent `Session`. Both run on the `flmbox` application,
so it needs a cluster whose executors run the host shim and can run `runsc` and
pull the sandbox image. Each task is `echo ok` run with `runsc exec`, far slower
than the `flmping` or echo task of the other suites, so the 1000-task cases use
`FLAME_AGENT_BENCHMARK_HEAVY_TASKS` tasks (default 100). The first task of each
sample includes binding an executor and starting the sandbox, which is what
changes with the sandbox startup work.

`.github/workflows/e2e.yaml` runs it after the `app.py` step, in the CRI
Shim job: that bare-metal cluster has `runsc`, and
`ci/cri/applications/flmbox.yaml` registers `flmbox` there. `flmbox` runs on the
host shim, so the benchmark cluster config lists both shims
(`executors.shims: [cri, host]`) and its executor manager runs both. The report
goes to the Actions summary. The script pins its image by digest (see `IMAGE`):
each creation resolves a tag with the registry, the benchmark creates more than 200
sandboxes, and Docker Hub rate limits an unauthenticated address to 100 pulls.

```sh
python3 agent.py
```

Before each benchmark suite, CI waits for every session to close
and every executor to reach `Released`, so a prior suite's retained executors
do not consume the next suite's capacity. The wait uses `flmctl list -s -o json`
and `flmctl list -e -o json`, parsed with `jq`. JSON output is available for
every `flmctl list` and `view` resource.

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

This is a separate Python project with no declared dependencies. The App
benchmark packages this directory, so its cold-start result does not include
installing the `e2e` project's test dependencies. The Flame Python SDK must
already be installed in the client and worker environments. The worker builds
and installs this project's source package on its first use; App registration
happens before timed samples.

The Host Shim job in `.github/workflows/e2e-bench.yaml` runs both Python
benchmarks inside `flame-console`; the CRI Shim job runs them with the
installed Python client. Both add the reports to the GitHub Actions summary.

`agent.py` times `flamepy.agent`: `Sandbox` (create, first command, warm commands,
suspend and resume, and parallel creation) and the agent `Session` (open, first
command, warm commands, and parallel sessions). Both run on the `flmbox`
application, so it needs a cluster whose executor managers can run `runsc` and
pull the sandbox image; the first command of each sample includes binding an
executor and starting the sandbox, which is what changes with the sandbox
startup work. The case sizes and the image are set with the
`FLAME_AGENT_BENCHMARK_*` variables listed in the script.

`.github/workflows/e2e-bench.yaml` runs it after the `app.py` step, in the CRI
Shim job: that bare-metal cluster has `runsc`, and
`ci/cri/applications/flmbox.yaml` registers `flmbox` there. `flmbox` runs on the
host shim, so the benchmark cluster config lists both shims
(`executors.shims: [cri, host]`) and its executor manager runs both. The cluster
has no session storage, so the suspend case is skipped, and the report goes to
the Actions summary.

```sh
python3 agent.py
```

Before each benchmark suite, CI waits for every session to close
and every executor to reach `Released`, so a prior suite's retained executors
do not consume the next suite's capacity. The wait uses `flmctl list -s -o json`
and `flmctl list -e -o json`, parsed with `jq`. JSON output is available for
every `flmctl list` and `view` resource.

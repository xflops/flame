# RFE683: Install Flame with scripts and retire flmadm

Issue: [#683](https://github.com/xflops/flame/issues/683)

Status: implemented in this PR (the scripts, the tests, the missing-configuration errors, the callers,
the release job that publishes the scripts, and the removal of `flmadm`).
Items marked **(verify)** are assumptions about the current code that were not checked.

## 1. Motivation

**Background:**
`flmadm` (about 4,600 lines of Rust, `flmadm/`) installs Flame: it clones or takes a source tree,
builds it, copies the binaries, installs the Python SDK with `uv`, generates `flame-cluster.yaml`,
writes the systemd units, and uninstalls with a backup. Its problems:

- **Bootstrap.** `flmadm` is the installer, so you need it before you can install anything: build it
  from a checkout, or already have an installation. There is no way to install Flame on a clean
  machine from a release.
- **No cluster.** The config it writes is fixed: `cluster.endpoint: http://127.0.0.1:8080` and cache
  `advertise: lo`. A worker cannot join a head node without hand-editing YAML.
- **No release binaries.** The GitHub release publishes no executables. The images, `make install`,
  the e2e jobs and the docs all run `flmadm install --src-dir . --skip-build` against a source tree.
- **Weight.** A Rust crate (with its dependencies and 65 tests) for work that is copying files and
  writing text.

**Target:**
Three scripts in `<top dir>/scripts/` replace `flmadm` completely:

| Script | Purpose |
|---|---|
| `scripts/build.sh` | Build the release package from a source tree. |
| `scripts/setup.sh` | Install the cluster: the head node or a joining worker. Served from `https://xflops.io/flame/setup.sh`. |
| `scripts/sdk.sh` | Install the client: the tools `flmctl`, `flmping`, `flmexec` and `flmbench`, and the Python SDK `flamepy`. Served from `https://xflops.io/flame/sdk.sh`. |

Success criteria:

- On a clean Linux machine, `curl -fsSL https://xflops.io/flame/setup.sh | sh` installs a head node from
  the latest release, and `… | sh -s -- --join` installs a worker, with no Rust
  toolchain and no other Flame software present.
- `curl -fsSL https://xflops.io/flame/sdk.sh | sh` installs the client tools and `flamepy` on a client
  machine.
- The release publishes the package archive and the four client tools, each with its checksum; the
  two scripts are served from `xflops.io`, not from the release.
- The images, `make`, CI and the docs use the scripts; the `flmadm` crate is gone.

**Non-goals:** DEB/RPM packages, macOS or Windows installs, signed releases (a follow-up), a join
token (a follow-up; see 5), and installing the examples.

## 2. Function Specification

### 2.1 Package

`build.sh` writes the package; `setup.sh` installs from it. One archive per platform,
`flame-<tag>-<os>-<arch>.tar.gz` (`os` is `linux`; `arch` is `amd64` or `arm64`), holds one
top-level directory of the same name:

```text
flame-<tag>-linux-<arch>/
  bin/        flame-session-manager flame-executor-manager flame-object-cache
              flmctl flmping flmping-service flmexec flmexec-service flmbench flmbox  uv
  wheels/     flamepy-<version>-py3-none-any.whl
  config/migrations/sqlite/     the database migrations (installed to <prefix>/migrations/sqlite, where the
                                session manager reads them relative to its working directory)
  config/applications/          the application manifests (flmbox, flmexec, flmping, flmrun)
  sbin/flmenv.sh.in             the environment script template (section 2.3, step 4)
  sbin/setup.sh  sbin/sdk.sh    the hosted scripts (self-contained), so a downloaded package installs itself
  LICENSE  VERSION  PACKAGE_FORMAT
```

`sbin/setup.sh` and `sbin/sdk.sh` are the same self-contained scripts that are hosted at
`https://xflops.io/flame/setup.sh` and `.../sdk.sh` (section 2.5): run from a package they install that
package, with no network (`dist/package/sbin/setup.sh`, `dist/package/sbin/sdk.sh`); from a URL they
download the release. `PACKAGE_FORMAT` is an integer (1 for this layout) that the scripts check, so a
hosted script refuses a package layout it does not know instead of installing it wrong.
Each archive has a `<archive>.sha256` in `sha256sum` format.

### 2.2 `scripts/build.sh`

```text
scripts/build.sh [--output DIR] [--tag TAG] [--force]
```

Builds everything from the source tree the script lives in (`cargo build --release` for the
workspace, `uv build --wheel sdk/python`, a copy of `uv`), with no prebuilt-binary mode: cargo reuses
what is up to date, so building again costs little. It writes `DIR/package/` (the layout above),
`DIR/flame-<tag>-<os>-<arch>.tar.gz`, `DIR/<tool>-<tag>-<os>-<arch>` for `flmctl`, `flmping`, `flmexec` and
`flmbench`, and a `.sha256` for each archive and binary.

| Option | Default | Meaning |
|---|---|---|
| `--output DIR` | `./dist` | Where the assets are written. A non-empty directory is replaced only with `--force`. |
| `--tag TAG` | `v<version in common/Cargo.toml>` | The version recorded in `VERSION` and used in the names. |

Needs `cargo`, `uv`, `tar` and `sha256sum` (or `shasum`). Exits non-zero on any failure and writes
nothing under `DIR` unless the whole build succeeded (it assembles in a temporary directory and moves
it into place).

### 2.3 `scripts/setup.sh`

```text
setup.sh [--join] [--prefix DIR] [--package DIR|FILE|URL] [--version TAG]
         [--python-version V]... [--no-systemd] [--force]
```

There are two modes, and no flags to pick components:

| Mode | Installs | Systemd units |
|---|---|---|
| `setup.sh` (the head node) | `flame-session-manager`, the migrations and the application manifests; **no object cache and no command-line tools** | `flame-session-manager` |
| `setup.sh --join` (a worker) | `flame-executor-manager`, `flame-object-cache`, `flmping-service`, `flmexec-service`, `flmbox`, `uv` and `flamepy` | `flame-executor-manager`, `flame-object-cache` |

`setup.sh` installs only the head: no executor manager and no object cache. To run everything on one
machine, run the script twice: `setup.sh`, then `setup.sh --join`. Both installs share one prefix, so
the second run finds the files of the first (the unit files and `sbin/flmenv.sh`) and asks before
overwriting, or needs `--force`. `setup.sh` has no option for the cluster's address or the cache's interface:
it generates `conf/flame-cluster.yaml` from `FLAME_*` settings (section 3.3). Clients use section 2.4.

Other options:

| Option | Default | Meaning |
|---|---|---|
| `--prefix DIR` | `/usr/local/flame` | Installation directory (absolute). |
| `--package X` | the release | A package directory (or the directory a package was extracted into), a `.tar.gz` archive, or an `http(s)` URL of an archive. |
| `--version TAG` | latest release | The release to download. `FLAME_VERSION` is the same setting. |
| `--python-version V` | `3.11 3.12 3.13 3.14` | Python versions to install `flamepy` for (repeatable). |
| `--no-systemd` | systemd on | Do not write unit files or start anything (for containers and image builds). |
| `--force` | off | Replace an installation that is already there without asking. Without it the script **asks** whether to overwrite, and with no terminal to ask on it stops. |

Environment: `FLAME_VERSION`, `FLAME_BASE_URL` (the release download base, for a mirror; default
`https://github.com/xflops/flame/releases/download/<tag>`).

Behaviour:

1. **Find the package.** `--package` wins (this is how a build is installed from a checkout:
   `scripts/setup.sh --package dist/package`). Without it the script downloads: it resolves the tag
   (`--version`, `FLAME_VERSION`, else the redirect of `https://github.com/xflops/flame/releases/latest`),
   fetches `<base>/flame-<tag>-<os>-<arch>.tar.gz` and its `.sha256` (required), verifies the archive,
   checks its `PACKAGE_FORMAT`, and extracts it to a temporary directory that is removed on exit. A
   local archive with a `.sha256` beside it is verified too; a bare local archive is trusted.
2. **Install the mode's components** under `--prefix`: `bin/`, `conf/`, `lib/`, `logs/`, `work/`,
   `data/` (mode 0700), `migrations/`, `wheels/`.
3. **Python SDK.** For each Python version, `uv pip install --python python<V> --prefix <prefix>
   <wheel>` with `UV_CACHE_DIR=<prefix>/data/cache/uv`; a version with no interpreter is skipped
   with a warning, and no installed version at all is an error.
4. **`sbin/flmenv.sh`**: generated from the package's `sbin/flmenv.sh.in` by replacing its two
   placeholders, `@PREFIX@` (the installation prefix) and `@PYTHON_VERSIONS@` (the versions `flamepy`
   was installed for, newest last), with `sed`. The result sets `FLAME_HOME`, `PATH`, `PYTHONPATH` and
   `LD_LIBRARY_PATH` for one Python version (`--python-version`, else the host's `python3` when
   `flamepy` is installed for it, else the newest installed), as `flmadm` generated it. The template's
   header names the placeholders, so a Dockerfile can render its own copy with the same `sed`.
5. **Files** (section 3.3): `conf/flame-cluster.yaml` is generated from the `FLAME_*` settings (the
   environment, then `./.env`, then defaults), `conf/applications/` gets the manifests and the migrations
   are installed. Before anything is written, the script lists the files this install would write (the
   binaries, the config, the manifests, the migrations, the unit files, `sbin/flmenv.sh`); if any
   exists and `--force` is not given, it names the file and asks "Overwrite the existing installation?
   [y/N]". The answer is read from the terminal (`/dev/tty`), not stdin, which is the script itself when it
   is piped from curl. A yes, or `--force`, replaces everything the script writes; a no, an empty
   answer, or no terminal at all (a CI job, `ssh` without a tty) stops the script with a message and
   nothing written.
6. **Systemd** (section 3.4): unit files for the installed components, then `daemon-reload`, then
   `systemctl enable --now` semantics for each: enabled, then started (restarted, if it was running, so
   replaced binaries take effect; that interrupts running tasks).
   `--no-systemd` skips this step
   and the script prints the commands to run the binaries by hand.
7. **Summary**: what was installed, where, and the next command (for a head node, the exact `--join`
   command for workers, and where the generated config is).

There is no uninstall option: `scripts/README.md` documents removing Flame (disable and remove the
units, reload systemd, copy out what to keep, delete the prefix), which is a few commands and needs no
script.

Exit codes: 0 success; 1 failure (a message on stderr says what); 2 invalid arguments.

Examples:

```sh
curl -fsSL https://xflops.io/flame/setup.sh | sh                                    # head node
curl -fsSL https://xflops.io/flame/setup.sh | sh -s -- --join         # worker
curl -fsSL https://xflops.io/flame/setup.sh | FLAME_VERSION=v0.7.0 sh -s -- --join     # a pinned version
sudo scripts/setup.sh --package ./dist/package --join --prefix /opt/flame    # from a build
curl -fsSL https://xflops.io/flame/setup.sh | sh && curl -fsSL https://xflops.io/flame/setup.sh | sh -s -- --join   # one machine
```

### 2.4 `scripts/sdk.sh`

```text
sdk.sh [--prefix DIR] [--version TAG] [--python-version V]... [--force]
```

Installs the client tools and the Python SDK:

```sh
curl -fsSL https://xflops.io/flame/sdk.sh | sh
```

0. **From a package.** Run as `<package>/sbin/sdk.sh` (and without `--version`), it installs the tools
   in the package's `bin/` and the wheel in its `wheels/` instead of downloading: steps 2 and 3 below use
   the package, with no network except for the dependencies of `flamepy`.
1. Resolve the version (`--version`, `FLAME_VERSION`, else the latest release, as `setup.sh` does) and
   the platform.
2. **Tools.** For each of `flmctl`, `flmping`, `flmexec` and `flmbench`, download
   `<base>/<tool>-<tag>-<os>-<arch>` and its `.sha256` (required), verify it, and install it to
   `<prefix>/bin/<tool>` (mode 0755). The tools come individually, not from the package archive, which is
   large and carries the services.
3. **SDK.** Install `flamepy==<version>` from PyPI into `<prefix>` for every Python version requested
   (`--python-version`, repeatable, default 3.11 to 3.14, as `setup.sh`): `uv pip install --python
   python<V> --prefix` when `uv` is on `PATH` (a version with no interpreter is skipped with a warning),
   else `python<V> -m pip install --prefix` for each `python<V>` on `PATH`; no version at all is an
   error, and no pip is an error that says how to get it. `<version>` is the release tag as a
   PEP 440 version (`v0.7.0` is `0.7.0`, `v0.7.0-rc3` is `0.7.0rc3`, the mapping `hack/release.sh
   validate-metadata` already enforces between a tag and the Python version), so the SDK and the tools
   match. The `flamepy` dependencies come from PyPI too.
4. **`<prefix>/sbin/flmenv.sh`**, rendered from the same template as `setup.sh`'s (`@PREFIX@`,
   `@PYTHON_VERSIONS@` with the versions installed). Sourcing it puts the tools on `PATH` and selects
   the `flamepy` that matches the `python3` you run (else the newest) on `PYTHONPATH`.
5. **No `flame.yaml` is written or touched.** The script does not know where the cluster is, so the
   summary tells the user to create `$HOME/.flame/flame.yaml` (it prints the minimal file, the format of
   `ci/bm/flame.yaml`) or, for the tools other than `flmctl` and for `flamepy`, set `FLAME_ENDPOINT` and
   `FLAME_CACHE_ENDPOINT`. Until they do, the tools
   and `flamepy` report the error of section 3.8; they do not guess an endpoint.

| Option | Default | Meaning |
|---|---|---|
| `--prefix DIR` | `$HOME/.flame` | Where `bin/`, `lib/` and `sbin/flmenv.sh` go (no root needed). |
| `--version TAG` | the script's own release | The release to download. `FLAME_VERSION` is the same setting. |
| `--python-version V` | `3.11 3.12 3.13 3.14` | Python versions to install `flamepy` for (repeatable). |
| `--force` | off | Replace a client that is already there without asking. Without it `sdk.sh` asks, and with no terminal to ask on it stops. |

`FLAME_BASE_URL` overrides the download base, as for `setup.sh`. `sdk.sh` reads
`./.env` and the environment (the environment wins) and, only when `FLAME_ENDPOINT` is set, writes
`<prefix>/flame.yaml` with that endpoint and, if set, `FLAME_CACHE_ENDPOINT`. **There are no defaults**:
which cache a client uses, now that none runs on the head, is the site's to say, and without
`FLAME_ENDPOINT` no file is written (section 3.8 is what a client without one reports). An existing
`flame.yaml` is covered by the overwrite prompt and `--force`.

### 2.5 Release assets and hosting

The release publishes, for each of `linux-amd64` and `linux-arm64`, each with a `.sha256`:

- `flame-<tag>-linux-<arch>.tar.gz`, the package, and
- `flmctl-<tag>-linux-<arch>`, `flmping-<tag>-linux-<arch>`, `flmexec-<tag>-linux-<arch>` and
  `flmbench-<tag>-linux-<arch>`, the client tools as executables.

`flamepy` is on PyPI already. Every release also publishes its own `setup.sh` and `sdk.sh`, to the
`xflops/xflops.github.io` repository (served at `https://xflops.io`): `flame/<tag>/setup.sh` and
`flame/<tag>/sdk.sh`, and, for a stable release (not a release candidate), `flame/setup.sh` and
`flame/sdk.sh`, which are the latest release's. HTTPS only: the scripts run on the user's machine,
`setup.sh` as root, so the documentation shows only the HTTPS URLs. A hosted script has its release's tag
baked in (`scripts/inline.sh --version TAG`) and **installs that release by default**, so
`https://xflops.io/flame/setup.sh` installs the latest release and `https://xflops.io/flame/v0.7.0/setup.sh`
installs v0.7.0, with the script it was tested with. `PACKAGE_FORMAT` still protects a script from a
package layout it does not know (`--version` or `--package` can name another release). A release's job
(section 3.6) publishes them after the assets and `flamepy`, with a token for the site repository.

## 3. Implementation Detail

### 3.1 Scripts

- POSIX `sh` only (no bash arrays or `[[ ]]`): it must run under `dash` as `curl … | sh`.
- Each script is one file made of functions and ends with `main "$@"`, so a truncated download runs
  nothing. `set -eu`; every temporary directory is removed by a `trap` on exit.
- No dependency beyond `sh`, `curl` or `wget`, `tar`, `sha256sum` or `shasum -a 256`, `uname`, `cp`,
  `mkdir`, `install`, `sed`, and `uv` (bundled in the package, so it is not a prerequisite for
  `setup.sh`; `build.sh` needs `uv` and `cargo`).
- Files are written with `install -m` and generated text with `cat > file <<EOF`; nothing is built
  at install time.
- `setup.sh` reads `PACKAGE_FORMAT` from the package and exits with a message naming the format and
  the script version when it does not support it.
- `setup.sh` and `sdk.sh` are fetched separately, so they share no files at run time. The code both need
  (platform detection, version resolution, download, checksum verification, rendering `flmenv.sh`
  from the template `scripts/flmenv.sh.in`) is written once in `scripts/lib/common.sh` and inlined
  into each hosted script by the publish workflow; run from a checkout, the scripts source it.
- Test hooks, so the scripts are testable without root or systemd: `FLAME_SYSTEMD_DIR` (default
  `/etc/systemd/system`) and `FLAME_SYSTEMCTL` (default `systemctl`).

### 3.2 Components by mode

As in the table of section 2.3. The head is the `flmadm` control-plane profile minus `flmadm` and
`flmctl`, and the worker is the worker and cache profiles together (`flmadm/src/types.rs`). The
client profile is `sdk.sh` (section 2.4). The `flmbox` binary and the
`runsc` dependency are unchanged by this design.

### 3.3 Config

`setup.sh` generates `<prefix>/conf/flame-cluster.yaml` from `FLAME_*` settings, read from the environment
and from `./.env` (the environment wins; only `FLAME_*` keys are read, never executed), with defaults
that suit one machine: `FLAME_CLUSTER_NAME=flame`, `FLAME_ENDPOINT=http://127.0.0.1:8080`,
`FLAME_RESREQ=cpu=1,mem=2g`, `FLAME_POLICIES=priority drf das`, `FLAME_STORAGE=fs://<prefix>/data`,
`FLAME_SHIMS=host`, `FLAME_MAX_EXECUTORS=128`, `FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090`,
`FLAME_CACHE_ADVERTISE=lo`, `FLAME_CACHE_STORAGE=<prefix>/data/cache`. A value with a double quote or a
newline is refused before anything is written. The file has many more possible settings, so the site
administrator edits it for anything else; it is a target file like any other (the prompt, `--force`).
Head and worker get the same file. Images carry the defaults; a deployment mounts its own. The server
side is unchanged (`common/src/ctx.rs`).

### 3.4 Systemd units

The units `flmadm` writes today (`flmadm/src/managers/systemd.rs`), unchanged except for the Python
default: `Environment="RUST_LOG=info"`, `Environment="FLAME_HOME=<prefix>"`, `ExecStart=<prefix>/bin/<binary> --config
<prefix>/conf/flame-cluster.yaml`, logs appended to `<prefix>/logs/{fsm,fem,foc}.log`,
`Restart=on-failure`, `LimitNOFILE=65536`, and `MemoryMax=12G` for the object cache. The executor
manager also gets `Environment="FLAME_PYTHON_VERSION=<v>"` (PR #679: an app that names no Python runs
under the version the installation chose), where `<v>` is the host's `python3` version when `flamepy`
is installed for it, else the newest installed.

### 3.5 Building the package

`build.sh` runs `cargo build --release` in the source tree, then assembles the layout in a temporary
directory: binaries copied from `target/release`, `uv` from `PATH`, the wheel from `uv build --wheel
--out-dir wheels sdk/python` (removing the `.gitignore` uv leaves), the migrations and manifests
copied from the tree, `LICENSE`, `VERSION` and `PACKAGE_FORMAT`. The archive is written with
`tar -czf` (`--owner=0 --group=0 --numeric-owner` when the tar is GNU) from the directory above the
package, so it has one top-level directory. The four client tools are copied out as `<tool>-<tag>-<os>-<arch>`. The package also gets `sbin/flmenv.sh.in` (a copy of
`scripts/flmenv.sh.in`), the template of step 4 of section 2.3.
Each checksum is `sha256sum <file>` in the output directory.

### 3.6 Callers

| Caller | Change |
|---|---|
| `docker/Dockerfile.{fsm,foc,fem,console,flmrt}` | The builder stage runs `scripts/build.sh --output /tmp/dist` after the shared `cargo build --release`, then installs with the package's own scripts: `sbin/setup.sh --no-systemd` for the session manager image, `sbin/setup.sh --join --no-systemd` for the worker images (the object cache image keeps only `bin/flame-object-cache`, the runtime image drops the executor manager and the cache), and `sbin/sdk.sh --python python3.12` for the console image. The images carry no cluster config; the deployment mounts its own. `.dockerignore` includes `scripts/` and `LICENSE`. |
| `Makefile` | `package` runs `scripts/build.sh`; `install` and `install-dev` run `scripts/setup.sh --package`. |
| `.github/workflows/e2e.yaml`, `ci/cri/e2e.sh` | `build-binaries` runs `scripts/build.sh` and uploads the output as one artifact; the host-shim, CRI and benchmark jobs unpack it and run `scripts/setup.sh --package …` (head) and `scripts/setup.sh --package … --join 127.0.0.1` (worker), from the checkout; their cleanup disables and removes the units and deletes the prefix, as the documentation says. |
| `.github/workflows/release-publish.yaml` | `publish-binaries` runs `scripts/build.sh --tag $RELEASE_TAG` on `ubuntu-24.04` and `ubuntu-24.04-arm`, smoke-tests (the checksums, then the hosted `setup.sh` piped to `sh` installing the archive from an `http://127.0.0.1` URL as a head and as a `--join 127.0.0.1` worker, then the package's own `sdk.sh`, then `flmctl --version` and `import flamepy`; the PyPI install of `sdk.sh` is covered by tests because a parallel job publishes this release's `flamepy`), and uploads the archive, the four tools and their `.sha256` files for each arch. A `publish-scripts` job, after the assets and `flamepy`, tests the scripts, inlines them with the release's tag, and pushes them to `xflops/xflops.github.io` (section 2.5). |
| Docs | `README.md`, `docs/release-process.md`, `docs/tutorials/local-development.md`, the `flmbench`, `flmbox`, `object_cache` and example READMEs, and `docs/sdk/index.md` replace `flmadm install` with the script and section 2.4. The vLLM example READMEs drop `--with-examples` (run the examples from a checkout). |

### 3.7 Removed

The `flmadm` workspace member and directory, `CHANGELOG_FLMADM.md`, and the `tar`, `flate2`, `sha2`
and `reqwest` dependencies of the crate from `Cargo.lock`. Not carried over: interactive overwrite
prompts (the one overwrite question remains, `--force` skips it), `--with-examples`, `--uninstall` (removal is documented), the client profile's install location (`sdk.sh` is its replacement), and `flmadm
completion`.

### 3.8 Missing client configuration

The tools and the SDK must not guess where the cluster is, and there is **no fallback**: no default
endpoint, no `127.0.0.1:8080` assumed, no empty file written for them. Without a configuration they fail
with a message that says what to do. This ships in the same change as the scripts, because `sdk.sh` relies
on it (it writes no config unless `FLAME_ENDPOINT` is set), not as a later step. Today the Rust tools (`flmctl`, `flmping`, `flmexec`, `flmbench`, and
the services) fail with `<path> is not a file` (`common/src/ctx.rs`), and `flamepy` silently falls back
to `DEFAULT_FLAME_ENDPOINT = http://127.0.0.1:8080` (`sdk/python/src/flamepy/core/types.py`). Both
change:

- **No `flame.yaml` and no `FLAME_ENDPOINT`:** the Rust tools and `flamepy` exit with the same message:
  `no Flame cluster configured: <path of flame.yaml> does not exist and FLAME_ENDPOINT is not set.
  Create it with: <the minimal file>, or export FLAME_ENDPOINT=http://<head>:8080.` The message names
  the path they looked at (`FLAME_CONF`, `~/.flame/flame.yaml`).
- **`flmctl` reads only `flame.yaml`** (`FlameContext::from_file`), so its message names the file and no
  environment variable; the other tools and `flamepy` also accept `FLAME_ENDPOINT` alone (with no file), and
  say so in theirs.
- **A `flame.yaml` without an endpoint** gets the same message, naming the missing key.
- `flamepy` raises `FlameError(INVALID_CONFIG, …)` instead of connecting to `127.0.0.1:8080`; callers that
  relied on the default set `FLAME_ENDPOINT` or write the file. This is a behaviour change for anyone
  running a local cluster with the defaults, so it is called out in the release notes, and the unit
  tests and CI jobs that depend on the default set `FLAME_ENDPOINT` explicitly (the e2e jobs already
  copy `ci/bm/flame.yaml`). `DEFAULT_FLAME_ENDPOINT` is removed from `flamepy`, and the Rust side keeps no
  default either.

### 3.9 Tests

Python `unittest` under `scripts/tests/`, run by the existing `code-verify` workflow, with fake `cargo`,
`uv`, `systemctl` and `ip` on `PATH` and a local `http.server`:

- `build.sh`: layout, `VERSION`, `PACKAGE_FORMAT`, the `flmenv.sh.in` template, archive top directory and modes, the four
  client-tool assets, checksums, a failure leaves nothing.
- `setup.sh` modes: files installed per mode, absent for the other, `--join`, the `FLAME_*` settings and `.env` for the generated config, running twice on one machine (needs `--force`), the rendered `flmenv.sh` (placeholders replaced, Python
  version chosen), the Python default in the executor unit, the overwrite prompt (a yes replaces, with a restart; a no, an empty answer or no terminal leaves everything as it was, with no `systemctl` call), `--force` skipping it, and that `--uninstall` is not an option.
- Acquisition: a package directory, the directory it was extracted into, an archive with and without
  a checksum, a wrong checksum, a missing `.sha256`, an unknown `PACKAGE_FORMAT`, a URL (success,
  404), and latest-release resolution against a local redirect.
- `sdk.sh`: the four tools installed and verified (a wrong or missing checksum installs nothing), the
  SDK installed with `uv` and with `pip` (fakes), the PEP 440 version mapping, the rendered `flmenv.sh`,
  and that no `flame.yaml` is created or modified.
- Lint: `sh -n` and `shellcheck` (when available) on the scripts and `lib/common.sh`, the inlining
  step (the hosted scripts contain no `source`), and a run under `dash`.
- Missing configuration (section 3.8): unit tests in `common` and `flamepy` for the message with no file,
  no endpoint, and `FLAME_ENDPOINT` set.

## 4. Phases and acceptance

| Phase | Deliverable | Acceptance |
|---|---|---|
| 1 | `scripts/build.sh`, `scripts/setup.sh`, `scripts/sdk.sh` and their tests (flmadm untouched), **and the missing-configuration error of section 3.8 in the Rust tools and `flamepy`** | The tests pass; a package built here installs head and worker, and `sdk.sh` installs the client, on a Linux machine; a client with no configuration fails with the message and never connects to a default endpoint. |
| 2 | Callers switch: images, `Makefile`, e2e, CRI, release workflow (adds the four client-tool assets) | CI is green with `flmadm` unused; a release rehearsal publishes the assets and the smoke test passes. |
| 3 | Remove `flmadm`; update docs | `git grep flmadm` finds only history; the install instructions are the script and section 2.4. |
| 4 | Publish `setup.sh` and `sdk.sh` to `https://xflops.io/flame/` from `main` | Both `curl … \| sh` commands work on a clean VM, and a change to `scripts/` reaches the site after CI passes. |

Phases 1 and 2 can be one PR or two; phase 3 must not land before the callers are switched.

## 5. Risks

- **Parity.** `flmadm` has 65 tests and some edge handling (partial installs, prompts). The script
  ports the behaviour that matters (section 3.7 lists what is dropped); the tests above pin it.
- **POSIX `sh` portability.** Small differences between `dash`, `busybox` and bash. Mitigation: no
  bashisms, `shellcheck`, and the tests run under `dash`.
- **Pipe-to-shell.** A script run as root over the network. Mitigation: HTTPS only, the `main`
  wrapper, checksum verification before anything is installed, a pinned `--version`. The checksum is
  served next to the archive, so it protects against corruption, not a compromised host; signing the
  assets (cosign or minisign) is the follow-up.
- **Open join.** Who can join is decided by the administrator's config and the network: anyone who can
  reach the head's port can join a worker. Mitigation: document a firewall rule; a join token is the
  follow-up.
- **Script and release drift.** The hosted script updates independently of the archives. Mitigation:
  `PACKAGE_FORMAT` (the script refuses a format it does not know), tests that run the current script
  against the previous release's archive layout, and no format change without a new number.
- **No default endpoint.** A client with no configuration now fails instead of using `127.0.0.1:8080`,
  which breaks anyone relying on the old default. Mitigation: the message says exactly what to do,
  `setup.sh` and `sdk.sh` print what to configure, and the release
  notes call the change out.
- **SDK from PyPI at a pinned version.** `sdk.sh` installs `flamepy==<version>` from PyPI, which a
  release publishes before the assets are announced; a client installing in between fails until the
  upload lands. Mitigation: the release workflow publishes PyPI before it uploads the assets.
- **Glibc.** Binaries built on `ubuntu-24.04` need glibc 2.39 or newer. State the minimum in the
  release notes.
- **Network at install.** `flamepy`'s dependencies come from PyPI at install time. An offline
  install needs a mirror configured for `uv` (`UV_INDEX_URL`); document it.

## 6. Decisions

Settled in review:

1. **Rerunning.** If anything the install would write exists, `setup.sh` (and `sdk.sh`) asks whether to
   overwrite; `--force` answers yes. With no terminal to ask on they stop with an error, and a no
   changes nothing; a yes replaces everything and restarts running units.
2. **Hosting.** A job in the release workflow pushes the scripts to `xflops/xflops.github.io` (the
   `XFLOPS_SITE_TOKEN` secret, a token with write access to that repository, must exist).
3. **Versions.** Every release publishes its own scripts, whose default version is that release.
4. **Python versions.** `sdk.sh` installs `flamepy` for every requested Python (default 3.11 to 3.14);
   `flmenv.sh` selects the right one when sourced.
5. **Cache and client config.** `setup.sh` generates `flame-cluster.yaml` from `FLAME_*`/`.env` with
   defaults; `sdk.sh` generates `flame.yaml` from the same sources with **no defaults** (nothing without
   `FLAME_ENDPOINT`), so a client never guesses a cluster.
6. `setup.sh` installs only the head, with no object cache and no `flmctl`; a worker is `--join`, which
   installs the executor manager and the object cache; `setup.sh` generates the cluster config from `FLAME_*`.

Still open: the retention of old versioned scripts on the site (they are kept; nothing removes them).

## References

- Issue [#683](https://github.com/xflops/flame/issues/683)
- `flmadm/` (the code being replaced), `docs/release-process.md`
- PR #679 (default Python version), `docs/designs/RFE565-gpu-selection`

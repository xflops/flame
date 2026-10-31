# Installing Flame

Three scripts install Flame; they live in this directory. Every release publishes its own `setup.sh` and
`sdk.sh` on `https://xflops.io`: `https://xflops.io/flame/setup.sh` and `.../sdk.sh` are the latest
release's and install it by default, and `https://xflops.io/flame/<tag>/setup.sh` (for example
`.../flame/v0.7.0/setup.sh`) is the script of that release, which installs that release. The design is in [`docs/designs/RFE683-script-install/FS.md`](../docs/designs/RFE683-script-install/FS.md).

| Script | Use it to |
|---|---|
| `setup.sh` | install a cluster node: the head node, or a worker that joins it |
| `sdk.sh` | install the client: `flmctl`, `flmping`, `flmexec`, `flmbench` and the Python SDK `flamepy` |
| `build.sh` | build the release package from a source tree |

## A cluster

```shell
# the head node: the session manager
curl -fsSL https://xflops.io/flame/setup.sh | sudo sh

# each worker: the executor manager and the object cache (set FLAME_ENDPOINT to the head's address)
curl -fsSL https://xflops.io/flame/setup.sh | sudo FLAME_ENDPOINT=http://HEAD:8080 sh -s -- --join

# one machine: both (the two runs share one prefix and one config, so the second one needs --force)
curl -fsSL https://xflops.io/flame/setup.sh | sudo sh
curl -fsSL https://xflops.io/flame/setup.sh | sudo sh -s -- --join --force
```

Everything after `sh -s --` goes to the script. `setup.sh` downloads its release, verifies its `.sha256`,
installs it to `/usr/local/flame`, and enables the systemd units. If anything it would write
already exists it asks whether to overwrite (it reads the answer from the terminal, so it works when piped
from curl; with no terminal it stops with an error and changes nothing); `--force` replaces an existing
installation without asking, restarting its units.

`setup.sh` generates `<prefix>/conf/flame-cluster.yaml` from `FLAME_*` settings, read from the
environment and from a `.env` file in the current directory (the environment wins), with these defaults:

| Setting | Default |
|---|---|
| `FLAME_CLUSTER_NAME` | `flame` |
| `FLAME_ENDPOINT` | `http://127.0.0.1:8080` |
| `FLAME_RESREQ` | `cpu=1,mem=2g` |
| `FLAME_POLICIES` | `priority drf das` |
| `FLAME_STORAGE` | `fs://<prefix>/data` |
| `FLAME_SHIMS` | `host` |
| `FLAME_MAX_EXECUTORS` | `128` |
| `FLAME_CACHE_ENDPOINT` | `grpc://127.0.0.1:9090` |
| `FLAME_CACHE_ADVERTISE` | `lo` |
| `FLAME_CACHE_STORAGE` | `<prefix>/data/cache` |

For a real cluster set at least `FLAME_ENDPOINT` (the head's address), `FLAME_CACHE_ENDPOINT` and
`FLAME_CACHE_ADVERTISE` (the worker's interface), or edit the file afterwards; the site administrator owns
the cluster configuration, which has many more settings than these.

| Option | Meaning |
|---|---|
| `--join` | install a worker (the executor manager and the object cache) instead of the head node |
| `--prefix DIR` | installation directory (default `/usr/local/flame`) |
| `--package DIR\|FILE\|URL` | install this package (a directory, a `.tar.gz`, or the URL of one) instead of downloading |
| `--version TAG` | the release to install (`FLAME_VERSION` is the same) |
| `--python-version V` | the Python versions to install `flamepy` for (repeatable; default 3.11 to 3.14) |
| `--no-systemd` | do not write units or start anything (containers, image builds) |
| `--force` | replace an installation that is already there without asking (without it, `setup.sh` asks, and stops when there is no terminal) |

`FLAME_BASE_URL` points at a mirror of the release assets. Anyone who can reach the head's port can join a
worker to it through the configuration you write, so restrict it with a firewall.

## Removing Flame

There is no uninstall script; removing Flame is stopping its services and deleting its files. On a node
installed by `setup.sh` (use the `--prefix` you installed with, `/usr/local/flame` by default):

```shell
# stop and remove the services (the ones the node has; a head node has only the session manager)
sudo systemctl disable --now flame-executor-manager flame-object-cache flame-session-manager
sudo rm -f /etc/systemd/system/flame-executor-manager.service \
           /etc/systemd/system/flame-object-cache.service \
           /etc/systemd/system/flame-session-manager.service
sudo systemctl daemon-reload

# keep a copy of what you want first: data/ (sessions, the cache), conf/ and logs/ are in the prefix
sudo cp -R /usr/local/flame/conf /usr/local/flame/data ~/flame-backup     # optional
sudo rm -rf /usr/local/flame
```

A client installed by `sdk.sh` is its prefix: `rm -rf ~/.flame` (this also deletes your `flame.yaml`).

## A client

```shell
curl -fsSL https://xflops.io/flame/sdk.sh | sh
source ~/.flame/sbin/flmenv.sh
```

It installs the four tools and `flamepy` (the matching version, from PyPI) for every Python version
requested (`--python-version`, repeatable; default 3.11 to 3.14, those that exist) to `~/.flame`
(`--prefix`, `--version`; `--force` replaces an existing client without asking, as for `setup.sh`). `source ~/.flame/sbin/flmenv.sh`
selects the `flamepy` that matches your `python3`. It writes `<prefix>/flame.yaml` only when `FLAME_ENDPOINT` (and optionally `FLAME_CACHE_ENDPOINT`) is set, in the
environment or in `./.env`, and has no defaults: without it, no configuration is written, and the site administrator gives users a `~/.flame/flame.yaml`,

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://HEAD:8080"
    cache:
      endpoint: "grpc://WORKER:9090"
```

or, for the tools other than `flmctl` and for `flamepy`, export `FLAME_ENDPOINT` and
`FLAME_CACHE_ENDPOINT`. Flame never guesses an endpoint: without a configuration the tools and the SDK
fail with a message that says what to do.

## From a source tree

```shell
scripts/build.sh --output dist          # builds everything: dist/package, the archive, the tools
dist/package/sbin/setup.sh              # the package carries its own setup.sh and sdk.sh
dist/package/sbin/setup.sh --join
dist/package/sbin/sdk.sh
```

`make package`, `make install` and `make install-dev` wrap these. `build.sh` needs `cargo` and `uv`.
The release publishes `flame-<tag>-linux-<amd64|arm64>.tar.gz` and the four tools
(`<tool>-<tag>-linux-<arch>`), each with a `.sha256`.

## Tests

```shell
cd scripts/tests && python3 -m unittest discover -s . -p 'test_*.py'
```

The scripts are POSIX `sh`; `scripts/inline.sh DIR` writes the self-contained `setup.sh` and `sdk.sh` that
are hosted (`lib/common.sh` and `flmenv.sh.in` inlined), and the tests run against both forms.

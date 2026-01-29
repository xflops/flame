# Local Development with Flame

This guide explains how to set up a local Flame cluster for development and testing without Docker.

## Prerequisites

- Rust toolchain (1.70.0+)
- Python 3.8+
- pip/pip3
- Git

## Quick Start

### Using the Helper Script

The fastest way to get started is using the `local-test.sh` helper script:

```bash
# Install Flame locally
./hack/local-test.sh install

# Start Flame services
./hack/local-test.sh start

# Check service status
./hack/local-test.sh status

# Run E2E tests
./hack/local-test.sh test

# View logs
./hack/local-test.sh logs

# Stop services
./hack/local-test.sh stop

# Clean up (stop and uninstall)
./hack/local-test.sh clean
```

### Using Make Targets

You can also use Make targets directly:

```bash
# Install Flame to /tmp/flame-dev
make install-dev

# Start services manually
/tmp/flame-dev/bin/flame-session-manager --config /tmp/flame-dev/conf/flame-cluster.yaml &
/tmp/flame-dev/bin/flame-executor-manager --config /tmp/flame-dev/conf/flame-cluster.yaml &

# Run tests
make e2e-py-local

# Uninstall
make uninstall-dev
```

### Custom Installation Path

Set the `INSTALL_PREFIX` environment variable to customize the installation location:

```bash
export INSTALL_PREFIX=$HOME/flame
make install-dev
```

## Manual Installation

### Step 1: Build Flame

Build all components in release mode:

```bash
cargo build --release
```

### Step 2: Install with flmadm

Install Flame to a local directory (no root required):

```bash
./target/release/flmadm install \
    --src-dir . \
    --skip-build \
    --no-systemd \
    --prefix /tmp/flame-dev
```

This will:
- Create directory structure at `/tmp/flame-dev`
- Install binaries (session-manager, executor-manager, flmctl, flmadm)
- Install Python SDK
- Generate default configuration

### Step 3: Start Services

Start the session manager:

```bash
/tmp/flame-dev/bin/flame-session-manager \
    --config /tmp/flame-dev/conf/flame-cluster.yaml \
    > /tmp/flame-dev/logs/fsm.log 2>&1 &
```

Wait a few seconds, then start the executor manager:

```bash
/tmp/flame-dev/bin/flame-executor-manager \
    --config /tmp/flame-dev/conf/flame-cluster.yaml \
    > /tmp/flame-dev/logs/fem.log 2>&1 &
```

### Step 4: Verify Installation

Check that services are running:

```bash
ps aux | grep flame
```

Test with flmctl:

```bash
/tmp/flame-dev/bin/flmctl list
```

## Running Tests

### Python E2E Tests

Run Python E2E tests against your local cluster:

```bash
cd e2e
FLAME_ENDPOINT=http://127.0.0.1:8080 uv run pytest -vv .
```

Or use the Make target:

```bash
make e2e-py-local
```

### Rust Tests

Run Rust tests:

```bash
make e2e-rs
```

## Development Workflow

### Typical Development Cycle

1. **Make code changes** to Flame components
2. **Rebuild**:
   ```bash
   cargo build --release
   ```
3. **Stop services**:
   ```bash
   ./hack/local-test.sh stop
   ```
4. **Reinstall**:
   ```bash
   ./target/release/flmadm install \
       --src-dir . \
       --skip-build \
       --no-systemd \
       --prefix /tmp/flame-dev \
       --clean
   ```
5. **Start services**:
   ```bash
   ./hack/local-test.sh start
   ```
6. **Test changes**:
   ```bash
   ./hack/local-test.sh test
   ```

### Quick Reinstall

For faster iteration, use the helper script:

```bash
# Stop, rebuild, reinstall, and restart
./hack/local-test.sh stop
cargo build --release
make install-dev
./hack/local-test.sh start
```

## Viewing Logs

Service logs are written to:
- Session Manager: `/tmp/flame-dev/logs/fsm.log`
- Executor Manager: `/tmp/flame-dev/logs/fem.log`

View logs with:

```bash
# Using the helper script
./hack/local-test.sh logs

# Or directly
tail -f /tmp/flame-dev/logs/fsm.log
tail -f /tmp/flame-dev/logs/fem.log
```

## Configuration

The default configuration is generated at `/tmp/flame-dev/conf/flame-cluster.yaml`:

```yaml
---
cluster:
  name: flame
  endpoint: "http://127.0.0.1:8080"
  slot: "cpu=1,mem=2g"
  policy: proportion
  storage: "sqlite:///tmp/flame-dev/data/sessions.db"
executors:
  shim: host
  limits:
    max_executors: 128
cache:
  endpoint: "grpc://127.0.0.1:9090"
  network_interface: "lo"
  storage: "/tmp/flame-dev/data/cache"
```

You can edit this file to customize your local cluster.

## Troubleshooting

### Services Won't Start

Check the logs for errors:

```bash
cat /tmp/flame-dev/logs/fsm.log
cat /tmp/flame-dev/logs/fem.log
```

Common issues:
- Port already in use (check with `lsof -i :8080` and `lsof -i :8081`)
- Configuration file issues
- Missing dependencies

### Port Conflicts

If ports 8080/8081 are in use, edit the configuration file to use different ports:

```bash
vim /tmp/flame-dev/conf/flame-cluster.yaml
# Change endpoint ports
# Restart services
```

### Tests Fail to Connect

Ensure:
1. Services are running: `./hack/local-test.sh status`
2. Correct endpoint: `export FLAME_ENDPOINT=http://127.0.0.1:8080`
3. Firewall allows local connections

### Clean Start

For a completely clean start:

```bash
./hack/local-test.sh clean
./hack/local-test.sh install
./hack/local-test.sh start
```

## Cleanup

To remove your local Flame installation:

```bash
# Stop services
./hack/local-test.sh stop

# Uninstall
make uninstall-dev

# Or using flmadm directly
./target/release/flmadm uninstall \
    --prefix /tmp/flame-dev \
    --no-backup \
    --force
```

## CI/CD Integration

The Python E2E CI workflow now uses `flmadm` for installation instead of Docker Compose. See `.github/workflows/e2e-py.yaml` for the implementation.

Key steps in CI:
1. Build Flame with `cargo build --release`
2. Install with `flmadm install --no-systemd`
3. Start services in background
4. Run tests with `uv run pytest`
5. Cleanup with `flmadm uninstall`

## Comparison: Docker vs Local

### Docker Compose (Traditional)

**Pros:**
- Isolated environment
- Consistent across machines
- No host system changes

**Cons:**
- Slower iteration (rebuild images)
- More resource intensive
- Harder to debug

**Use for:**
- Integration testing
- Multi-node testing
- Production-like environment

### Local Installation (New)

**Pros:**
- Faster iteration (no image rebuilds)
- Easy debugging with logs
- Native performance
- Direct access to binaries

**Cons:**
- Installs on host system
- May have port conflicts
- Single-node only

**Use for:**
- Development
- Unit/E2E testing
- Debugging
- CI/CD (faster)

## See Also

- [flmadm README](../../flmadm/README.md) - Administration tool documentation
- [RFE333 Functional Specification](../designs/RFE333-flmadm/FS.md) - Design details
- [AGENTS.md](../../AGENTS.md) - Development guidelines

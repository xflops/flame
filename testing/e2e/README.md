# Flame E2E package

This package contains the services and shared types used by Flame's E2E tests.

The test fixtures deploy this directory with `flmctl deploy`. Its
`flame.yaml` supplies the default Python service command, and each fixture
overrides the application name. The executor downloads and installs the
package from object cache; it does not need an E2E source mount.

## Instance lifecycle

The lifecycle tests deploy a serving process, verify setup finishes before its
first session enters, reuse the same process for two sessions, and verify
teardown when its executor is released. Events are written to a separate,
registered observer application's cache object and read after the worker exits.
The two cases use immediate setup and a real 31-second setup, exceeding the
connection timeout.

Run from the repository root with the current checkout's Python SDK and E2E
package importable, `pytest` and `pytest-timeout` installed, and the current
`flmctl` on `PATH`:

```bash
PYTHONPATH=testing/e2e/src python -m pytest testing/e2e/tests/test_serving.py -q
```

Host, CRI, and Kubernetes CI suites include these cases. The tests reuse the
`flmrun` application's configured shim/image when available, and otherwise use
the default Host shim.

The cluster must run the updated session/executor managers and object cache.
Configure the client endpoints through the normal Flame configuration or
`FLAME_ENDPOINT` and `FLAME_CACHE_ENDPOINT`; the cache address must also be
reachable from the executor. The Python installer on the worker must use the
updated SDK. The tests use ordinary CPU capacity and need no GPU.

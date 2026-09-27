# Flame E2E package

This package contains the services and shared types used by Flame's E2E tests.

The test fixtures deploy this directory with `flmctl deploy`. Its
`flame.yaml` supplies the default Python service command, and each fixture
overrides the application name. The executor downloads and installs the
package from object cache; it does not need an E2E source mount.

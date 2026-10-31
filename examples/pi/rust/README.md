# Flame Rust Pi Example

This example estimates pi with Monte Carlo sampling on Flame. The client starts a Flame session, submits multiple typed tasks, and combines the number of sampled points that landed inside the unit circle. The service receives each typed request and performs the random sampling for that task.

Deploy the service binary from the installed examples directory:

```bash
source /usr/local/flame/sbin/flmenv.sh
flmctl deploy \
  --name pi \
  --application ./target/release/pi-service
```

Run the client with the same application name:

```bash
./target/release/pi --app pi
```

`flmctl deploy` uploads the service binary package to object cache and registers the `pi` application so executors download the package instead of relying on a local worker-image path. Use another `--name` value when deploying and pass the same value to `pi --app <name>` for a custom registration.

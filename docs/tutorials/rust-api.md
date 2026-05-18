# Rust API Tutorial

This tutorial walks through the Rust API by building, deploying, and running the existing Rust Pi example. The example uses typed request and response structs, a Rust service entrypoint, and an async Rust client that submits many Flame tasks in parallel.

## Prerequisites

- A running Flame cluster with the session manager, executor manager, and object cache.
- `flmctl` on `PATH`.
- Rust toolchain from this repository.
- Flame client configuration in `~/.flame/flame.yaml` or environment variables.

A minimal local client configuration is:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://127.0.0.1:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
```

If Flame was installed with `flmadm`, source the generated environment file before running the commands:

```bash
source /usr/local/flame/sbin/flmenv.sh
```

For a source-tree local cluster, follow [Local Development](local-development.md) first.

## Project Layout

The tutorial uses `examples/pi/rust`:

```text
examples/pi/rust/
  Cargo.toml
  src/api.rs
  src/service.rs
  src/client.rs
```

`Cargo.toml` defines two binaries:

- `pi-service`: the Flame service that executors run.
- `pi`: the client that creates a session and submits tasks.

The example depends on `flame-rs` with the `macros` feature enabled:

```toml
flame-rs = { path = "../../../sdk/rust", features = ["macros"] }
```

## Define Typed Payloads

The Rust SDK uses `FlameMessage` to encode task input, task output, and common data. The Pi example defines request and response types in [api.rs](../../examples/pi/rust/src/api.rs):

```rust
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, flame_rs::FlameMessage)]
pub struct PiRequest {
    pub samples: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, flame_rs::FlameMessage)]
pub struct PiResponse {
    pub inside: u32,
}
```

`#[derive(flame_rs::FlameMessage)]` serializes the payloads with JSON. When the client submits a `PiRequest`, the SDK encodes it before creating the task. When the task finishes, the SDK decodes the returned bytes into `PiResponse`.

## Write The Service

The service in [service.rs](../../examples/pi/rust/src/service.rs) is a normal async Rust function annotated with `#[flame::entrypoint]`:

```rust
use flame_rs::{self as flame, apis::FlameError};

use api::{PiRequest, PiResponse};

#[flame::entrypoint]
async fn estimate_pi(input: PiRequest) -> Result<PiResponse, FlameError> {
    let mut inside = 0u32;

    for _ in 0..input.samples {
        // Work happens here.
    }

    Ok(PiResponse { inside })
}
```

The entrypoint macro adapts the typed function to the Flame host-shim service protocol. The service binary starts the SDK runtime with `flame::run(...)`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::run(estimate_pi).await?;
    Ok(())
}
```

Inside an executor, Flame sets `FLAME_INSTANCE_ENDPOINT`. The Rust SDK binds a Unix domain socket at that endpoint and receives session and task callbacks from the executor manager.

## Write The Client

The client in [client.rs](../../examples/pi/rust/src/client.rs) creates a Flame session for the deployed application name:

```rust
use futures::future::try_join_all;
use flame_rs as flame;
use flame_rs::client::SessionOptions;

let ssn = flame::create_session(SessionOptions::new(cli.app.clone())).await?;
```

The application name must match the name passed to `flmctl deploy`. The client then submits many tasks:

```rust
let request = PiRequest {
    samples: cli.task_input,
};

let handles =
    try_join_all((0..cli.task_num).map(|_| ssn.run::<_, PiResponse>(&request))).await?;
let tasks = try_join_all(handles).await?;
```

`Session::run()` creates a task and returns `Result<TaskFuture<PiResponse>, FlameError>`, so the first `try_join_all` handles task creation errors. Each `TaskFuture<PiResponse>` then resolves to `Result<TaskResult<PiResponse>, FlameError>`, so the second `try_join_all` handles SDK-level wait or decode errors. Remote task failures are still represented inside each `TaskResult` and are checked by `count_inside()`.

For a single task where failures should become `FlameError`, use `invoke()`:

```rust
let handle = ssn.invoke::<_, PiResponse>(&request).await?;
let output = handle.await?.expect("service returned no output");
```

Close the session after submitting all work:

```rust
ssn.close().await?;
```

Closing prevents new task submissions and lets Flame release executors after the session drains.

## Build The Example

From the repository root:

```bash
cargo build -p pi --release
```

This produces:

- `target/release/pi-service`
- `target/release/pi`

## Deploy The Service

Deploy the service binary as an application named `pi`:

```bash
flmctl deploy \
  --name pi \
  --application target/release/pi-service
```

`flmctl deploy` packages the service binary, uploads it to object cache, and registers the application. Confirm the registration:

```bash
flmctl list -a
```

The output should include an enabled application named `pi`.

## Run The Client

Run the client with the same application name:

```bash
cargo run -p pi --bin pi -- \
  --app pi \
  --task-num 10 \
  --task-input 10000
```

The client prints the estimated value:

```text
pi = 4*(78542/100000) = 3.14168
```

Actual values vary because the service uses random sampling.

Inspect the session:

```bash
flmctl list -s
```

The session should eventually show all tasks in `Succeed`.

## Add Session Options

`SessionOptions` configures scheduling and task placement. For example:

```rust
let ssn = flame::create_session(
    SessionOptions::new(cli.app.clone())
        .min_instances(1)
        .max_instances(4)
        .batch_size(1)
        .priority(10)
        .resreq("cpu=1,mem=1g"),
)
.await?;
```

Common fields:

| Field | Meaning |
|-------|---------|
| `min_instances` | Minimum executors to keep for the session |
| `max_instances` | Maximum executors the scheduler may allocate |
| `batch_size` | Executors to allocate per scheduling batch |
| `priority` | Session priority used by priority scheduling |
| `resreq` | Resource request such as `cpu=1,mem=1g,gpu=0` |

Use typed common data when every task in the session needs the same configuration:

```rust
let options = SessionOptions::new(cli.app.clone())
    .common_data(&shared_config)?;
let ssn = flame::create_session(options).await?;
```

Services can read common data from `FlameInstance` when using `#[flame::instance]`, or from `SessionContext` when implementing `FlameService` directly.

## API Summary

| Goal | API |
|------|-----|
| Connect with default config | `flame_rs::connect()` |
| Create a session | `flame_rs::create_session(SessionOptions::new(app))` |
| Submit a task and keep full task status | `Session::run::<_, Output>(&input)` |
| Submit a task and get successful output | `Session::invoke::<_, Output>(&input)` |
| Close a session | `Session::close()` |
| Define typed payloads | `#[derive(flame_rs::FlameMessage)]` |
| Define a function service | `#[flame::entrypoint]` and `flame::run(handler)` |
| Store shared objects | `flame_rs::put_object()`, `get_object()`, `update_object()` |

## Troubleshooting

`flmctl deploy` cannot connect: verify `FLAME_ENDPOINT` and `FLAME_CACHE_ENDPOINT`, then confirm the session manager and object cache are running.

Client fails with application not found: verify `flmctl list -a` includes the same application name passed to `--app`.

Tasks remain pending: check executor-manager logs and verify the cluster has available resources for the session `resreq`.

Service exits immediately outside Flame: `pi-service` expects `FLAME_INSTANCE_ENDPOINT`, which is set by the executor manager. Run the client locally, not the service binary directly, after deploying the service.

Typed decode errors: make sure the client and service use matching `FlameMessage` request and response types.

## See Also

- [Rust SDK](../sdk/rust.md)
- [Rust Pi example](../../examples/pi/rust/README.md)
- [Local Development](local-development.md)
- [Frontend API reference](../api/frontend.md)

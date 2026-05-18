# Flame Rust SDK

The Rust SDK lives in this repository as the `flame-rs` crate. It provides:

- Async client APIs for sessions, tasks, applications, nodes, and executors.
- Typed task payloads through `FlameMessage`.
- Service helpers for host-shim applications.
- Optional macros for concise typed service entrypoints.
- Object-cache helpers for typed objects and files.

## Add The Dependency

Inside this repository, use a path dependency and enable the `macros` feature when you want typed service macros:

```toml
[dependencies]
flame-rs = { path = "sdk/rust", features = ["macros"] }
```

Adjust the path for nested examples. The [Rust Pi example](../../examples/pi/rust/Cargo.toml) uses:

```toml
flame-rs = { path = "../../../sdk/rust", features = ["macros"] }
```

## Configure A Client

The top-level helpers read `~/.flame/flame.yaml` and apply `FLAME_ENDPOINT`, `FLAME_CACHE_ENDPOINT`, and `FLAME_CA_FILE` overrides:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://127.0.0.1:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
```

Use `flame_rs::connect_with_config(Some(path))` or `flame_rs::connect_with_context(context)` when a process needs an explicit config source.

## Define Typed Messages

With the `macros` feature enabled, derive `FlameMessage` for request, response, and common-data types:

```rust
use flame_rs::FlameMessage;
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, FlameMessage)]
struct EstimateRequest {
    samples: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, FlameMessage)]
struct EstimateResponse {
    inside: u32,
}
```

The derive serializes with JSON. For custom formats, implement `flame_rs::FlameMessage` directly with `encode()` and `decode()`.

## Create Sessions And Run Tasks

Use `create_session()` when the session should be new, `open_session()` for an existing session, and `open_or_create_session()` when repeated clients may attach to the same session.

```rust
use flame_rs as flame;
use flame_rs::client::SessionOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::apis::init_logger()?;

    let session = flame::create_session(
        SessionOptions::new("pi")
            .min_instances(1)
            .resreq("cpu=1,mem=1g"),
    )
    .await?;

    let input = EstimateRequest { samples: 10_000 };
    let handle = session.invoke::<_, EstimateResponse>(&input).await?;
    let output = handle.await?.expect("service returned no output");

    println!("inside = {}", output.inside);
    session.close().await?;

    Ok(())
}
```

For bulk submission, `Session::run()` returns a `TaskFuture<O>`. Awaiting it returns `Result<TaskResult<O>, FlameError>`: `Ok` contains task ID, session ID, terminal state, decoded output, and task-level error details, while `Err` reports SDK-level failures such as watch or decode errors. `Session::invoke()` converts that result into a simpler handle that resolves to successful output or `FlameError`.

## Write A Service

The shortest Rust service is a typed async function annotated with `#[flame::entrypoint]`:

```rust
use flame_rs::{self as flame, apis::FlameError};

#[flame::entrypoint]
async fn estimate(input: EstimateRequest) -> Result<EstimateResponse, FlameError> {
    Ok(EstimateResponse {
        inside: input.samples,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::run(estimate).await
}
```

For stateful services, annotate an inherent `impl` block with `#[flame::instance]` and mark exactly one method as the task entrypoint:

```rust
use std::sync::Mutex;

use flame_rs::{self as flame, apis::FlameError};
use flame_rs::service::FlameInstance;
use serde_derive::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, flame::FlameMessage)]
struct Factor {
    factor: u32,
}

#[derive(Clone, Serialize, Deserialize, flame::FlameMessage)]
struct Number {
    value: u32,
}

#[derive(Default)]
struct Multiplier {
    factor: Mutex<u32>,
}

#[flame::instance]
impl Multiplier {
    async fn enter(&self, instance: FlameInstance) -> Result<(), FlameError> {
        let factor = instance
            .common_data::<Factor>()?
            .map(|value| value.factor)
            .unwrap_or(1);
        *self.factor.lock().unwrap() = factor;
        Ok(())
    }

    #[flame::entrypoint]
    async fn multiply(&self, input: Number) -> Result<Number, FlameError> {
        let factor = *self.factor.lock().unwrap();
        Ok(Number {
            value: input.value * factor,
        })
    }

    async fn leave(&self) -> Result<(), FlameError> {
        Ok(())
    }
}
```

By default the instance macro looks for lifecycle hooks named `enter` and `leave`. Implement `FlameService` directly when you need byte-level task control.

## Use Object Cache

Object keys use either a prefix, `<app>/<session>`, or a full key, `<app>/<session>/<object>`.

```rust
use flame_rs as flame;

let reference = flame::put_object("pi/shared", &model_config).await?;
let loaded: ModelConfig = reference.get().await?;

let updated = flame::update_object(&reference, &next_model_config).await?;
let loaded_again: ModelConfig = flame::get_object(updated).await?;
```

Use `patch_object()` for versioned delta updates, `upload_object()` and `download_object()` for files, and `delete_objects()` to remove an object or prefix. `ObjectRef` carries the cache endpoint, full key, and object version.

## Deploy And Run

Build the service binary, deploy it with `flmctl deploy`, then use the same application name from the client:

```bash
cargo build -p pi --release
flmctl deploy --name pi --application target/release/pi-service
cargo run -p pi --bin pi -- --app pi
```

Use the exact deployed application name in `SessionOptions::new(...)`.

## API Map

| Area | Rust API |
|------|----------|
| Connect | `flame_rs::connect()`, `connect_with_config()`, `connect_with_context()` |
| Sessions | `create_session()`, `open_session()`, `open_or_create_session()`, `SessionOptions` |
| Tasks | `Session::run()`, `Session::invoke()`, `Session::create_task()`, `Session::watch_task()` |
| Services | `flame_rs::run()`, `FlameService`, `#[flame::entrypoint]`, `#[flame::instance]` |
| Messages | `FlameMessage`, `IntoTaskInput`, `FromTaskOutput`, `IntoCommonData` |
| Objects | `put_object()`, `get_object()`, `update_object()`, `patch_object()`, `upload_object()`, `download_object()` |

See also:

- [Rust API tutorial](../tutorials/rust-api.md)
- [Rust Pi example](../../examples/pi/rust/README.md)
- [Rust Candle Based example](../../examples/candle/based/README.md)
- [Frontend API reference](../api/frontend.md)

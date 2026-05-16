# RFE455: Simplify flame-rs API for Users

GitHub issue: https://github.com/xflops/flame/issues/455

## Summary

This report proposes a higher-level Rust SDK API for Flame. The proposal covers two related surfaces:

- Client helpers for creating sessions and invoking typed tasks with minimal boilerplate.
- Service macros for defining typed service entrypoints while preserving the existing `FlameService` runtime boundary.

The design centers on one typed payload contract, `FlameMessage`, for task input, task output, and common data. Raw byte workflows remain supported by the existing low-level Rust APIs and are intentionally out of scope for this simplified layer.

## 1. Motivation

**Background:**

The current Rust SDK exposes mostly core APIs. Client code usually has to load `FlameContext`, connect manually, construct `SessionAttributes`, create a session through `Connection`, and then wire task creation/watching through `TaskInformer`. Service code similarly implements `flame_rs::service::FlameService` directly.

These APIs are explicit and stable, but simple applications repeat the same adapter code:

- load config and connect before creating a session
- build `SessionAttributes` for common session defaults
- create a task, watch it, and collect output for synchronous invocation
- implement a custom `TaskInformer` for one-shot task output
- import `tonic::async_trait` on the service side
- implement `on_session_enter`, `on_task_invoke`, and `on_session_leave`
- manually handle missing task input and encode/decode task bytes

This pattern appears in the Rust Pi example, `flmping`, and model-serving examples. In those applications, the core business logic is small: create a session, invoke a task, load state on session enter, and run inference on task invoke. The surrounding adapter code should be optional for common typed use cases.

**Target:**

Add a higher-level `flame-rs` API layer similar to FlamePy:

- top-level client helpers such as `flame_rs::create_session(...)`
- session methods such as `session.invoke(...)` over `FlameMessage` payloads
- async task handles for parallel invocation
- one `FlameMessage` derive macro for typed input, output, and common-data serialization
- service macros that give Rust services the same high-level shape as `flamepy.service.FlameInstance`

Success criteria:

- Existing `FlameService` implementations continue to compile unchanged.
- A basic Rust client can create a session and synchronously invoke a task without manually creating a connection or `TaskInformer`.
- Typed request/response/common-data clients can avoid repeated byte serialization boilerplate.
- A simple typed request/response service can be written as one annotated entrypoint function, without importing `tonic`, manually parsing task bytes, or writing lifecycle no-op methods.
- Stateful services can still load and clear resources in session hooks.
- Raw byte workflows remain available through the existing low-level Rust APIs, outside this simplified layer.
- Macro expansion is easy to reason about and maps directly to the existing service trait.

## 2. Functional Specification

**Configuration:**

Client helpers and the runtime `FlameMessage` trait are part of the default `flame-rs` API because they only wrap existing runtime dependencies.

Add an optional `macros` feature for the `FlameMessage` derive macro and service-side proc macros:

```toml
flame-rs = { path = "../../sdk/rust", features = ["macros"] }
```

The feature enables a new proc-macro crate and re-exports:

- `#[derive(flame_rs::FlameMessage)]` for input, output, and common-data payload types.
- `#[flame_rs::entrypoint(...)]` for free-function services.
- `#[flame_rs::instance(...)]` for stateful struct-backed services.
- `flame_rs::run(...)` as the high-level async runner used from the user's `main` function.

Free-function entrypoint:

```rust
#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct InferRequest {
    prompt: String,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct InferResponse {
    text: String,
}

#[flame_rs::entrypoint]
async fn infer(req: InferRequest) -> Result<InferResponse, FlameError> {
    ...
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame_rs::run(infer).await
}
```

Stateful instance:

```rust
use std::sync::Mutex;
use flame_rs::service::FlameInstance;

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct ModelContext {
    model_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct InferRequest {
    prompt: String,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct InferResponse {
    text: String,
}

#[derive(Default)]
struct ModelInstance {
    model: Mutex<Option<Model>>,
}

#[flame_rs::instance]
impl ModelInstance {
    async fn enter(&self, instance: FlameInstance) -> Result<(), FlameError> {
        let ctx = instance.common_data::<ModelContext>()?;
        ...
    }

    #[flame_rs::entrypoint]
    async fn infer(&self, req: InferRequest) -> Result<InferResponse, FlameError> {
        ...
    }

    async fn leave(&self) -> Result<(), FlameError> {
        ...
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame_rs::run(ModelInstance::default()).await
}
```

Fields:

- `entrypoint`: optional method name override for `#[flame_rs::instance]`. The normal path is to mark the method with `#[flame_rs::entrypoint]`.
- `enter`: optional session-enter method name. Default: `"enter"` when present; otherwise the generated hook returns `Ok(())`.
- `leave`: optional session-leave method name. Default: `"leave"` when present; otherwise the generated hook returns `Ok(())`.
- `debug`: reserved for a future local debug mode aligned with FlamePy. Disabled in the MVP.
- `crate`: optional crate path override for renamed dependencies. Default: `flame_rs`.

### Client Helper API

The low-level Rust client remains available under `flame_rs::client`. RFE455 adds a higher-level crate-root client path for the common FlamePy-style workflow:

```rust
let ssn = flame_rs::create_session("model-app").await?;
let handle = ssn.invoke(&request).await?;
let output: Option<GenerationResponse> = handle.await?;
ssn.close().await?;
```

Top-level helpers:

```rust
pub async fn connect() -> Result<Connection, FlameError>;

pub async fn connect_with_context(
    context: FlameContext,
) -> Result<Connection, FlameError>;

pub async fn connect_with_config(
    path: Option<String>,
) -> Result<Connection, FlameError>;

pub async fn create_session(
    options: impl Into<SessionOptions>,
) -> Result<Session, FlameError>;

pub async fn open_session(
    id: impl Into<SessionID>,
) -> Result<Session, FlameError>;

pub async fn open_or_create_session(
    id: impl Into<SessionID>,
    options: impl Into<SessionOptions>,
) -> Result<Session, FlameError>;
```

`connect()` loads `FlameContext::from_file_with_env(None)`, reads the current context's cluster endpoint and TLS config, and delegates to the existing `client::connect_with_tls(...)`. This mirrors FlamePy's default connection behavior without introducing a global singleton in the MVP. Applications that need connection reuse can call `connect()` once and keep the returned `Connection`.

Connection-level mirrors avoid reconnecting in long-running clients:

```rust
impl Connection {
    pub async fn create_session_with(
        &self,
        options: impl Into<SessionOptions>,
    ) -> Result<Session, FlameError>;

    pub async fn open_or_create_session_with(
        &self,
        id: impl Into<SessionID>,
        options: impl Into<SessionOptions>,
    ) -> Result<Session, FlameError>;
}
```

For `open_or_create_session(...)`, `id` is authoritative. If `SessionOptions::id` is present and differs from `id`, return `FlameError::InvalidConfig`.

`SessionOptions` is the ergonomic builder form of the current `SessionAttributes`:

```rust
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub id: Option<SessionID>,
    pub application: String,
    pub common_data: Option<CommonData>,
    pub min_instances: u32,
    pub max_instances: Option<u32>,
    pub batch_size: u32,
    pub priority: u32,
    pub resreq: Option<ResourceRequirement>,
}

impl SessionOptions {
    pub fn new(application: impl Into<String>) -> Self;
    pub fn id(self, id: impl Into<SessionID>) -> Self;
    pub fn common_data(self, data: impl IntoCommonData) -> Result<Self, FlameError>;
    pub fn min_instances(self, value: u32) -> Self;
    pub fn max_instances(self, value: u32) -> Self;
    pub fn batch_size(self, value: u32) -> Self;
    pub fn priority(self, value: u32) -> Self;
    pub fn resreq(self, value: impl Into<ResourceRequirement>) -> Self;
}

impl From<&str> for SessionOptions;
impl From<String> for SessionOptions;
impl From<SessionOptions> for SessionAttributes;
```

Defaults match FlamePy and the current Rust examples: a generated session ID using `<application>-<short_name>`, `min_instances = 0`, `max_instances = None`, `batch_size = 1`, `priority = 0`, and no explicit resource requirement.

Payload serialization uses one message trait for task input, task output, and common data:

```rust
pub trait FlameMessage: Sized {
    fn encode(&self) -> Result<bytes::Bytes, FlameError>;
    fn decode(bytes: &[u8]) -> Result<Self, FlameError>;
}
```

Users derive it once on each typed payload:

```rust
#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationRequest {
    prompt: String,
    sample_len: u32,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationResponse {
    text: String,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationContext {
    system_prompt: String,
}
```

The derive macro generates serialization code for the `FlameMessage` trait. The MVP should use `serde_json` internally for portability and debuggability. This RFE does not expose a user-facing serialization-format option. Raw byte workflows should continue to use the existing low-level client and `FlameService` APIs.

High-level session methods:

```rust
impl Session {
    pub async fn invoke<I, O>(
        &self,
        input: I,
    ) -> Result<TaskHandle<O>, FlameError>
    where
        I: IntoTaskInput,
        O: FromTaskOutput + Send + 'static;

    pub async fn run<I, O>(
        &self,
        input: I,
    ) -> Result<TaskFuture<O>, FlameError>
    where
        I: IntoTaskInput,
        O: FromTaskOutput + Send + 'static;

    pub fn common_data<T>(&self) -> Result<Option<T>, FlameError>
    where
        T: FlameMessage;
}
```

`invoke(...)` creates a task and returns a `TaskHandle<O>`. Awaiting the handle waits for terminal task state, returns decoded output when the task succeeds, and returns `FlameError` when the task fails, is cancelled, or the output cannot be decoded.

`run(...)` starts a task and returns a richer future for parallel invocation:

```rust
pub struct TaskHandle<O> {
    task_id: TaskID,
    future: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<O>, FlameError>> + Send + 'static>,
    >,
}

pub struct TaskFuture<O> {
    task_id: TaskID,
    future: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TaskResult<O>, FlameError>> + Send + 'static>,
    >,
}

pub struct TaskResult<O> {
    pub task_id: TaskID,
    pub session_id: SessionID,
    pub state: TaskState,
    pub output: Option<O>,
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}

impl<O> TaskHandle<O> {
    pub fn id(&self) -> &TaskID;
}

impl<O> std::future::Future for TaskHandle<O> {
    type Output = Result<Option<O>, FlameError>;
}

impl<O> TaskFuture<O> {
    pub fn id(&self) -> &TaskID;
}

impl<O> std::future::Future for TaskFuture<O> {
    type Output = Result<TaskResult<O>, FlameError>;
}
```

`TaskFuture` is directly awaitable. Awaiting it waits for terminal task state and returns a `TaskResult<O>` containing the task ID, session ID, terminal state, decoded output on success, and error code/message on failure or cancellation. Transport, watch, and decode failures still return `FlameError`. Dropping a handle or future does not cancel the remote task in the MVP.

Input conversion is explicit and shared by `invoke(...)` and `run(...)`:

```rust
pub trait IntoTaskInput {
    fn into_task_input(self) -> Result<Option<TaskInput>, FlameError>;
}

impl<T> IntoTaskInput for &T where T: FlameMessage;
impl<T> IntoTaskInput for Option<&T> where T: FlameMessage;
impl IntoTaskInput for ();
```

Output and common-data conversion use matching traits:

```rust
pub trait FromTaskOutput: Sized {
    fn from_task_output(output: Option<TaskOutput>) -> Result<Option<Self>, FlameError>;
}

impl<T> FromTaskOutput for T where T: FlameMessage;
impl FromTaskOutput for ();

pub trait IntoCommonData {
    fn into_common_data(self) -> Result<CommonData, FlameError>;
}

impl<T> IntoCommonData for &T where T: FlameMessage;
```

The typed helpers do not mimic FlamePy's `cloudpickle` service session. Rust callers should use an explicit, portable wire format.

Client example:

```rust
use flame_rs::client::SessionOptions;

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationRequest {
    prompt: String,
    sample_len: u32,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationResponse {
    text: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ssn = flame_rs::create_session(
        SessionOptions::new("model-app")
            .min_instances(1)
            .resreq("cpu=4,mem=16g"),
    )
    .await?;

    let handle = ssn
        .invoke(&GenerationRequest {
            prompt: "Hello".to_string(),
            sample_len: 64,
        })
        .await?;
    let response: Option<GenerationResponse> = handle.await?;

    println!("{response:?}");
    ssn.close().await?;
    Ok(())
}
```

Typed common-data example:

```rust
#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct ChatContext {
    system_prompt: String,
}

let ctx = ChatContext {
    system_prompt: "Answer concisely.".to_string(),
};

let options = SessionOptions::new("chat-app")
    .min_instances(1)
    .common_data(&ctx)?;

let ssn = flame_rs::create_session(options).await?;
let restored: Option<ChatContext> = ssn.common_data()?;
```

Parallel invocation example:

```rust
let handles = futures::future::try_join_all(
    prompts
        .iter()
        .map(|prompt| ssn.run::<_, GenerationResponse>(prompt)),
)
.await?;

for result in futures::future::try_join_all(handles).await? {
    if !result.is_succeed() {
        eprintln!(
            "task {} finished with {}: {:?}",
            result.task_id,
            result.state,
            result.error_message,
        );
        continue;
    }

    let response = result.output;
    println!("{response:?}");
}
```

### Service Macro API

The stable runtime boundary remains the existing trait:

```rust
#[tonic::async_trait]
pub trait FlameService: Send + Sync + 'static {
    async fn on_session_enter(&self, _: SessionContext) -> Result<(), FlameError>;
    async fn on_task_invoke(&self, _: TaskContext) -> Result<Option<TaskOutput>, FlameError>;
    async fn on_session_leave(&self) -> Result<(), FlameError>;
}
```

The macros expand user entrypoints into generated wrappers that implement `FlameService`. Users own the service function or service type and business logic. Generated wrappers own adapter state, such as the current `SessionContext`.

This mirrors FlamePy with Rust-native syntax:

| FlamePy | Rust macro proposal |
| --- | --- |
| `instance = FlameInstance()` | annotated entrypoint function or struct-backed instance |
| `@instance.entrypoint` | `#[flame_rs::entrypoint]` |
| entrypoint has zero or one parameter | entrypoint supports zero input, required input, optional input, and optional `FlameInstance` handle |
| `instance.context()` | `FlameInstance::common_data<T>()` for `T: FlameMessage` in MVP; cache-backed context after Rust cache SDK support |
| `instance.update_context(data)` | staged for the Rust cache SDK, not part of the MVP |
| `instance.run()` | `flame_rs::run(...).await` inside the user's `main` |

Example typed service:

```rust
use flame_rs::apis::FlameError;

#[derive(Default, serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct PingRequest {
    duration: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct PingResponse {
    message: String,
}

#[flame_rs::entrypoint]
async fn ping(req: Option<PingRequest>) -> Result<PingResponse, FlameError> {
    let req = req.unwrap_or_default();
    Ok(PingResponse {
        message: format!("duration={:?}", req.duration),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame_rs::run(ping).await
}
```

Example stateful typed service:

```rust
use std::sync::Mutex;
use flame_rs::apis::FlameError;
use flame_rs::service::FlameInstance;

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationRequest {
    prompt: String,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct GenerationResponse {
    text: String,
}

#[derive(Default)]
struct ModelService {
    assets: Mutex<Option<Assets>>,
}

#[flame_rs::instance]
impl ModelService {
    async fn enter(&self, _instance: FlameInstance) -> Result<(), FlameError> {
        let mut assets = self.assets.lock().map_err(|_| {
            FlameError::Internal("model assets lock poisoned".to_string())
        })?;
        *assets = Some(load_assets()?);
        Ok(())
    }

    #[flame_rs::entrypoint]
    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, FlameError> {
        let assets = self.assets.lock().map_err(|_| {
            FlameError::Internal("model assets lock poisoned".to_string())
        })?;
        let assets = assets.as_ref().ok_or_else(|| {
            FlameError::Internal("model assets are not loaded".to_string())
        })?;
        run_generation(assets, req)
    }

    async fn leave(&self) -> Result<(), FlameError> {
        let mut assets = self.assets.lock().map_err(|_| {
            FlameError::Internal("model assets lock poisoned".to_string())
        })?;
        *assets = None;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame_rs::run(ModelService::default()).await
}
```

Supported entrypoint signatures:

```rust
async fn invoke(&self) -> Result<O, FlameError>
async fn invoke(&self, input: I) -> Result<O, FlameError>
async fn invoke(&self, input: Option<I>) -> Result<O, FlameError>
async fn invoke(&self, instance: FlameInstance) -> Result<O, FlameError>
async fn invoke(&self, instance: FlameInstance, input: I) -> Result<O, FlameError>
async fn invoke(&self, instance: FlameInstance, input: Option<I>) -> Result<O, FlameError>
```

For free-function entrypoints, the same signatures are supported without `&self`.

`FlameInstance` is a lightweight runtime handle passed by the generated adapter:

```rust
pub struct FlameInstance {
    session: SessionContext,
}

impl FlameInstance {
    pub fn session_id(&self) -> &str;
    pub fn application(&self) -> &ApplicationContext;
    pub fn common_data<T>(&self) -> Result<Option<T>, FlameError>
    where
        T: FlameMessage;
}
```

This starts narrower than FlamePy's `context()` and `update_context()` methods because the Rust SDK does not yet expose the object-cache APIs that FlamePy uses for cached context. Cache-backed context update methods should be added after Rust `ObjectRef`, `get_object`, and `update_object` are available.

To make this practical, `ApplicationContext`, `SessionContext`, and `TaskContext` should derive `Clone` and `Debug`. They contain owned strings and `Bytes`, so cloning is acceptable for adapter use.

For typed payloads:

- `I` must implement `FlameMessage` for required and optional typed input.
- `O` must implement `FlameMessage`, unless `O` is `()`.
- Missing required input returns `FlameError::InvalidConfig`.
- `Option<I>` receives `None` when the task input is absent.

Raw `TaskInput` and `TaskOutput` are not supported by the high-level macros. Services that need raw payload control should implement `flame_rs::service::FlameService` directly.

Supported hook signatures:

```rust
async fn enter(&self) -> Result<(), FlameError>
async fn enter(&self, instance: FlameInstance) -> Result<(), FlameError>
async fn leave(&self) -> Result<(), FlameError>
```

Generated implementations do not support mutable `&mut self`; service state should use the same interior-mutability patterns already required by the current `FlameService` trait.

**CLI:**

No user-facing Flame CLI changes.

**Other Interfaces:**

Add reusable message conversion helpers under `flame_rs::message`:

```rust
pub fn decode<T>(input: TaskInput) -> Result<T, FlameError>
where
    T: FlameMessage;

pub fn decode_optional<T>(input: Option<TaskInput>) -> Result<Option<T>, FlameError>
where
    T: FlameMessage;

pub fn encode<T>(output: T) -> Result<Option<TaskOutput>, FlameError>
where
    T: FlameMessage;

pub fn encode_optional<T>(output: Option<T>) -> Result<Option<TaskOutput>, FlameError>
where
    T: FlameMessage;

pub fn encode_unit() -> Result<Option<TaskOutput>, FlameError>;
```

Client helpers and service macros should call these helpers instead of embedding repeated parsing code. They are public so manual clients and service adapters can share the same behavior. `flame_rs::service::message` may re-export this module for service-oriented imports, but `flame_rs::message` is the canonical path.

**Scope:**

In scope:

- Top-level Rust client helpers for default-context connections, creating sessions, and opening sessions.
- `SessionOptions` as a builder-compatible replacement for repetitive `SessionAttributes` construction.
- `Session::invoke(...)`, `Session::run(...)`, typed `FlameMessage` invocation helpers, and task handles.
- `FlameMessage` derive support for input, output, and common-data payloads.
- Optional proc-macro support for Rust FlameInstance-style service implementations.
- Free-function entrypoint services for the simplest FlamePy-like path.
- Struct-backed instance services for stateful model/service code.
- A lightweight `FlameInstance` handle for session/task/common-data access.
- Typed `FlameMessage` task and common-data payloads.
- Optional lifecycle hook methods.
- High-level `flame_rs::run(...)` helper used from a user-authored `main` function.
- Compile-time validation for unsupported signatures.
- Tests and docs for generated behavior.

Out of scope:

- Multiple task endpoints per service. The current shim invokes one task method per service.
- A new wire protocol or protobuf change.
- Automatic application YAML generation.
- Separate client-side typed task macros beyond `FlameMessage`.
- A Rust equivalent of Python `cloudpickle` service sessions.
- A global default connection singleton. This can be added later if repeated context loading becomes a measured problem.
- Python service API changes.
- User-facing serialization format selection. `FlameMessage` owns the typed serialization contract.
- Raw payload support in the simplified client and service macro APIs. Existing `flame_rs::client::Session::{create_task, run_task, watch_task}` and manual `FlameService` implementations remain the raw path.
- Full FlamePy `context()` and `update_context()` parity until Rust has object-cache APIs.
- Local FastAPI-style debug serving in the MVP; Rust local debug mode can be a later feature.

Limitations:

- The macro does not remove the need for explicit state synchronization inside a service.
- `FlameMessage` is the ergonomic typed path. Performance-sensitive binary payloads should continue using the existing low-level APIs until a typed binary message format is designed.
- Proc-macro errors must be carefully tested so users do not see opaque generated-code failures.

**Feature Interaction:**

Related features:

- Rust SDK client runtime: `sdk/rust/src/client/mod.rs`
- Rust SDK context loading: `sdk/rust/src/apis/ctx.rs`
- Rust SDK service runtime: `sdk/rust/src/service/mod.rs`
- Host shim gRPC bridge: `executor_manager/src/shims/host_shim.rs`
- Existing Rust services: `flmping`, `flmexec`, `examples/pi/rust`
- Model-serving examples

Updates required:

- `sdk/rust/src/client/mod.rs`: add `SessionOptions`, top-level helpers, invocation helpers, and task handles.
- `sdk/rust/src/lib.rs`: re-export high-level client helpers, `FlameMessage`, and message conversion traits at the crate root.
- `sdk/rust/src/message.rs`: add `FlameMessage`, input/output/common-data conversion traits, and shared message helpers.
- `sdk/rust/Cargo.toml`: optional `flame-rs-macros` dependency behind `macros`.
- Root `Cargo.toml`: add `sdk/rust/macros` as a workspace member.
- `sdk/rust/src/service/mod.rs`: re-export shared message helpers for service imports and expose `FlameInstance`.
- Rust SDK docs and at least one example should show the macro path.

Compatibility:

- No breaking changes to `Connection`, `Session`, `SessionAttributes`, or `TaskInformer`.
- No breaking changes to `FlameService`.
- Existing manual service implementations remain supported.
- Existing manual client implementations remain supported.
- Macro-generated wrapper services produce the same gRPC responses as manual implementations.
- The `macros` feature is opt-in to avoid adding `syn`/`quote` compile cost to minimal SDK consumers.

Breaking changes:

- None.

## 3. Implementation Details

**Architecture:**

The simplification layer has a client half and a service half. Neither changes the Flame runtime protocol.

Client helpers are thin async wrappers over the existing frontend gRPC client:

```text
flame_rs::create_session(...)
      |
      | loads FlameContext and connects
      v
Connection::create_session(SessionAttributes)
      |
      v
Existing Frontend gRPC API

Session::invoke(...)
      |
      | create task + watch task + collect output
      v
Existing Session::create_task / watch_task flow
```

The service macro layer is a source-level adapter:

```text
User entrypoint function or instance impl
      |
      | #[flame_rs::entrypoint] / #[flame_rs::instance]
      v
Generated wrapper impl FlameService
      |
      v
Existing flame_rs::service::run()
      |
      v
Existing HostShim / GrpcShim / Executor flow
```

**Components:**

- `sdk/rust/src/client/mod.rs`
  - Add `SessionOptions` and conversion to `SessionAttributes`.
  - Preserve session common data on `Session` so callers can decode it through `Session::common_data<T>()`.
  - Add default-context helpers: `connect`, `connect_with_context`, `connect_with_config`, `create_session`, `open_session`, and `open_or_create_session`.
  - Add connection-level `create_session_with` and `open_or_create_session_with` for reused connections.
  - Add high-level `Session::invoke`, `Session::run`, and typed common-data accessors.
  - Add generic `TaskHandle<O>`, `TaskFuture<O>`, and `TaskResult<O>`.

- `sdk/rust/src/message.rs`
  - Define `FlameMessage`, `IntoTaskInput`, `FromTaskOutput`, and `IntoCommonData`.
  - Provide shared encode/decode helpers for typed task payloads and common data.
  - Serve client helpers, macro output, and manual services.

- `sdk/rust/macros`
  - New `proc-macro = true` crate.
  - Derive `FlameMessage` for typed payload structs and enums.
  - Parse free-function `#[flame_rs::entrypoint(...)]`.
  - Parse impl-block `#[flame_rs::instance(...)]` and its entrypoint marker.
  - Validate method signatures.
  - Emit a `FlameService` impl.

- `sdk/rust/src/service/mod.rs`
  - Re-export shared `message` helpers for service imports.
  - Expose `FlameInstance`.
  - Add `Clone`/`Debug` derives to service context structs.
  - Re-export `tonic::async_trait` for generated code if needed.

- `sdk/rust/src/lib.rs`
  - Re-export high-level client helpers and message traits.
  - Re-export `flame_rs_macros::{FlameMessage, entrypoint, instance}` behind `macros`.
  - Export `run(...)` and the conversion trait that turns generated typed entrypoints into `FlameService` wrappers.

**Data Structures:**

No runtime data model changes.

Client helper data structures are SDK-only wrappers around existing types:

```rust
pub struct SessionOptions {
    id: Option<SessionID>,
    application: String,
    common_data: Option<CommonData>,
    min_instances: u32,
    max_instances: Option<u32>,
    batch_size: u32,
    priority: u32,
    resreq: Option<ResourceRequirement>,
}

// Added to the existing client Session representation.
pub struct Session {
    common_data: Option<CommonData>,
    // existing fields omitted
}

pub struct TaskHandle<O> {
    task_id: TaskID,
    future: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<O>, FlameError>> + Send + 'static>,
    >,
}

pub struct TaskFuture<O> {
    task_id: TaskID,
    future: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TaskResult<O>, FlameError>> + Send + 'static>,
    >,
}

pub struct TaskResult<O> {
    pub task_id: TaskID,
    pub session_id: SessionID,
    pub state: TaskState,
    pub output: Option<O>,
    pub error_code: Option<i32>,
    pub error_message: Option<String>,
}
```

`TaskHandle` stores the created task ID plus an internal future that maps successful terminal state to decoded output and failed terminal state to `FlameError`. `TaskFuture` stores the same task ID plus the richer future that returns `TaskResult<O>` so parallel callers receive typed output and task lifecycle metadata together. Dropping a handle or future does not cancel the remote task in the MVP. Explicit cancellation can be designed separately if the frontend protocol grows that operation.

Generated services use a small wrapper:

```rust
pub struct GeneratedInstance<T> {
    inner: T,
    session: std::sync::Mutex<Option<SessionContext>>,
}
```

For free functions, `T` is a zero-sized marker. For struct-backed instances, `T` is the user service type.

The high-level runner accepts generated entrypoint descriptors or generated instance wrappers:

```rust
pub trait IntoFlameInstance {
    type Service: FlameService;

    fn into_flame_instance(self) -> Self::Service;
}

pub async fn run<T>(target: T) -> Result<(), Box<dyn std::error::Error>>
where
    T: IntoFlameInstance,
{
    crate::apis::init_logger()?;
    crate::service::run(target.into_flame_instance()).await
}
```

The existing `flame_rs::service::run(service)` remains the lower-level runner for manual services, including raw byte services and callers that want to initialize logging or runtime behavior themselves.

Internal macro parser structures:

```rust
struct ServiceMacroArgs {
    entrypoint_method: Option<String>,
    enter_method: Option<String>,
    leave_method: Option<String>,
    debug: bool,
    crate_path: syn::Path,
}

struct HandlerSignature {
    has_instance_handle: bool,
    input: HandlerInput,
    output: HandlerOutput,
}

enum HandlerInput {
    None,
    Required(syn::Type),
    Optional(syn::Type),
}
```

**Generated Code Sketch:**

For a free-function entrypoint:

```rust
#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct PingRequest {
    duration: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize, flame_rs::FlameMessage)]
struct PingResponse {
    message: String,
}

#[flame_rs::entrypoint]
async fn ping(req: Option<PingRequest>) -> Result<PingResponse, FlameError> {
    ...
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame_rs::run(ping).await
}
```

The macro turns the annotated function into an entrypoint descriptor and emits a hidden wrapper service equivalent to:

```rust
pub const ping: __FlamePingEntrypoint = __FlamePingEntrypoint;

async fn __flame_ping_impl(req: Option<PingRequest>) -> Result<PingResponse, FlameError> {
    ...
}

struct __FlamePingEntrypoint;

struct __FlamePingInstance {
    session: std::sync::Mutex<Option<flame_rs::service::SessionContext>>,
}

impl flame_rs::IntoFlameInstance for __FlamePingEntrypoint {
    type Service = __FlamePingInstance;

    fn into_flame_instance(self) -> Self::Service {
        __FlamePingInstance::default()
    }
}

#[flame_rs::service::async_trait]
impl flame_rs::service::FlameService for __FlamePingInstance {
    async fn on_session_enter(
        &self,
        ctx: flame_rs::service::SessionContext,
    ) -> Result<(), flame_rs::apis::FlameError> {
        *self.session.lock().map_err(|_| {
            flame_rs::apis::FlameError::Internal("instance session lock poisoned".to_string())
        })? = Some(ctx);
        Ok(())
    }

    async fn on_task_invoke(
        &self,
        ctx: flame_rs::service::TaskContext,
    ) -> Result<Option<flame_rs::apis::TaskOutput>, flame_rs::apis::FlameError> {
        let req = flame_rs::message::decode_optional::<PingRequest>(ctx.input)?;
        let output = __flame_ping_impl(req).await?;
        flame_rs::message::encode(output)
    }

    async fn on_session_leave(&self) -> Result<(), flame_rs::apis::FlameError> {
        *self.session.lock().map_err(|_| {
            flame_rs::apis::FlameError::Internal("instance session lock poisoned".to_string())
        })? = None;
        Ok(())
    }
}
```

The user writes the service binary entrypoint explicitly:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame_rs::run(ping).await
}
```

For a struct-backed instance, `#[flame_rs::instance]` keeps the inherent impl and emits a generated wrapper around the user type. This is required because Rust macros cannot add hidden fields to a user's struct.

The proc macro should use `proc_macro_crate` plus a `crate = "..."` override so renamed dependencies work.

**Algorithms:**

Default connection:

1. Load `FlameContext::from_file_with_env(None)`.
2. Read the current context's cluster endpoint and optional TLS config.
3. Call existing `client::connect_with_tls(endpoint, tls.as_ref())`.
4. Return the existing `Connection` type.

High-level session creation:

1. Convert `impl Into<SessionOptions>` into `SessionOptions`.
2. Validate that `application` is not empty.
3. Fill a missing session ID with `<application>-<stdng::rand::short_name()>`.
4. Convert `SessionOptions` into existing `SessionAttributes`.
5. Create a default `Connection`.
6. Call `Connection::create_session(&attrs)`.

Synchronous client invocation:

1. Convert `impl IntoTaskInput` into `Option<TaskInput>`.
2. Call existing `Session::create_task(input)`.
3. Watch the task to terminal state through an internal one-shot informer, or factor the current stream loop into a private helper that returns the final `Task`.
4. For `run(...)`, convert the terminal task into `TaskResult<O>` with decoded output on success and error code/message on failure.
5. For `invoke(...)`, wrap the `TaskFuture<O>` in `TaskHandle<O>` and map awaited `TaskResult<O>` to either successful decoded output or `FlameError`.

Typed message client invocation:

1. Convert input with `IntoTaskInput`. For `T: FlameMessage`, call `T::encode`; for `()`, produce no task input.
2. Call `invoke(...)` or `run(...)`.
3. Decode present successful output through `FromTaskOutput`. For `O: FlameMessage`, call `O::decode`.
4. Return `Ok(None)` if the service produced no output.
5. Preserve the `FlameError` returned by `FlameMessage`. The generated JSON implementation maps encode failures to `FlameError::Internal` and decode failures to `FlameError::InvalidConfig`, matching the service-boundary rules below.

Attribute parsing:

1. Parse `entrypoint`, `enter`, `leave`, `debug`, and `crate`.
2. Determine whether the target item is a free function or an inherent impl block.
3. For free functions, validate the function as the single task entrypoint.
4. For impl blocks, find exactly one entrypoint by marker attribute or configured method name.
5. Detect optional hooks by configured/default names.
6. Validate receiver is `&self` for instance methods.
7. Validate async methods and return shape.
8. Generate trait methods and message conversion calls.
9. Preserve the original function or impl block unchanged, removing consumed helper attributes from emitted impl items.

Input conversion:

1. If the entrypoint has no input parameter, do not inspect `ctx.input`.
2. If a parameter is `FlameInstance`, construct it from session/task context and pass it.
3. If parameter is `I: FlameMessage`, require input bytes and decode.
4. If parameter is `Option<I>`, decode only when bytes are present.
5. Map decode failures to `FlameError::InvalidConfig`.

Output conversion:

1. `()` maps to `Ok(None)`.
2. `O: FlameMessage` maps via `O::encode`.
3. `Option<O>` maps absent output to `None`.
4. Map encode failures to `FlameError::Internal`.

Error handling:

- MVP handler methods return `Result<_, FlameError>`.
- Future extension may support `error = "display"` for `Result<_, E>` where `E: Display`, mapping to `FlameError::Internal(e.to_string())`.

**System Considerations:**

Performance:

- Client `invoke(...)` does the same frontend calls as manual `create_task` plus `watch_task`.
- Top-level session helpers open a connection per call; callers that issue many operations should reuse `Connection` and call the connection-level helper methods.
- Macro-generated code is equivalent to hand-written adapter code.
- `FlameMessage` serialization cost is explicit at the payload type boundary.
- Raw byte performance remains a property of the existing low-level APIs, not this simplified layer.

Scalability:

- No scheduler, executor, or shim changes.
- Existing per-instance concurrency constraints remain unchanged.

Reliability:

- Client task handles should report failed or cancelled tasks as `FlameError` instead of silently returning absent output.
- Generated code must avoid `unwrap`.
- Missing input and bad message bytes should become structured `FlameError::InvalidConfig`.
- Hook failures propagate exactly as current manual implementations do.

Resource Usage:

- Runtime memory usage is unchanged except for normal message encode/decode buffers.
- Compile-time cost of `syn`/`quote` is isolated behind the optional `macros` feature.

Security:

- No new code execution path beyond user service code.
- Message decoding should not log request bodies by default.
- Error messages should include type/context, not sensitive payload contents.

Observability:

- Client helper errors should include session ID and task ID when available.
- Generated trait methods should preserve current `tracing::debug!` behavior from `ShimService`.
- Optional macro-generated trace spans can be added later, but MVP should avoid surprising log volume.

Operational:

- Service binaries are launched the same way by HostShim.
- Application YAML does not change.
- Operators can continue debugging generated services through normal Rust logs and task events.

**Dependencies:**

New optional compile-time dependencies:

- `syn`
- `quote`
- `proc-macro2`
- `proc-macro-crate`

Existing runtime dependencies used:

- `bytes`
- `futures`
- `serde`
- `serde_json`
- `tokio`
- `tonic`

## 4. Use Cases

**Example 1: Simple Rust Client**

Description:

Create a Flame session and invoke a typed service once without manually managing `Connection`, `SessionAttributes`, task creation, task watching, or `TaskInformer`.

Workflow:

1. Build a `SessionOptions` value or pass the application name directly.
2. Call `flame_rs::create_session(...)`.
3. Call `ssn.invoke(...)` for typed `FlameMessage` request/response data.
4. Await the returned `TaskHandle` to get decoded output or `FlameError`.
5. Close the session.

Expected outcome:

- The client code is close to FlamePy's `flamepy.create_session(...)` plus `ssn.invoke(...)` shape.
- Existing low-level Rust APIs remain available for callers that need custom task watching.
- Typed examples do not repeat byte serialization and deserialization code.

**Example 2: Parallel Rust Client Invocations**

Description:

Submit multiple prompts or work items to the same session and collect results later.

Workflow:

1. Create or open a session once.
2. Call `ssn.run::<_, ResponseType>(...)` for each request.
3. Await each `TaskFuture<ResponseType>` as capacity becomes available and inspect the returned `TaskResult<ResponseType>`.

Expected outcome:

- Client code can express FlamePy's `ssn.run(...)` pattern with explicit Rust async handles.
- Callers receive task ID, session ID, state, decoded output, and failure details in one result value.
- The future API leaves room for future cancellation without changing the one-shot `invoke(...)` method.

**Example 3: Simple Typed Service**

Description:

Convert `flmping` service logic to a typed request/response entrypoint.

Workflow:

1. Add `features = ["macros"]` to the `flame-rs` dependency.
2. Derive `FlameMessage` on `PingRequest` and `PingResponse`.
3. Replace the manual trait impl with a free function marked `#[flame_rs::entrypoint]`.
4. Write `async fn ping(req: Option<PingRequest>) -> Result<PingResponse, FlameError>`.
5. Call `flame_rs::run(ping).await` from the service binary `main`.

Expected outcome:

- Same wire format as current `flmping`.
- Less boilerplate in the service file.
- No direct `tonic` dependency needed by the service binary for the trait impl.

**Example 4: Stateful Model Service**

Description:

Simplify a model-serving service.

Workflow:

1. Store model assets in `Mutex<Option<Assets>>`.
2. Use `enter` to load model files and tokenizer once per executor instance.
3. Mark the typed `generate` method with `#[flame_rs::entrypoint]`.
4. Use `leave` to release assets.
5. Call `flame_rs::run(ModelService::default()).await` from the service binary `main`.

Expected outcome:

- Service lifecycle remains explicit.
- Task handler code deals with `GenerationRequest` and `GenerationResponse`, not task bytes.
- Existing Flame session semantics are unchanged.

## 5. References

**Related Documents:**

- `docs/designs/templates.md`
- `docs/api/shim.md`
- `docs/designs/RFE323-runner-v2/FS.md`

**Implementation References:**

- `sdk/rust/src/client/mod.rs`
- `sdk/rust/src/apis/ctx.rs`
- `sdk/rust/src/service/mod.rs`
- `sdk/rust/Cargo.toml`
- `sdk/python/src/flamepy/core/client.py`
- `sdk/python/src/flamepy/service/client.py`
- `examples/pi/rust/src/service.rs`
- `flmping/src/service.rs`
- `flmexec/src/service.rs`

**Test Plan:**

- Unit tests for client helpers:
  - `SessionOptions::from("app")` matches current FlamePy-style defaults
  - builder setters map correctly to `SessionAttributes`
  - `resreq("cpu=4,mem=16g,gpu=1")` uses existing `ResourceRequirement` parsing
  - `IntoTaskInput` implementations encode `FlameMessage` inputs and optional absence
  - `IntoTaskInput for ()` produces absent task input
  - `TaskHandle::id()` returns the created task ID
  - `TaskFuture::id()` returns the created task ID
  - `TaskHandle<O>` implements `Future<Output = Result<Option<O>, FlameError>>`
  - `TaskFuture<O>` implements `Future<Output = Result<TaskResult<O>, FlameError>>`
  - `TaskResult<O>` carries task ID, session ID, terminal state, decoded successful output, and failed task error code/message

- Unit tests for shared `message` helpers:
  - `FlameMessage` derive encodes and decodes a typed payload
  - missing required message input returns `InvalidConfig`
  - optional message input accepts absence
  - invalid message bytes return `InvalidConfig`
  - typed output serializes to bytes
  - unit output returns no task output
  - client-side typed output decode handles absent output as `None`
  - typed common data round-trips through `SessionOptions::common_data`

- `trybuild` compile-pass tests for macro signatures:
  - `FlameMessage` derive on request, response, and common-data types
  - typed handler with required input
  - typed handler with optional input
  - free-function entrypoint
  - struct-backed instance entrypoint
  - handler with `FlameInstance` plus input
  - hooks present and hooks omitted
  - custom method names

- `trybuild` compile-fail tests:
  - missing entrypoint method
  - more than one entrypoint method
  - non-async entrypoint
  - `&mut self` receiver
  - typed input without `FlameMessage`
  - entrypoint input using `TaskInput`
  - entrypoint output using `TaskOutput`

- Integration smoke tests:
  - Add or update a Rust client example to use `flame_rs::create_session(...)` and `ssn.invoke(...)`.
  - Convert or add a minimal Rust example service using the macro.
  - `cargo check -p flame-rs`.
  - `cargo check -p flame-rs --features macros`.
  - `cargo check` for the converted client/service examples.
  - Existing `cargo check --workspace --exclude cri-rs` still passes.

---
Issue: #25
Author: klaus
Date: 2026-05-19
Status: Draft
---

# RFE25: Session Bind Failure Recovery

## 1. Motivation

**Background:**

Flame currently separates session assignment from service initialization:

1. The session manager scheduler selects an executor and moves it to `Binding`.
2. The executor manager receives the assignment through the node stream.
3. The executor manager installs the application, creates a shim instance, and calls `OnSessionEnter`.
4. On success, the executor manager calls `BindExecutorCompleted`, and the session manager moves the executor to `Bound`.

This makes `OnSessionEnter` the real admission point for a session on a host. If that call fails today, the executor manager retries locally, logs the failure, transitions the local executor to `Unbinding`, and eventually returns the same executor to `Idle`. The session manager does not record a user-visible event for the bind failure or enforce a session-level retry limit, leaving users with little visibility into why the session is not making progress.

**Target:**

When session bind or enter fails, Flame should:

1. Record a session-level event that identifies the session, executor, node, session retry count, error code, and error message.
2. Detach the failed executor binding so the session can be scheduled again.
3. Remove local `OnSessionEnter` retry loops from executor-manager; each bind attempt calls `OnSessionEnter` once.
4. Enforce `recovery.session.retry_limits` with a transient retry counter on the in-memory session object.
5. Preserve existing task state semantics; no task should be marked failed merely because a session enter attempt failed before any task was launched.

## 2. Function Specification

### Configuration

Use the existing recovery/session namespace for session admission recovery:

| Parameter | Description | Default |
| --------- | ----------- | ------- |
| `recovery.session.retry_limits` | Maximum failed session bind completions allowed for one session before Flame stops retrying that session. | `5` |

Example:

```yaml
cluster:
  recovery:
    session:
      retry_limits: 5
```

### API

#### Backend RPC

Extend `BindExecutorCompletedRequest` so executor-manager reports the result of the session bind/enter attempt through the completion RPC using the existing wire `Result` message. At the Rust implementation boundary, convert this wire type into an internal `FlameResult`; session-manager controller and state logic should use `FlameResult`, not `rpc::Result`.

```protobuf
message BindExecutorCompletedRequest {
  string executor_id = 1;
  optional Result result = 2;  // omitted or return_code == 0 means success
}
```

Behavior:

- `result` omitted or `result.return_code == 0`: `BindingState::bind_session_completed(result)` resets the session object's transient `retry_count` and moves the executor from `Binding` to `Bound`.
- `result.return_code != 0`: `BindingState::bind_session_completed(result)` increments the session object's transient `retry_count`, records the bind failure event with `result.return_code` and `result.message`, detaches the session from the executor, and transitions it from `Binding` to `Unbinding` for cleanup.
- If the session `retry_count` reaches `recovery.session.retry_limits`, record a session event that the retry limit was reached. Keep the session open, but do not assign, pipeline, or bind a new executor to it while the retry count remains at or above the limit. Existing executors already assigned to the session are not changed by retry-limit exhaustion.
- Do not clamp `retry_count` at `retry_limits`. Multiple executors may already be in `Binding` for the same session when the limit is reached, so later in-flight bind failures can push `retry_count` above the configured limit.

Define shared constants for `Result.return_code` so the failure phase is machine-readable without adding a new RPC message:

| Constant | Code | Meaning |
| -------- | ---- | ------- |
| `BIND_RESULT_OK` | `0` | Bind completed successfully. |
| `BIND_RESULT_APPLICATION_INSTALL_FAILED` | `10` | Executor-manager failed to install or prepare the application package. |
| `BIND_RESULT_SHIM_CREATE_FAILED` | `11` | Executor-manager failed to create the shim or service instance. |
| `BIND_RESULT_ON_SESSION_ENTER_FAILED` | `12` | The service instance returned an error from `OnSessionEnter`. |
| `BIND_RESULT_UNKNOWN_FAILED` | `19` | Bind failed before executor-manager could classify the phase. |

`Result.message` should carry human-readable details and any source-specific code, for example `installer exited with status 127` or `service_return_code=7: missing model file`. `Result.return_code` should identify the Flame bind phase, not carry arbitrary plugin or service codes.

No frontend RPC changes are required. Session events are returned through existing `GetSession` and `ListSession` responses by populating `SessionStatus.events`.

### Event Model

Existing event storage is task-scoped by `EventOwner { session_id, task_id }`, while `SessionStatus` already exposes `events`. This RFE reserves `task_id = 0` as the session-level event owner. Real task IDs start at `1`, so this does not conflict with normal task lifecycle events.

Add common event code constants rather than scattering raw integers:

| Constant | Code | Owner | Meaning |
| -------- | ---- | ----- | ------- |
| `SESSION_BIND_FAILED` | `1001` | session (`task_id = 0`) | A session bind/enter attempt failed. |
| `SESSION_RETRY_LIMIT_REACHED` | `1002` | session (`task_id = 0`) | A session reached its retry limit and will not receive new executor assignments. Existing assigned executors are unaffected. |

Event messages should be concise and structured enough for humans:

```text
Executor <exec-1> on node <host-a> failed to bind session with return_code <12>: service_return_code=7: missing model file
Session retry limit reached after 5/5 failed attempts; new executor assignment is paused for this session; existing executors are unchanged.
```

### CLI

`flmctl view --session <id>` should show recent session events in table output. JSON output already includes the session object and should include `status.events` after the backend populates it.

Example table addition:

```text
Events:
  10:31:02.125 Executor <e1> on node <host-a> failed to bind session with return_code <12>: service_return_code=7: missing dependency (1001)
```

### Scope

**In Scope:**

- User-visible session events for bind/enter failures.
- A bind result report path from executor-manager to session-manager.
- Detaching failed `Binding` executors from the session so the session can be scheduled again.
- Configuring the global session retry limit with `recovery.session.retry_limits`.
- A transient `retry_count` on the in-memory session object.
- Focused tests for failure reporting, event recording, and unbind cleanup.

**Out of Scope:**

- Adding a terminal `SessionState::Failed`.
- Failing pending tasks because session enter failed.
- Checkpointing or resuming partially entered service state.
- Node health diagnosis. A bind failure affects only the session/node pair, not the global node state.
- Task retry counts from RFE384. This RFE handles pre-task session admission, not running task retry.
- Host avoidance behavior such as blocking hosts, excluding hosts from scheduling, or forcing rebinding to a different host. That can be handled in a later RFE.

**Limitations:**

- After a bind failure, the normal scheduler may select the same host again. This design only records the failure and releases the binding; host avoidance is out of scope.
- If an application is broken on every host, Flame will keep retrying according to normal scheduling behavior until the session reaches `recovery.session.retry_limits`.
- When the session reaches `recovery.session.retry_limits`, Flame keeps the session open but stops assigning or binding new executors to it. Existing assigned executors are not preempted or unbound by this condition. Pending tasks remain pending until the user closes the session or the transient retry count is cleared by a session-manager restart.

### Feature Interaction

**Related Features:**

- Scheduler dispatch and allocation actions select idle, void, and unbinding executors.
- RFE384 connection recovery handles node disconnect and task retry. This RFE handles successful node connectivity with failed session admission.
- Existing task event storage and `SessionStatus.events`.

**Compatibility:**

- Adding an optional `Result` to `BindExecutorCompletedRequest` is wire compatible if omitted values are treated as success.
- Existing executor managers that do not send `result` continue current behavior.
- Existing clients can ignore new event codes.

**Breaking Changes:**

None.

## 3. Implementation Detail

### Architecture

```mermaid
sequenceDiagram
    participant SCH as Scheduler
    participant SM as Session Manager
    participant EM as Executor Manager
    participant SVC as Service Instance

    SCH->>SM: bind_session(executor, session)
    SM-->>EM: Executor state Binding(session)
    EM->>SM: BindExecutor(executor)
    SM-->>EM: Application + Session
    EM->>SVC: OnSessionEnter(session)
    SVC-->>EM: failure
    EM->>SM: BindExecutorCompleted(executor, result=error)
    SM->>SM: record session event task_id=0
    SM->>SM: increment session.retry_count
    SM->>SM: detach executor from session; state=Unbinding
    EM->>SVC: OnSessionLeave best effort
    EM->>SM: UnbindExecutorCompleted(executor)
    SM->>SM: executor state Idle
    SCH->>SM: next cycle skips session if !session.is_ready(retry_limits)
```

### Components

| Component | Responsibility |
| --------- | -------------- |
| `executor_manager::states::idle` | Call `OnSessionEnter` once per bind attempt; report bind success or failure through `BindExecutorCompletedRequest.result`. |
| `executor_manager::states::unbinding` | Clean up shim state. For failed enter, `OnSessionLeave` is best effort because enter did not complete. |
| `session_manager::apiserver::backend` | Convert `BindExecutorCompletedRequest.result` from `rpc::Result` to `FlameResult` and forward it without interpreting success or failure. |
| `session_manager::controller` | Convert the backend completion into a state-machine call, persist the resulting executor state, and notify the node. It should not interpret bind result success/failure itself. |
| `session_manager::controller::executors::binding` | Interpret bind completion success/failure, validate binding state/session attachment, update the session object's transient `retry_count`, record bind failure and retry-limit events, and move the executor to `Bound` or `Unbinding`. |
| `session_manager::scheduler` | Skip sessions where `SessionInfo::is_ready(retry_limits)` is false; do not modify executors already assigned to that session. |
| `session_manager::storage` | Record session-level events. Do not persist `retry_count`. |
| `flmctl` | Display session-level events in session table output. |

### Data Structures

Reuse the existing RPC `Result` on the wire, but use an internal `FlameResult` in Rust code:

```rust
pub struct FlameResult {
    pub return_code: i32,
    pub message: Option<String>,
}

impl From<rpc::Result> for FlameResult { ... }
impl From<FlameResult> for rpc::Result { ... }

pub const BIND_RESULT_OK: i32 = 0;
pub const BIND_RESULT_APPLICATION_INSTALL_FAILED: i32 = 10;
pub const BIND_RESULT_SHIM_CREATE_FAILED: i32 = 11;
pub const BIND_RESULT_ON_SESSION_ENTER_FAILED: i32 = 12;
pub const BIND_RESULT_UNKNOWN_FAILED: i32 = 19;
```

Add a transient `retry_count` to the session manager's in-memory session object. This is the global session recovery retry count, not a bind-only counter. This RFE increments it on failed bind completion, resets it on successful bind completion, and clears it when the session is deleted or the session manager restarts. Do not add this field to the persisted storage engine record or public RPC `Session` object:

```rust
pub struct Session {
    // existing fields...
    pub retry_count: u32,
}

impl Session {
    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
    }
}
```

`SessionInfo` should mirror `retry_count` when scheduler snapshots are built, so scheduler decisions do not need a separate retry map:

```rust
pub struct SessionInfo {
    // existing fields...
    pub retry_count: u32,
}

impl SessionInfo {
    pub fn is_ready(&self, retry_limits: u32) -> bool {
        self.retry_count < retry_limits
    }
}
```

Use these helpers for admission checks instead of open-coding `retry_count < retry_limits`. The helper name describes readiness for new executor assignment, not session liveness; a session can be open but not ready for new executors after it reaches the retry limit.

`retry_count` is not capped by `retry_limits`. The limit controls whether the session is ready for new executor assignment; it does not cancel bind attempts that are already in flight.

Add session-level event helpers:

```rust
pub const SESSION_EVENT_TASK_ID: TaskID = 0;

impl EventOwner {
    pub fn session(session_id: SessionID) -> Self {
        Self {
            session_id,
            task_id: SESSION_EVENT_TASK_ID,
        }
    }
}
```

`Storage::get_session` and `Storage::list_session` should populate `session.events` from `EventOwner::session(session.id.clone())`.

### Algorithms

#### Executor Manager Enter Failure

```text
function bind_idle_executor(executor):
    session = backend.BindExecutor(executor.id)
    if session is none:
        transition executor to Releasing
        return

    if install application fails:
        backend.BindExecutorCompleted(
            executor.id,
            result = {
                return_code: BIND_RESULT_APPLICATION_INSTALL_FAILED,
                message: install_error,
            },
        )
        transition executor to Unbinding
        return

    if create shim fails:
        backend.BindExecutorCompleted(
            executor.id,
            result = {
                return_code: BIND_RESULT_SHIM_CREATE_FAILED,
                message: shim_error,
            },
        )
        transition executor to Unbinding
        return

    enter_result = shim.OnSessionEnter(session)
    if enter_result succeeds:
        backend.BindExecutorCompleted(
            executor.id,
            result = {
                return_code: BIND_RESULT_OK,
            },
        )
        transition executor to Bound
        return

    backend.BindExecutorCompleted(
        executor.id,
        result = {
            return_code: BIND_RESULT_ON_SESSION_ENTER_FAILED,
            message: format("service_return_code={}: {}", enter_result.return_code, enter_result.error_message),
        },
    )
    transition executor to Unbinding
```

Application install and shim creation failures use the same `BindExecutorCompleted` report path with phase-specific `Result.return_code` values and clear `Result.message` strings. If no shim instance exists, executor-manager should skip `OnSessionLeave` during local cleanup.

#### Session Manager Completion Handling

```text
function bind_executor_completed(executor_id, result):
    executor = storage.get_executor(executor_id)
    state = executor_state_machine(executor)
    state.bind_session_completed(result)
    storage.update_executor(executor)
    notify executor node

function BindingState.bind_session_completed(result):
    executor = self.executor

    if result is omitted or result.return_code == 0:
        if executor.ssn_id exists:
            session = storage.get_session_ptr(executor.ssn_id)
            session.retry_count = 0
        executor.state = Bound
        return

    if executor.ssn_id is none:
        return INVALID_STATE

    session_id = executor.ssn_id
    now = clock.now()

    storage.record_event(
        EventOwner::session(session_id),
        Event {
            code: SESSION_BIND_FAILED,
            message: format(
                "Executor <{}> on node <{}> failed to bind session with return_code <{}>: {}",
                executor.id,
                executor.node,
                result.return_code,
                result.message.unwrap_or_default(),
            ),
            creation_time: now,
        },
    )

    increment_session_retry_count(session_id)
    executor.state = Unbinding
    executor.ssn_id = none
    executor.task_id = none

function increment_session_retry_count(session_id):
    session = storage.get_session_ptr(session_id)
    previous_retry_count = session.retry_count
    session.retry_count += 1
    retry_count = session.retry_count
    retry_limit = config.recovery.session.retry_limits

    if previous_retry_count < retry_limit and retry_count >= retry_limit:
        storage.record_event(
            EventOwner::session(session_id),
            Event {
                code: SESSION_RETRY_LIMIT_REACHED,
                message: format(
                    "Session retry limit reached after {}/{} failed attempts; new executor assignment is paused for this session; existing executors are unchanged.",
                    retry_count,
                    retry_limit,
                ),
                creation_time: clock.now(),
            },
        )

    // Additional in-flight binding executors may fail after the limit is reached.
    // Keep incrementing retry_count and recording SESSION_BIND_FAILED, but do not
    // duplicate SESSION_RETRY_LIMIT_REACHED unless the count crosses the threshold.

```

Detaching `ssn_id` for a failed `Binding` executor is important. Existing scheduler accounting counts executors with `ssn_id` as allocated to the session, so leaving the session attached until unbind completion can delay the next bind attempt.

#### Scheduler Readiness

```text
function select_schedulable_sessions(snapshot):
    retry_limit = config.recovery.session.retry_limits
    return snapshot.find_sessions(
        SessionFilter::by_state(Open).with_predicate(
            |session| session.is_ready(retry_limit)
        )
    )
```

The scheduler should skip not-ready sessions during normal session candidate selection by applying readiness through `SessionFilter`, rather than fetching open sessions and filtering them in a separate pass. No separate `Statement` guard is required: skipped sessions never produce allocation, pipeline, or bind operations in that scheduling cycle. This readiness check is admission-only; it does not preempt, unbind, or otherwise modify executors already assigned or bound to the session.

#### Retry After Failure

When the session's transient `retry_count` is below `recovery.session.retry_limits`, `UnbindExecutorCompleted` returns the executor to `Idle` and the session remains open. The existing scheduler is responsible for any next bind attempt. This RFE does not add host avoidance state, so the next attempt may target the same host or a different host depending on ordinary scheduler state.

When the session's transient `retry_count` reaches `recovery.session.retry_limits`, the session manager records `SESSION_RETRY_LIMIT_REACHED`, leaves the session open, and prevents future executor assignment or binding for that session. Existing assigned executors keep their current states and follow normal lifecycle rules. This uses the existing open session state instead of introducing `SessionState::Failed` or closing the session.

Because scheduler decisions and executor-manager bind completions are asynchronous, several executors may already be binding to the same session when the session becomes not ready. Those in-flight bind attempts continue to completion. If they fail, session-manager still records `SESSION_BIND_FAILED` and increments `retry_count`, so `retry_count` can exceed `recovery.session.retry_limits`. The retry-limit event should be recorded when the count first crosses the threshold, not for every later in-flight failure.

### System Considerations

**Performance:**

Bind failure handling adds one event write on each reported failed bind completion, plus one retry-limit event when the transient `retry_count` reaches the limit. The scheduler adds only a simple per-session readiness check while selecting schedulable sessions.

**Scalability:**

The number of stored events grows with reported bind failures. This matches existing task event behavior and can use the same retention or cleanup policy.

**Reliability:**

The failure path is idempotent at the executor state level. Repeated reports may create repeated session events, but they should still leave the executor unbound and schedulable. Event recording failure should not leave the executor permanently bound; log the error and continue unbinding where possible.

**Resource Usage:**

No additional scheduler-owned state is introduced. The only new state is `retry_count` on the in-memory session object, and it is not persisted.

**Security:**

Failure messages come from service code. Sanitize control characters and truncate them to an internal bounded size before storing or returning them to clients.

**Observability:**

Add logs and metrics:

- `flame_session_retries_total{reason="bind_failure",result_code,node,application}`

Logs should include session ID, executor ID, node, result code, session retry count, retry limit, and error message.

**Operational:**

Operators can inspect session events through `flmctl view --session <id>` and JSON output. There are no bind-failure host records to inspect or clear.

### Dependencies

- Existing backend `BindExecutor` and `BindExecutorCompleted` RPCs.
- Existing backend `UnbindExecutorCompleted` RPC for local cleanup after failed enter.
- Existing backend `UnbindExecutor` RPC for normal unbind behavior.
- Existing session/task event storage.
- Existing executor state machine.

## 4. Use Cases

**Example 1: Session Enter Fails**

1. Session `s1` is scheduled to executor `e-a` on node `host-a`.
2. Application install succeeds, but one `OnSessionEnter` call fails because a local dependency is missing.
3. Executor-manager calls `BindExecutorCompleted(e-a, result={ return_code: BIND_RESULT_ON_SESSION_ENTER_FAILED, message: "service_return_code=7: missing dependency" })`.
4. Session-manager increments `s1.retry_count` and records `SESSION_BIND_FAILED`.
5. Session-manager detaches the failed binding and returns the executor through normal unbind cleanup.
6. A later scheduler pass retries the still-open session according to existing scheduling rules.

Expected outcome: users see the bind failure event, and the session can be scheduled again.

**Example 2: Application Installation Fails**

1. Session `s2` is scheduled to executor `e-b`.
2. Executor-manager cannot install the application package.
3. Executor-manager calls `BindExecutorCompleted(e-b, result={ return_code: BIND_RESULT_APPLICATION_INSTALL_FAILED, message: "installer exited with status 127" })`.
4. Session-manager records `SESSION_BIND_FAILED` with the installation failure code and increments `s2.retry_count`.
5. Executor `e-b` moves from `Binding` to `Unbinding` for cleanup.

Expected outcome: users and metrics can distinguish an installation failure from an `OnSessionEnter` failure using `Result.return_code`.

**Example 3: Repeated Enter Failures**

1. Session `s3` fails to enter on `host-a`.
2. Session-manager records `SESSION_BIND_FAILED`.
3. The executor is unbound and becomes eligible again.
4. The scheduler retries using existing policies. It may select the same host again because host avoidance is out of scope.
5. When `s3.retry_count` reaches `recovery.session.retry_limits`, session-manager records `SESSION_RETRY_LIMIT_REACHED`.
6. Later scheduler passes do not assign, pipeline, or bind a new executor to `s3` while the retry count remains at or above the limit.
7. Any other executor that was already assigned or bound to `s3` keeps running through its existing lifecycle.

Expected outcome: users get repeated session events that expose the failure, Flame stops assigning executors after the configured limit, and the session remains open with pending tasks unchanged.

**Example 4: Concurrent Bind Failures Exceed Limit**

1. Session `s4` has three executors already in `Binding`.
2. `recovery.session.retry_limits = 2`.
3. The first two bind completions fail. Session-manager records two `SESSION_BIND_FAILED` events, increments `s4.retry_count` to `2`, and records `SESSION_RETRY_LIMIT_REACHED`.
4. The third in-flight bind completion fails later. Session-manager records another `SESSION_BIND_FAILED` and increments `s4.retry_count` to `3`, but does not record a duplicate retry-limit event.
5. Later scheduler passes skip `s4` because `s4.is_ready(2)` is false.

Expected outcome: `retry_count` can exceed `retry_limits`; the limit stops only new scheduling, not in-flight bind completion handling.

**Example 5: Normal Unbind**

1. Scheduler preempts a bound executor or a session closes.
2. Executor-manager calls `UnbindExecutor`.
3. Session-manager follows existing unbind behavior.

Expected outcome: no bind failure event.

## 5. Verification Plan

**Unit Tests:**

- `BindExecutorCompletedRequest` with failed `result` records a session event owned by `task_id = 0`.
- `BindExecutorCompletedRequest` with omitted or successful `result` preserves the current success path.
- Executor-manager uses `BIND_RESULT_APPLICATION_INSTALL_FAILED` for application installation failures.
- Executor-manager uses `BIND_RESULT_SHIM_CREATE_FAILED` for shim or service instance creation failures.
- Executor-manager uses `BIND_RESULT_ON_SESSION_ENTER_FAILED` for `OnSessionEnter` failures and keeps the service return code in `Result.message`.
- Executor-manager calls `OnSessionEnter` once per bind attempt and reports failure through `BindExecutorCompletedRequest.result` instead of retrying locally.
- A failed bind result returns `INVALID_STATE` if the executor no longer has an assigned `ssn_id`.
- A failed bind result returns `INVALID_STATE` if the executor is not in `Binding`, so a stale failure cannot move `Bound` to `Unbinding`.
- Binding-state failure detach clears `ssn_id` and `task_id` while moving the executor to `Unbinding`.
- Successful bind completion resets the session object's transient `retry_count`.
- `Session::is_ready(retry_limits)` and `SessionInfo::is_ready(retry_limits)` return true only when `retry_count < retry_limits`.
- Scheduler session selection uses `is_ready` rather than open-coding retry-count comparisons.
- Reaching `recovery.session.retry_limits` records `SESSION_RETRY_LIMIT_REACHED`, leaves the session open, and prevents new executor assignment or binding.
- `retry_count` is not capped at `recovery.session.retry_limits`; in-flight bind failures can increment it above the limit.
- `SESSION_RETRY_LIMIT_REACHED` is recorded when `retry_count` first crosses the limit, not for every later failure above the limit.
- Reaching `recovery.session.retry_limits` does not preempt, unbind, or change executors already assigned to the session.
- The retry count is held on the in-memory session object and is not persisted or exposed through public session RPCs.
- `Storage::get_session` and `Storage::list_session` populate session-level events.

**Integration Tests:**

- Force `OnSessionEnter` to fail. Verify:
  - session events include `SESSION_BIND_FAILED`;
  - the event includes `BIND_RESULT_ON_SESSION_ENTER_FAILED`;
  - the executor returns through unbind cleanup;
  - the task remains pending.
- Force application installation to fail and verify the bind completion uses `BIND_RESULT_APPLICATION_INSTALL_FAILED`.
- Configure `recovery.session.retry_limits = 2` and verify session-manager records the retry-limit event after two failed bind completions, keeps the session open, does not assign or bind another executor to that session, and leaves any already assigned executor unchanged.
- With `recovery.session.retry_limits = 2`, fail three already-binding executors for the same session and verify `retry_count == 3`, all three `SESSION_BIND_FAILED` events are recorded, and only one `SESSION_RETRY_LIMIT_REACHED` event is recorded.

**Manual Smoke Test:**

1. Start a local cluster.
2. Register a test application whose `OnSessionEnter` fails.
3. Create a session and a task.
4. Run `flmctl view --session <id>` and confirm the bind failure event is visible.
5. Confirm the task remains pending and the executor is no longer stuck in `Binding`.

## 6. References

**Issue:**

- https://github.com/xflops/flame/issues/25

**Related Documents:**

- `docs/designs/RFE384-flame-recovery/FS.md`
- `docs/designs/RFE400-batch-session/FS.md`

**Implementation References:**

- `session_manager/src/scheduler/actions/dispatch.rs`
- `session_manager/src/scheduler/actions/allocate.rs`
- `session_manager/src/scheduler/statement.rs`
- `session_manager/src/controller/mod.rs`
- `session_manager/src/controller/executors/binding.rs`
- `session_manager/src/apiserver/backend.rs`
- `session_manager/src/storage/mod.rs`
- `session_manager/src/events/mod.rs`
- `executor_manager/src/states/idle.rs`
- `executor_manager/src/states/unbinding.rs`
- `rpc/protos/backend.proto`
- `rpc/protos/types.proto`

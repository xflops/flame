# RFE418: Replace Busy-Wait Future with Watch Channels

## 1. Motivation

**Background:**

Previously, the session manager used custom `Future` implementations with busy-wait polling for:
- Watching task state changes (`WatchTaskFuture`)
- Waiting for tasks in a session (`WaitForTaskFuture`)
- Waiting for session binding to executors (`WaitForSsnFuture`)

These implementations had significant performance issues:

1. **CPU Waste**: Busy-wait polling continuously called `wake_by_ref()` and returned `Poll::Pending`, consuming CPU cycles even when no state changes occurred.
2. **Scalability Bottleneck**: In large-scale deployments with thousands of tasks and executors, the constant polling created a performance bottleneck.
3. **Latency**: Polling-based approaches introduce latency between state changes and detection.

**Target:**

Replace busy-wait futures with an event-driven notification system using `tokio::sync::watch` channels to achieve:

1. **Efficient Waiting**: Tasks sleep until explicitly notified, consuming no CPU while waiting.
2. **Immediate Response**: State changes trigger immediate notification to waiters.
3. **Scalability**: Supports thousands of concurrent waiters without performance degradation.
4. **Race-Free Design**: Watch channels eliminate race conditions between state check and wait.

## 2. Function Specification

### Interface

**No external API changes.** The notification system is an internal implementation detail. All existing gRPC APIs (`WatchTask`, `LaunchTask`, `WaitForSession`) continue to work unchanged.

### Architecture

The notification system introduces a `NotifyManager` that manages two types of notifiers using watch channels:

```
┌──────────────────────────────────────────────────────────────────────┐
│                          Controller                                  │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │                       NotifyManager                            │  │
│  │  ┌──────────────────────────────────┐   ┌────────────────────┐ │  │
│  │  │          tasks                   │   │     executors      │ │  │
│  │  │  TaskNotifier                    │   │  ExecutorNotifier  │ │  │
│  │  │  HashMap<SessionID,              │   │  HashMap<          │ │  │
│  │  │    HashMap<TaskID, WatchChannel>>│   │    ExecutorID,     │ │  │
│  │  │                                  │   │    WatchChannel>   │ │  │
│  │  │  task_id=0: session-level notify │   │                    │ │  │
│  │  └──────────────────────────────────┘   └────────────────────┘ │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

### Components

**WatchChannel**: Wrapper around `watch::Sender<u64>` and `watch::Receiver<u64>`.
- Stores a version counter that increments on each notification
- Subscribers receive a `watch::Receiver` and call `changed().await`
- Notifications are never lost - late subscribers see the current version

**TaskNotifier**: Nested HashMap of watch channels for task state changes.
- `HashMap<SessionID, HashMap<TaskID, Arc<WatchChannel>>>`
- **task_id = 0**: Special sentinel meaning "any task in this session changed". Waiters on task_id=0 receive notifications whenever ANY task state changes in the session. Used for session-level watchers (e.g., `wait_for_task`).
- **task_id > 0**: Specific task notification. Waiters receive notifications only for that specific task.

**ExecutorNotifier**: Simple HashMap of watch channels for executor state changes.
- `HashMap<ExecutorID, Arc<WatchChannel>>`
- Notifies when executor state changes (e.g., session bound, released).

**NotifyManager**: Aggregates the two notifiers.
- `tasks`: TaskNotifier for task and session-level notifications.
- `executors`: ExecutorNotifier for executor state changes.

### Notification Points

| Event | Notifier | Method | Details |
|-------|----------|--------|---------|
| Task created | `tasks` | `notify(ssn_id, 0)` | Wakes session-level waiters |
| Task state changed | `tasks` | `notify(ssn_id, task_id)` | Wakes specific task waiters only |
| Task completed | `tasks` | `notify(ssn_id, task_id)` | Wakes specific task waiters |
| Session closed | `tasks` | `notify(ssn_id, 0)` | Wakes session-level waiters |
| Session deleted | `tasks` | `remove(ssn_id)` | Removes entire inner HashMap |
| Executor bound to session | `executors` | `notify(executor_id)` | Wakes executor waiters |
| Executor unregistered | `executors` | `remove(executor_id)` | Cleanup |

### Wait Operations

**watch_task**: Wait for specific task state change.
```rust
let mut rx = notifier.tasks.subscribe(ssn_id, task_id)?;
loop {
    if task.state != initial_state || task.is_completed() {
        return Ok(task);
    }
    rx.changed().await?;
}
```

**wait_for_task**: Wait for any task to be available in a session (uses task_id=0).
```rust
let mut rx = notifier.tasks.subscribe(ssn_id, 0)?;  // session-level
loop {
    if let Some(task) = session.pop_pending_task() {
        return Ok(Some(task));
    }
    if session.state == Closed {
        return Ok(None);
    }
    if timeout expired {
        return Ok(None);
    }
    timeout(remaining, rx.changed()).await;
}
```

**wait_for_session**: Wait for executor to be bound to a session.
```rust
let mut rx = notifier.executors.subscribe(executor_id)?;
loop {
    if executor.state == Releasing || Released {
        return Ok(None);
    }
    if executor.ssn_id is set {
        return Ok(session);
    }
    rx.changed().await;
}
```

### Why Watch Channels Over Notify

| Feature | `tokio::sync::Notify` | `tokio::sync::watch` |
|---------|----------------------|----------------------|
| Notifications stored | No - lost if no waiter | Yes - version persisted |
| Multiple consumers | Limited | All receive updates |
| Race condition | Possible | Eliminated by design |
| Check-then-wait | Requires `enable()` pattern | `changed()` handles atomically |

**Watch channel behavior:**
- `changed()` after `notify()`: Returns immediately
- `changed()` twice (no new notify): Second call blocks until next notify
- Multiple consumers wait, single `notify()`: All consumers wake up
- `notify()` before `subscribe()`: Subscriber sees the updated version

### Memory Management

Notifier entries are removed when sessions are deleted:
- **Tasks**: All task notifiers for a session removed via `remove(ssn_id)` in `delete_session()`.
- **Executors**: Entry removed in `unregister_executor()`.

This prevents unbounded growth of the notifier HashMaps.

## 3. Implementation Detail

### Data Structures

**WatchChannel**:
```rust
struct WatchChannel {
    tx: watch::Sender<u64>,
    rx: watch::Receiver<u64>,
}

impl WatchChannel {
    fn new() -> Self {
        let (tx, rx) = watch::channel(0u64);
        Self { tx, rx }
    }

    fn notify(&self) {
        self.tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.rx.clone()
    }
}
```

**TaskNotifier**:
```rust
pub struct TaskNotifier {
    channels: MutexPtr<HashMap<SessionID, HashMap<TaskID, Arc<WatchChannel>>>>,
}

impl TaskNotifier {
    pub fn subscribe(&self, ssn_id: &SessionID, task_id: TaskID) 
        -> Result<watch::Receiver<u64>, FlameError>;
    
    pub fn notify(&self, ssn_id: &SessionID, task_id: TaskID) 
        -> Result<(), FlameError>;
    
    pub fn remove(&self, ssn_id: &SessionID) -> Result<(), FlameError>;
}
```

**ExecutorNotifier**:
```rust
pub struct ExecutorNotifier {
    channels: MutexPtr<HashMap<ExecutorID, Arc<WatchChannel>>>,
}

impl ExecutorNotifier {
    pub fn subscribe(&self, id: &ExecutorID) 
        -> Result<watch::Receiver<u64>, FlameError>;
    
    pub fn notify(&self, id: &ExecutorID) -> Result<(), FlameError>;
    
    pub fn remove(&self, id: &ExecutorID) -> Result<(), FlameError>;
}
```

**NotifyManager**:
```rust
pub struct NotifyManager {
    pub tasks: TaskNotifier,
    pub executors: ExecutorNotifier,
}
```

**Controller**:
```rust
pub struct Controller {
    storage: StoragePtr,
    connection_manager: ConnectionManager<NodeCallbacks>,
    notifier: NotifyManagerPtr,
}
```

### Code Changes

**Removed**:
- `WatchTaskFuture` - Custom future for watching tasks
- `WaitForTaskFuture` - Custom future for waiting for tasks
- `WaitForSsnFuture` - Custom future for waiting for sessions
- `SessionNotifier` - Replaced by TaskNotifier with task_id=0
- `Notifier<TaskGID>` - Replaced by watch channel structure

**Added**:
- `session_manager/src/notify.rs` - WatchChannel, TaskNotifier, ExecutorNotifier, NotifyManager
- `Controller::wait_for_task()` - Moved from BoundState

**Modified**:
- `Controller::watch_task()` - Uses `notifier.tasks.subscribe(ssn_id, task_id)` + `rx.changed()`
- `Controller::wait_for_session()` - Uses `notifier.executors.subscribe(executor_id)` + `rx.changed()`
- `Controller::wait_for_task()` - Uses `notifier.tasks.subscribe(ssn_id, 0)` for session-level
- `Controller::create_task()` - Calls `notifier.tasks.notify(ssn_id, 0)` (session-level)
- `Controller::complete_task()` - Calls `notifier.tasks.notify(ssn_id, task_id)`
- `Controller::close_session()` - Calls `notifier.tasks.notify(ssn_id, 0)`
- `Controller::delete_session()` - Calls `notifier.tasks.remove(ssn_id)`
- `Controller::bind_session()` - Calls `notifier.executors.notify(executor_id)`
- `Controller::unregister_executor()` - Calls `notifier.executors.remove(executor_id)`

### System Considerations

**Performance**:
- Zero CPU usage while waiting (tasks sleep on watch channel)
- O(1) notification delivery via `send_modify()`
- Minimal memory overhead (one `WatchChannel` per active task + one per session for task_id=0)

**Scalability**:
- Supports thousands of concurrent waiters
- Each `watch::Receiver` tracks its own "last seen" version independently
- Nested HashMap provides O(1) session lookup + O(1) task lookup
- Session-level notifications (task_id=0) efficiently broadcast to all session watchers
- No polling overhead regardless of scale

**Reliability**:
- Race-condition-free design - `changed()` handles check-then-wait atomically
- Notifications never lost - version counter persists
- Memory leak prevention with explicit cleanup at task/session/executor deletion
- Timeout support for bounded waiting

## 4. Use Cases

### Use Case 1: Client Watching Task Completion

1. Client calls `WatchTask(session_id, task_id)`
2. Controller subscribes via `notifier.tasks.subscribe(ssn_id, task_id)`
3. Controller checks task state - not completed
4. Controller awaits `rx.changed()`
5. Executor completes task, calls `complete_task()`
6. Controller calls `notifier.tasks.notify(ssn_id, task_id)` - increments version
7. Client's `changed()` returns, receives updated task state

### Use Case 2: Executor Waiting for Task (Session-Level)

1. Executor calls `LaunchTask(executor_id)`
2. Controller gets session for executor
3. Controller subscribes via `notifier.tasks.subscribe(ssn_id, 0)` (session-level)
4. Controller checks for pending task - none available
5. Controller awaits `rx.changed()` with timeout
6. Client creates task in session
7. Controller calls `notifier.tasks.notify(ssn_id, 0)` - increments version
8. Executor's `changed()` returns, receives task and starts execution

### Use Case 3: Multiple Executors Waiting for Same Session

1. Executors A, B, C all call `LaunchTask` bound to same session
2. Each subscribes via `notifier.tasks.subscribe(ssn_id, 0)`
3. All three await `rx.changed()`
4. Client creates one task
5. Controller calls `notifier.tasks.notify(ssn_id, 0)`
6. **All three** executors wake up (watch broadcasts to all receivers)
7. One executor gets the task, others loop back and wait again

### Use Case 4: Session Deletion Cleanup

1. Admin calls `DeleteSession(session_id)`
2. Controller marks session as deleted in storage
3. Controller calls `notifier.tasks.notify(ssn_id, 0)` - wakes any remaining waiters
4. Controller calls `notifier.tasks.remove(ssn_id)` - removes entire inner HashMap
5. All task notifiers for the session are cleaned up in one operation

## 5. References

### Related Documents
- RFE418 GitHub Issue: https://github.com/xflops/flame/issues/418
- Tokio Watch Documentation: https://docs.rs/tokio/latest/tokio/sync/watch/index.html

### Implementation References
- Notifier module: `session_manager/src/notify.rs`
- Controller: `session_manager/src/controller/mod.rs`
- Executor states: `session_manager/src/controller/executors/`

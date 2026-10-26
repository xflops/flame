# Design: Agent Sandbox with Session Suspend and Resume

**Status:** Draft (MVP scope, see 2 and 9)
**Created:** 2026-10-23
**Issue:** [#601](https://github.com/xflops/flame/issues/601)
**Related:** [#490](https://github.com/xflops/flame/issues/490) sandbox runtime for Agent and RL,
[#499](https://github.com/xflops/flame/issues/499) agent `Session` API (RFE499),
RFE518 instance-level data-aware scheduling

---

## 1. Motivation

An AI agent runs for a long time but is mostly idle: it waits for a user, a tool or a model. Flame
binds a warm executor to a session for the session's lifetime. `Controller::launch_task` long-polls
in `wait_for_task` and returns no task only when the session is closed, the executor is unbinding
or interrupted, or the session is gone, so an idle bound executor keeps its sandbox, CPU and
memory.

Systems such as Google's Agent Substrate keep many mostly idle agents on few workers by
snapshotting an idle sandbox and restoring it when work arrives. Flame cannot pause a session and
continue it later, possibly on another node. Agent process state is lost when the executor is
interrupted, released or its node fails.

## 2. Goals and non-goals

**Goals**

1. A session can be suspended and resumed: `Open --suspend--> Suspended --resume--> Open`.
   While suspended it holds no executor, and the agent's process and filesystem state is
   persisted.
2. Snapshots are persisted in an external object store (S3 in the MVP), so a snapshot survives the
   loss of any Flame node, and resume may run on any node that can reach the store.
3. A suspend that cannot checkpoint fails and returns the session to `Open`. The sandbox is kept
   on its executor, so the agent's state survives if the session is rebound to that executor, and
   otherwise the session restarts from its last committed snapshot, or fresh if there is none
   (4.5, 5).
4. Suspend never terminates or rejects work: a running task finishes normally and only new launches
   stop. Clients can still submit tasks to a suspending or suspended session; they stay pending until
   the resume.
5. Suspend is supported only for applications on the host shim that opt in with `suspendable`, and
   only when the cluster configures a checkpoint store (`storage.checkpoint`, 4.7). Other shims,
   including CRI, reject it. The sandbox itself is run by the application (`flmbox`, 4.6).
6. Agents use it through a new `flamepy.agent.Sandbox` class with `run`, `run_code`,
   `suspend` and `resume`, without the core session API (4.10).
7. Applications and sessions that do not opt in behave exactly as before, and `flamepy.agent`
   `Session` is unchanged.

**MVP scope.** This design covers the smallest useful suspend and resume: explicit suspend and
resume of one sandbox session, a durable snapshot in S3 or a local directory, and the Python
`Sandbox` class. Everything that only makes it faster, cheaper or safer to operate is listed in
section 9 and is not part of the first release: data-aware placement of resumed sessions, a local
snapshot cache, the extra limits and TTL, the orphan sweep and GCS.

The work is delivered in sub-tasks: #603 session suspend and resume (the session states, the controller
and scheduler behavior, the API, `flmctl` and the SDK calls), #604 the executor resource group, and
#605 `flamepy.agent.Sandbox` and `flmbox`. The snapshot and checkpoint parts of this design follow the
session-level step. Where a section below describes a later sub-task it says so.

**Non-goals**

- Several concurrent sessions per executor (executor slots).
- Gang sessions: a sandbox session has `min_instances = max_instances = batch_size = 1`.
- Egress or credential gateways, GPU state checkpointing, live migration of a running task.
- Automatic suspend of an idle session (an idle timeout). Suspend is an explicit API call;
  a policy can be built on it later.
- Client-side encryption of snapshots (bucket server-side encryption is expected; see 7).
- File transfer: `download_file` and `upload_file` are not in this release. An agent reads a file
  by printing it with `run_code`; transfer APIs can be added later without changing the session model.

## 3. Current behavior

| Area | Behavior |
| ---- | -------- |
| Session state | `SessionState` is `Open` or `Closed` (`common/src/apis/types.rs`, `rpc/protos/types.proto`). `Closed` is terminal. |
| Executor states | `Void`, `Idle`, `Binding`, `Bound`, `Unbinding`, `Releasing`, `Released`. The executor manager maps each to a handler in `states/`; the session manager has a matching handler in `controller/executors/`. |
| Idle executor | `BoundState` calls `LaunchTask`; the session manager blocks in `wait_for_task` and returns `None` only when the session is closed, the executor is unbinding or interrupted, or the session is gone. `None` makes the executor `Unbinding`. |
| Unbind | `UnbindingState` calls `UnbindExecutor`, runs `Shim::on_session_leave` (a failure is logged and the unbind completes), then `UnbindExecutorCompleted`, then sets the executor `Idle`. |
| Bind | `IdleState` calls `Shim::create_instance` once per executor, then `on_session_enter`. |
| Service lifetime | The application service is executor scoped: created once, kept across session bindings, destroyed when the executor is released (`AGENTS.md`). |
| Host shim | `HostShim` runs the application service as a local process and calls its `on_session_enter`, `on_task_invoke` and `on_session_leave` over the instance gRPC. Applications on it can run arbitrary node-local programs. |
| Backend RPCs | `UnbindExecutor` returns only `Result`; `UnbindExecutorCompletedRequest` carries only the executor id (`BindExecutorCompletedRequest` also carries attributes). |
| Startup applications | Built-in applications such as `flmexec` are registered from the cluster's startup application list, not by users. |
| Scheduler | MinMax computes demand `max(tasks, min_instances)` capped by `max_instances`, and `is_overused` uses demand 0 for a `Closed` session. |
| Object cache | Objects are `<app>/<session>/<object>`, create-once, stored on a per-node disk path with `fsync` and an atomic rename. Packages use the reserved session `pkg`. A reference names the owning cache node, so an object is lost with its node. |
| Transfer backends | The executor manager only downloads: `DownloaderRegistry` handles `file`, `http(s)` and `grpc(s)` (`executor_manager/src/appmgr/downloader.rs`). The Python SDK uploads packages to `file`, `http(s)` or the cache. There is no S3 or GCS client in the workspace. |

## 4. Design

### 4.1 Session state machine

`SessionState` gains two values.

```
          suspend                 all executors unbound,
 Open ------------> Suspending -----snapshot recorded-----> Suspended
  ^  ^                  |                                       |
  |  |  suspend failed  |                                       |
  |  `------------------'                                       |
  `-------------------------- resume --------------------------'

 Open / Suspending / Suspended --close--> Closed
```

| State | Meaning |
| ----- | ------- |
| `Open` | Schedulable. Unchanged. |
| `Suspending` | Transient. A suspend was accepted: no new task is launched, running tasks finish, and the executor unbinds (and, in the checkpoint step, checkpoints). With a snapshot the session also records the suspend `attempt` id (4.4). It is persisted; a restart keeps it and the next scheduler cycle completes it (4.5 step 7). |
| `Suspended` | No executor. The snapshot record of `session.version` (4.4) holds a complete snapshot, or none if the session has never been suspended with an executor. |
| `Closed` | Terminal. Reachable from every state; releases the snapshot. |

Rejected transitions: `suspend` on anything but `Open`; `resume` on anything but `Suspended`
(resume on `Open` is a no-op success); `Open` from `Closed`.

`close` during `Suspending` is allowed and wins. A completion that later arrives for a session that
is not `Suspending`, or whose attempt id is not the current one, is discarded and its object is
deleted at once.

`suspend` does not depend on running work. The admission check and the move to `Suspending` still
happen under the session lock (the cached `SessionPtr` mutex that `close` uses), which serialises it
with `wait_for_task`.

#### Admission

`suspend` fails with `InvalidState` or `InvalidConfig` unless all of these hold:

1. The session is `Open`.
2. A running task does not block suspend. It finishes normally, and the executor then unbinds because
   `wait_for_task` returns `None` for a session that is not `Open`. Pending tasks stay queued (see
   4.3). An executor that is still `Binding` is not special: it finishes or fails its bind, and then
   unbinds in the same way.
3. The application is suspendable (4.2): its shim is `Host`, it has opted in, and the cluster has `storage.checkpoint` set.

### 4.2 Application and session configuration

A new application attribute `suspendable` (default `false`) declares that the application's
sandbox can be checkpointed and restored.

- Registration rejects `suspendable = true` unless the shim is `Host` and `storage.checkpoint` is
  configured. Because a suspendable application runs `runsc` with elevated privilege (4.6), it is
  accepted only from the cluster's startup application list, as `flmexec` is. Users cannot register
  one.
- Session creation for a suspendable application requires `min_instances = 1`,
  `max_instances = 1` and `batch_size = 1`. With these the session always owns exactly one executor
  and is never interrupted for being over demand.
- The sandbox runtime (`runsc`) is run by the application, not by Flame (4.6), so Flame carries no
  runtime handler and `cri-rs` is unchanged.
- A suspended or closed session unbinds its executor, which goes `Idle` as today. It is not released
  at once: the application's `delay_release` keeps the idle executor warm (the shuffle action
  releases it after twice that period), so the next sandbox session of the application, or the
  resume of this one, can reuse it and skip the service start. The service starts once per executor
  and stops when the executor is released, as `AGENTS.md` requires. The sandbox is per session and
  not part of the service lifetime: `flmbox` starts it in `on_session_enter`. It removes it in
  `on_session_leave` (4.6), except after a failed suspend, when it keeps it for the session to
  reclaim (4.5). A sandbox that belongs to a different session is always removed at the next
  `on_session_enter`, so a warm executor never carries one session's sandbox into another.
- **Session creation validation.** For a suspendable application the session manager decodes the
  `SandboxConfig` common data (4.4) and rejects the session with `InvalidConfig` unless only `image`
  is set (`snapshot_path` and `checkpoint_path` must be empty, because the executor manager owns
  them) and `image` matches the cluster allowlist `sandbox.images` (exact names or repository
  prefixes, default `ubuntu:26.04` only). Each allowlist entry is pinned to a digest
  (`ubuntu:26.04@sha256:...`). At creation the session manager stores that pinned reference in
  `SandboxConfig.image`, the one place it rewrites the client's common data, and `flmbox` pulls
  exactly the reference it is given and refuses a bare tag. Every node therefore uses the same base
  image, and the `base` check at restore (4.4) cannot fail because a tag moved. `flmbox` does not
  read the cluster configuration.
- **Retention and limits.** A `Suspended` session is accounted in `limits.max_sessions` like any
  other session that is not closed, with no special handling. It stays in the in-memory session
  store, and `evict_sessions` compares the size of the whole store with the limit, so it uses one
  slot. The limit frees room only by evicting the oldest *closed* sessions, and it stays soft: when
  none is left it logs a warning and lets the count grow. A suspended session is never evicted,
  because that would destroy saved state, and it stays suspended until it is resumed or closed
  (section 9 lists a TTL).
- **Authorization.** `SuspendSession`, `ResumeSession`, `CloseSession` and reopening use the same
  authorization as other session calls. This design adds no new principal.

Alternative considered: infer suspend support from the shim alone. Rejected, because checkpoint
also depends on the image and the sandbox user, so it is an explicit opt-in per application.

### 4.3 Scheduler and controller

- `Session::is_closed` stays. Add `is_open`, `is_suspending`, `is_suspended`.
- MinMax `compute_demand` and `is_overused` use demand 0 for every state except `Open`. `Dispatch`
  and `Allocate` skip sessions that are not `Open`. DRF does not charge a suspended session.
- `wait_for_task` returns `None` when the session is not `Open`. The executor then unbinds exactly as
  it does for a closed session; suspend adds no executor state and no executor RPC (4.5). Suspending
  wakes the long poll by removing the session's task notifier, as `close_session` does.
  `Notifier::subscribe` creates the channel on demand, so resume needs no extra step.
- **Cost of suspended sessions.** They are not free for the session manager, because it keeps every
  non-closed session in memory and rebuilds the scheduler view from all of them:
  - `Storage::snapshot` deep-clones every session, with its tasks, and `Controller::snapshot` builds a
    `SessionInfo` for each one, at every scheduling cycle. The plugins and actions then iterate all of
    them. Cost grows with the number of suspended sessions even though their demand is 0.
  - `load_data` loads every session and, for each, all its tasks at startup, and sets up its event
    stream. Suspended sessions add startup time and memory.
  - `evict_sessions` only evicts closed sessions, so suspended sessions are never evicted.

  Mitigations in this design: `Storage::snapshot` leaves `Suspended` sessions out of the scheduler
  snapshot (they hold no executor and have no demand, so no plugin needs them), so the per-cycle cost
  depends on open and suspending sessions only, and a `Suspended` session is added back to the
  snapshot in the same step that moves it to `Open` on resume (`limits.max_sessions` counts them,
  4.2). The commit to `Suspended` runs in the handler of `UnbindExecutorCompleted`, which also clears
  the executor's session, so no executor in the scheduler snapshot references a session that is
  absent from it (`shuffle.rs` calls `get_session` on every bound executor). The implementation
  must keep that order. The memory and startup cost of suspended sessions stays, and
  keeping their tasks out of memory until resume is deferred (section 9).
- Tasks can be created on a `Suspending` or `Suspended` session (only `Closed` rejects them). They are
  stored `Pending` and run after the resume. The running tasks of a `Suspending` session complete
  normally: `update_task_state` and `update_task_result` accept every state but `Closed`.
  `create_task` and both updates check the session state before the engine write, so a rejected call
  writes nothing (today they write the engine and then check the cache). `close_session` is unchanged:
  it still refuses a session with a running task. Automatic resume on a new task is deferred
  (section 9).
- **Completion (session level).** `SuspendSession` calls `Controller::signal_session(id, Signal::Suspend)`, which
  only starts the suspend asynchronously: it moves the session to `Suspending`;
  the controller checks the current state and persists the new one through the single
  `Storage::update_session_state(id, state)`, which refuses only a `Closed` session;
  it removes the session's task notifier (waking the executors blocked in `wait_for_task`, as
  `close_session` does) and returns the session, always `Suspending`. The
  scheduler's shuffle action completes it: each cycle, a `Suspending` session that no executor in its
  view references goes to `Suspended` through `Controller::suspend_session`. The scheduler
  has already verified that no executor serves the session, so the controller does not look at the
  executors again; it only moves a session that is still `Suspending`, so a session resumed or closed
  since the check is left alone. No bind gate is needed: binds (`Controller::bind_session`) and the
  completion run in the same scheduler loop, so the view cannot miss a bind, and a stale view can
  only over-count executors, which delays the completion. Nothing notifies the scheduler when an
  executor stops serving a session (`unbind_executor_completed`, a failed bind, unregister, node
  loss): it notices in its next cycle, and the completion is level-triggered, so a restart cannot
  leave the session in `Suspending`. `Suspended` is committed only after the executor's session
  is cleared and persisted, because the scheduler view drops `Suspended` sessions and `shuffle.rs`
  looks up the session of every bound
  executor. There is no deadline in this step: a suspend that waits behind a long task, or for an
  executor that never unbinds, stays `Suspending`. The session-level step has no attempt id; the
  checkpoint step adds one (4.4) for the executor's completion report.

### 4.4 Session version, snapshot record and session context

**Version.** `Session` gains `version: u64`, starting at 0 and increased by one each time a suspend
commits. Version 0 means the session never had a snapshot. A resume uses only the snapshot of the
session's current version, so a stale or orphan object is never restored.

**Attempt.** Every `SuspendSession` that is accepted generates a new `attempt` id, stored on the
session while it is `Suspending`. The object key contains it, so two attempts never write the same
key, and a completion is committed only if its attempt is the session's current one and the session
is still `Suspending`. The version increments at that commit, not when the attempt starts.

**Storage.** Storage gains a snapshot record next to the session, in all three engines (sqlite,
filesystem, none):

| Field | Meaning |
| ----- | ------- |
| `application`, `session`, `version` | Key. `version` is the session version this snapshot produces. |
| `attempt` | The suspend attempt that produced it. It is part of the object key. |
| `executor` | The executor whose sandbox was checkpointed, or none. |
| `base`, `store`, `key`, `size`, `sha256`, `etag`, `created_at` | `base` is the digest of the base image the sandbox was built on, which restore requires. The rest is the location and integrity of the archive (4.7). |

When `executor` is none no checkpoint was taken: the record has no archive, there is nothing to
download, and resume starts a fresh sandbox. Only `ResumeSession(fresh = true)` writes such a record.
A session that is suspended while it has no executor keeps its current version and record unchanged
(4.5), so an earlier snapshot is never lost.

**Session context.** The session manager constructs the `SessionContext` of every bind and unbind
from the session's version and its snapshot record. The executor manager never decides on its own;
it follows the context. The record's `executor` field is what selects the behavior of
the snapshot and checkpoint directories below: none means no restore and no checkpoint.

| Situation | Context |
| --------- | ------- |
| Bind, version 0 or the current record has no executor | No snapshot: start fresh. |
| Bind, current record has an executor | `snapshot`: that record. The executor manager downloads and unpacks it and puts the local path in the sandbox configuration below: restore. This applies to every bind, so an open session whose executor is lost restarts from its last snapshot and sees `resumed = true`. |
| Unbind of a `Suspending` session | `suspend` target: application, session, version + 1 and the attempt id. The executor manager writes a `suspend` marker holding the attempt id into the checkpoint directory (below): checkpoint. |
| Any other unbind | None: as today. |

**Sandbox configuration in common data.** For a suspendable application the session's common data is
a `SandboxConfig` that Flame defines:

| Field | Set by |
| ----- | ------ |
| `image` | The SDK, at session creation (default `ubuntu:26.04`). Stored with the session and immutable. |
| `snapshot_path` | The executor manager, in the copy it passes to `on_session_enter`: the local path of the downloaded snapshot, absent for a fresh start. |
| `checkpoint_path` | The executor manager, in the copy it passes to `on_session_enter`: an empty local directory reserved for this bind, always set. A suspend writes its checkpoint there. |

The stored common data is never rewritten, and a client can never set the two paths: the session
manager rejects such a session (4.2), and the executor manager clears both fields in the copy before
it sets them, so a path never comes from the client. They are local to one node and one bind.

`on_session_leave` takes no arguments (`OnSessionLeave(EmptyRequest)` in `shim.proto`), so `flmbox`
keeps the `SandboxConfig` it receives in `on_session_enter` in its own state and reads it from
`self` in the later hooks. A suspend is signalled through the file system, not the protocol: before
it calls `on_session_leave` the executor manager writes a `suspend` marker, holding the attempt id,
into `checkpoint_path`, and `flmbox` checkpoints only if the marker is there. The service and the
executor manager share the node, and the directory belongs to the executor manager. No shim
protocol or SDK interface changes.

### 4.5 Suspend and resume flow

Suspend and resume are handled in the session manager. The executor side has no new operation, no
new state and no new RPC.

| | Session manager | Executor manager |
| --- | --- | --- |
| Suspend | `SuspendSession` sets `Suspending`. The session then does what a close does to its executor: unbind it. On completion it records the snapshot and sets `Suspended`. The executor goes `Idle` and stays warm for `delay_release`. | The existing unbind. The manager writes the `suspend` marker, `on_session_leave` checkpoints into the reserved directory, and the manager uploads it. |
| Resume | `ResumeSession` sets `Open`. The scheduler allocates or assigns an executor as for any open session needing one. | The existing bind. The bind context names a snapshot, the manager downloads it, and `on_session_enter` receives `snapshot_path`. |

```
Void -> Idle -> Binding -> Bound -> Unbinding -> Idle   (warm for delay_release, then Releasing)
          |                              |
 snapshot_path ready           on_session_leave fills checkpoint_path, then upload
 before on_session_enter
```

#### Suspend

1. `SuspendSession(id)` runs the admission checks (4.1), moves the session to `Suspending` under the
   session lock with a new attempt id (4.4, from the checkpoint step), and persists it. Under the same
   lock `wait_for_task` stops handing out tasks (`!is_open`), so no new task starts after the check. A
   task that is already running is left alone; its executor unbinds when it finishes.
2. If the session has no executor, there is nothing to checkpoint: the controller moves the session
   to `Suspended` and leaves its version and snapshot record as they are. If its executor is
   `Binding` or `Unbinding`, the suspend still proceeds: the session stays `Suspending` until that
   executor stops serving it.
3. Otherwise suspending wakes the `LaunchTask` long poll, `wait_for_task` returns `None`, and the
   executor goes `Bound` to `Unbinding` as today. `UnbindExecutor` returns the `suspend` target (4.4,
   4.8); the executor manager then writes the `suspend` marker into `checkpoint_path`.
4. `UnbindingState` calls `Shim::on_session_leave`. The service (`flmbox`, 4.6) finds the `suspend`
   marker in the `checkpoint_path` it kept from `on_session_enter`, checkpoints its sandbox into it
   with the sandbox left running, and returns. The executor manager then computes a SHA-256 of the
   directory archive and uploads it to the snapshot store (4.7) under the key of the target, which
   contains the attempt id. For a suspend a service error is reported; for any other unbind it is
   still logged and the unbind completes. The service retries the checkpoint, and the executor
   manager the upload, a bounded number of times. After a successful upload the executor manager
   removes the marker and calls `on_session_leave` a second time, which makes `flmbox` kill the
   sandbox (no marker), so the snapshot is the only copy that remains. It does this before it
   reports the completion.
5. The executor manager calls `UnbindExecutorCompleted` with the attempt id and the `SnapshotRef` or
   an error, and the executor goes `Idle` as for any unbind.
   - **Success:** the sandbox is already gone (above). In one storage transaction the session manager checks
     the attempt (4.4), writes the snapshot record (`executor` set), increments the version and moves
     the session to `Suspended`. Only after the commit is the previous version's object deleted.
   - **Failure:** the sandbox is **not** killed. It is still running because the checkpoint left it
     running. The session returns to `Open`, an event is recorded, and the user can retry the
     suspend. If the scheduler binds this executor again, `on_session_enter` finds the running
     sandbox of this session and reattaches to it, without restoring, so the agent's state is
     intact. Nothing steers the session back to this executor in the MVP (placement is deferred,
     section 9), so this is luck and not a guarantee. If another executor is chosen, or this one is
     released first (`delay_release`), the sandbox is gone and the session restarts from its last
     committed snapshot, or fresh. The executor manager deletes the `suspend` marker, the checkpoint
     directory and the failed attempt's object.
6. **Deadline:** the session-level step has none (4.3). The checkpoint step bounds the checkpoint and
   upload; a failed or timed-out upload takes the failure path above, and a late completion carries an
   attempt id that is no longer current and is discarded, and its object deleted.
7. **Restart recovery:** an executor manager or `flmbox` restart is treated as the loss of the
   executor, not resumed mid-suspend: `flmbox` keeps its configuration in memory, and the executor's
   resource group is killed at start (4.6). The attempt then fails as in the "Executor or node dies"
   row of section 5. A restarted session manager loads a persisted `Suspending` session as it is; the shuffle
   action completes it in the next cycle once no executor serves it.

#### Resume

1. `ResumeSession(id)` is a session state change only: it moves `Suspended` to `Open` under the
   session lock and notifies the scheduler. Nothing is sent to an executor.
   With `fresh = true` it also writes a record with no executor at version + 1, so the context carries
   no snapshot, and deletes the old object. This is the explicit data-loss path.
2. With `min_instances = 1` the scheduler allocates an executor on any node, as for a new session.
3. The session manager builds the bind context from the session version (4.4).
4. The executor manager's application manager downloads the snapshot named by the context from the
   snapshot store, verifies its SHA-256 and unpacks it into a local directory, before
   `on_session_enter`. It sits next to the package downloader in `appmgr/`. A snapshot is large,
   and this runs inside the `Binding` state, so the bind timeout and the session retry limit must
   allow for the download time.
5. The executor manager calls `on_session_enter` with the `SandboxConfig` whose `snapshot_path` is
   that directory. `flmbox` downloads the image named by `image` and starts the sandbox: restored
   from `snapshot_path` if it is set, fresh otherwise, or reattached to the live sandbox of this
   session if it is still running there (above). This is the whole resume action. The context
   carries `resumed = true` when it restored from `snapshot_path`, and `false` for a fresh start or
   a reattach, so the application can refresh what is not in its
   checkpointed memory, such as session tokens.
6. A download or restore failure takes the existing bind-failure path (`bind_executor_failed`) and
   the session retry limit. A corrupt snapshot is never silently replaced; only `fresh = true`
   discards it.

### 4.6 `flmbox` on the host shim

Flame does not run the sandbox. The sandbox is run by the application, `flmbox`, a Rust service
(Flame Rust SDK) on the existing host shim. This needs no new shim, no `cri-rs` change, no checkpoint call in the shim trait and no change to
the `OnSessionLeave` request. Flame supplies only the directory contract of 4.4 and the
transfer to and from the snapshot store.

| Hook | `flmbox` action (`runsc`) |
| ---- | ------------------------- |
| `on_session_enter` | Keep the `SandboxConfig` in `self`. Kill any sandbox that belongs to another session. Set up the sandbox network (below). If a sandbox of this session is still running here (a failed suspend), reattach to it. |
| `on_session_enter`, no running sandbox, no `snapshot_path` | Pull `image` by its pinned digest (once per node) and unpack it, build an OCI bundle over it, then `runsc run`. |
| `on_session_enter`, no running sandbox, `snapshot_path` | Same bundle (refuse an image whose digest differs from the snapshot's metadata), then `runsc restore --image-path <snapshot_path>` with the same mounts. |
| `on_task_invoke` | `runsc exec` the command or code in the sandbox. |
| `on_session_leave`, `suspend` marker in `checkpoint_path` | `runsc checkpoint --leave-running --image-path <checkpoint_path>`, plus a small metadata file with the base digest and the `runsc` version. The sandbox keeps running. |
| `on_session_leave`, no marker | `runsc kill` and `runsc delete`, and tear down the sandbox network, so the retained service holds no sandbox. This is the only call of an ordinary unbind. A suspend calls it a second time after the upload succeeds (4.5); a failed suspend never makes the second call, so the sandbox stays. |

**Executor resource group.** The host shim runs the service of every executor in a per-executor
resource group (cgroup v2, Linux). It fixes three problems: the sandbox needs resource limits, a
sandbox must not outlive its executor, and `flmbox` processes of different executors share a node.
It is predefined, not configurable: the root is `/sys/fs/cgroup` and the groups are
`/sys/fs/cgroup/flame/<executor-id>`.

- `create_instance` creates the group, sets `memory.max` (and `memory.swap.max = 0`) and `cpu.max`
  from the executor's resource requirement (CPU is whole cores, a quota of `cores * 100000` per
  100000 microseconds; zero means no limit), and moves the service into it right after the spawn,
  with any child it already forked. The requirement is the executor's reservation, and it equals the
  requirement of every session the executor serves (`is_available` in `scheduler/ctx.rs` matches
  them), so it is also the right limit for the sandbox. The session context needs no resource field.
- The limits bind everything the service starts, including `runsc` and its sandboxes and any group
  they create below, because cgroup limits are hierarchical. (Putting each sandbox in its own child
  group through `linux.cgroupsPath` in the OCI bundle is a refinement for later.)
- `destroy_instance` kills the group (`cgroup.kill`, Linux 5.14 or later; otherwise it kills every
  pid in `cgroup.procs` of the group and the groups below it until it is empty), waits for it to be
  empty and removes it. This also kills `runsc` sandboxes, which the existing process group does not
  reach because `runsc` starts its sandbox in its own session. So a released executor, a SIGKILLed
  service or a failed-suspend sandbox never leaves a live sandbox behind. A group that cannot be
  removed makes `destroy_instance` fail and is kept for a retry.
- When the executor manager starts it kills and removes every group under `/sys/fs/cgroup/flame/`
  that does not belong to one of its live executors; at start it has none, so all of them are stale.
  Group names carry the executor id, so one executor's cleanup never touches another's group.
- It needs cgroup v2 with the `cpu` and `memory` controllers and a writable subtree (the executor
  manager runs as root, or with a delegated `/sys/fs/cgroup`). Where that is missing (macOS, a
  container with a read-only cgroup file system, cgroup v1) the executor still starts, without a
  group, and the reason is logged as a warning. Nothing changes for those nodes.

The base rootfs is unpacked once per node and shared read-only. Credentials for the snapshot store stay in the executor manager and never reach the
service or the sandbox.

**Privilege.** `flmbox` runs `runsc` and so needs more privilege than a normal service. It is a
built-in application from the startup list (4.2), and agent code runs only inside the sandbox that
it creates, not in the service. Its inputs are untrusted: the image is checked against the
allowlist, the paths in `SandboxConfig` come only from the executor manager (4.4), and commands
and code reach only `runsc exec`. Its sandboxes run inside the executor's resource group below.

**Network** (target design; the first delivery has no network, see "Phase 1 implementation" below). Whether a sandbox can reach other hosts decides whether it can attack the cluster, so
`flmbox` sets the network up itself in `on_session_enter`, before it runs or restores the sandbox,
and does not rely on the node operator's firewall:

1. Create a network namespace for the sandbox and a veth pair to a node-local bridge. The address
   *inside* the sandbox is the same fixed one for every sandbox on every node (for example
   `10.200.0.2`), because a restored sandbox keeps its address and may resume on any node. The
   host side of the veth gets a unique address from a node-local allocator, and the sandbox's
   traffic is source-translated to it, so sandboxes on one node do not collide. The allocator is node-wide (a locked file), because
   several executors share a node.
2. Install per-sandbox filter rules on that interface. The default drops everything to: the Flame
   control plane and cache endpoints, the node's own addresses and services, other sandboxes, the
   cluster pod and service ranges, and the cloud metadata address (`169.254.169.254`). It allows
   DNS and the destinations in `sandbox.egress` (an allowlist of CIDRs or hosts, for example package
   mirrors), and the internet only when `sandbox.egress` includes it.
3. Run `runsc` with `--network=sandbox` inside that namespace, never `host`. `sandbox.network =
   none` skips steps 1 and 2 and gives the sandbox no interface.
4. Remove the namespace, interface and rules in `on_session_leave`, when the sandbox is killed. A
   sandbox kept after a failed suspend keeps its network too.

A restore is assumed to need the same network as the checkpoint (the spike confirms it): the gVisor
network stack is part of the saved state, and established connections do not survive a restore (the application sees them reset and
reconnects). So `flmbox` rebuilds the same interface and in-sandbox address before `runsc restore`, and, since the in-sandbox address is fixed,
that works on any node. This needs
`CAP_NET_ADMIN` in addition to the other privilege `flmbox` has. The spike verifies the isolation
rules and the restore with a rebuilt network (4.9).

The instance socket and the task input are the service's own concern now: the sandbox is entered
with `runsc exec`, so no socket is mounted into the sandbox and there is no mount to recreate on
restore beyond what the bundle already declares.

**Phase 1 implementation.** The first delivery (#605) runs sandboxes without the checkpoint flow. The OCI runtime
spec and the image manifest, index and config types come from the `oci-spec` crate:

- **Image acquisition.** `flmbox` pulls with the `oci-client` crate (anonymous token auth; the image index is resolved
  to the `linux/<arch>` manifest; manifest and blob digests are verified) and adds a size and layer cap, a hardened untar (path confinement, whiteouts applied while
  unpacking) and a node-local cache keyed by the manifest digest, with a per-digest file lock, so that the executors of a node pull an image once. This follows the
  `atelet` imagecache of Google's Agent Substrate, but flattened per image: there is no per-layer pool and no host
  overlayfs mount, the rootfs is read-only and `runsc --overlay2=root:memory` provides the writable layer. Phase 1 has no
  image allowlist: `flmbox` pulls any valid reference (limited by the image size cap), and the allowlist of 4.2 is
  deferred with the session-manager validation.
- **Binary and CLI.** The binary is `flmbox`. `flmbox -d` runs the Flame service instance (the application manifest
  starts it with that argument), and `flmbox pull|images|ps|run|exec|kill` drive the same lifecycle from a shell: `pull`
  downloads an image into the node cache, `images` lists the cache, `ps` lists the sandboxes with their images, `run`
  creates and starts a sandbox and prints its id, `exec` runs a command in it and exits with the command's exit code,
  and `kill` removes it. The sandboxes live in `runsc`'s state directory,
  so the CLI and the service reach them from separate processes. The lifecycle (`sandbox` module) is keyed by sandbox
  id and holds no state; the service only binds one id to the session.
- **Configuration.** `flmbox` reads a `sandbox:` section of a YAML file into `FlameSandbox`, the way the cluster
  file becomes `FlameCluster`: `state_dir`, `runtime` (`type: runsc`, `path`, `platform`, `overlay`) and `images`
  (`max_size`). Every key is optional. The file is `--config`, `$FLMBOX_CONF`, `$FLAME_HOME/conf/flame-sandbox.yaml` or
  `~/.flame/flame-sandbox.yaml` (the last two only when present). The `FLMBOX_*` variables override the file, and the file
  overrides the defaults. The runtime `type` selects the `Runtime` implementation below.
- **Runtime trait.** `Sandbox` talks to a `Runtime` trait (`start`, `exec`, `kill`, `list`; `runtime/mod.rs`), not
  to `runsc`. gVisor (`runtime/runsc`, which also writes its own OCI bundle) is the only implementation; another runtime such
  as Firecracker implements the trait and is selected in `runtime_for`.
- **No network by default.** Phase 1 is lifecycle only. Sandboxes run with `runsc --network=none` and an empty network namespace,
  so they cannot reach the control plane, the node or the internet. An opt-in `sandbox.network: host` (or `FLMBOX_NETWORK=host`, or `flmbox --network=host`, which
  the built-in `flmbox` application passes in its `arguments`) runs them with `runsc --network=host` instead: they get every egress of the `flmbox` process, which this design rules out
  as a default, so it is for clusters that trust the code sandboxes run. The network design above (per-sandbox namespace,
  deny rules, egress allowlist, fixed in-sandbox address) is deferred to its own change, together with the checkpoint
  and restore flow that needs it. The bundle carries a minimal `hosts` file, bind-mounted read-only.
- **No resource limits in `flmbox`.** The bundle has no `linux.resources`; limits belong to the host shim (#604).
- **No state persistence.** Suspend releases the executor and removes the sandbox; a resumed sandbox starts fresh.
- **Kubernetes.** `flmbox` is a host-shim application, so it runs inside the executor manager pod. The pod must be
  privileged (the chart default) so that `runsc` works; the executor manager
  image (`docker/Dockerfile.fem`) carries `runsc`, and the chart mounts an
  `emptyDir` at `${FLAME_HOME}/data/flmbox` for the state directory. The manifest comes from `config/applications`,
  installed by `flmadm` into the session manager image. The Kubernetes e2e runs the sandbox tests against this.

### 4.7 Snapshot persistence

What a sandbox keeps across suspend and resume is what `runsc checkpoint` captures: process memory
and kernel state, and, as the gVisor source suggests, the sandbox's root filesystem changes (files
written and packages installed after it started), because the full checkpoint includes the
filesystem. The spike (4.9) must confirm this end to end. The
snapshot archive is the checkpoint image directory plus a small metadata file (`base`, the `runsc`
version and the CPU feature set). The base image is not in the archive. It is identified by its
digest in the snapshot record (4.4), and restore refuses a node whose base has a different digest.

Both the `runsc` binary and the CPU matter: the same `runsc` binary must restore what it wrote, and
the restoring host needs the CPU features of the checkpointing host (or the sandbox is limited with
the `dev.gvisor.internal.cpufeatures` annotation). The `runsc` version and CPU model are therefore
part of snapshot compatibility, and a mismatch is refused rather than attempted.

**Snapshot store.** Snapshots go to an external object store, not the Flame object cache. The cache
is a per-node disk path and a reference names the owning node, so a snapshot kept there is lost
with that node, which would defeat resume after a failure.

- **Interface:** a `SnapshotStore` trait in `common`, used only by the executor manager (the shim
  uploads, the application manager downloads):

  ```rust
  async fn put(&self, key: &str, file: &Path) -> Result<StoredObject, FlameError>; // multipart, returns size, sha256, etag
  async fn get(&self, key: &str, dest: &Path) -> Result<(), FlameError>;
  async fn delete(&self, key: &str) -> Result<(), FlameError>;
  async fn list(&self, prefix: &str) -> Result<Vec<String>, FlameError>;
  ```

- **Backends:** `s3://bucket/prefix` and `file://`, selected by URL scheme like the existing
  downloader registry. `file://` is for tests and single-node setups. `gs://` is deferred.
  The Rust workspace has no S3 or GCS client today, so this adds a dependency. An existing
  multi-cloud crate such as `object_store` is the likely choice, subject to a dependency review.
- **Configuration:** `FlameCluster` gains `storage.checkpoint` in `flame.yaml`: the store URL, with
  optional endpoint, region and path-style options for S3-compatible stores such as MinIO.
  `cluster.storage` is a plain engine URL today, so it accepts either that string or a map
  `{ engine, checkpoint }` and existing files parse unchanged. `sandbox.images`, `sandbox.network` and `sandbox.egress` are sibling settings (4.2, 4.6). When `storage.checkpoint` is set, the
  executor checkpoints during the unbind of a suspending session (the unbind of 4.5);
  when it is not set, suspend is rejected. Credentials come from the environment or the node or pod
  identity of the executor manager, never from the session or the application, and never enter the
  sandbox. Registration of a suspendable application fails when it is not set.
- **Object:** one archive per snapshot attempt, `<prefix>/<app>/<session>/snapshot-<version>-<attempt>`,
  where the version is the one the commit will produce and the attempt id is unique (4.4). A key is
  never written twice, so a retry or a late attempt can never overwrite a committed object. Large
  archives use multipart upload.
- **Pointer:** the snapshot record of 4.4 (`store`, `key`, `size`, `sha256`, `etag`, `created_at`, `attempt`).
  It records the store URL, so a snapshot stays readable if the configured default changes.
- **Order:** upload the new object, commit the record and version with the state change, then
  delete the old object. A crash leaves either the old record with an orphan new object, or the new
  record with an orphan old object. The MVP leaves such orphans for an operator (4.7 Cleanup).
- **Cleanup:** deleting a session deletes `<prefix>/<app>/<session>/`, and closing one releases its
  snapshot after the close commits. After a commit the previous version's object is deleted, and a
  failed or discarded attempt deletes its own object. A crash between steps can leave an orphan
  object. The MVP has no sweep for these (section 9), so they are removed by an operator, and a
  bucket lifecycle rule is the backstop.
- **Restore path:** the application manager of the executor manager that will host the sandbox
  downloads the object named by the session context to local disk, verifies the SHA-256 against
  the record, and `flmbox` restores from it. The downloaded copy is removed with the unbind.
- **Integrity and access:** the SHA-256 is verified before unpacking. Access is limited by the
  bucket policy to the executor manager identity. Snapshots hold agent memory, so server-side
  encryption of the bucket is expected, and client-side encryption is out of scope (7).

### 4.8 API, storage and compatibility

- `rpc/protos/types.proto`: `SessionState` gains `Suspending = 2` and `Suspended = 3`.
  `SessionStatus` gains `version` and the suspend `attempt`. Application spec gains `suspendable`.
  `SessionContext` gains `snapshot` and the `suspend` target (4.4). `SandboxConfig` is a new message
  carried in the common data of suspendable applications; the Rust and Python SDK contexts expose
  `resumed`. `ExecutorState` is unchanged.
- `frontend.proto`: `SuspendSession` and `ResumeSession` (only `session_id` in the session-level step; `fresh` arrives with the snapshot). `OpenSession` keeps its get-or-create
  meaning and does not resume.
- `backend.proto`: no new RPC, but two messages change. `UnbindExecutor` returns today only `Result`;
  it returns a new `UnbindExecutorResponse` with the `Result` and an optional `suspend` target.
  `UnbindExecutorCompletedRequest` gains `attempt` and an optional `snapshot` or error. The bind response carries the snapshot reference. Because a changed return type is
  not wire compatible with the old one, the session manager and the executor managers are
  upgraded together, as they already are for other backend changes.
- `flame.yaml`: `storage.checkpoint` (URL plus optional endpoint, region and path-style settings),
  `storage` accepting a map form (4.7), `sandbox.images`, `sandbox.network` and `sandbox.egress`.
- SDKs: the Python `flamepy.agent.Sandbox` class (4.10) and the Python session calls it wraps,
  `suspend()` and `resume()`, plus `SessionContext.resumed`. The session-level step (#603) also adds
  `flmctl suspend <session>` and `flmctl resume <session>` and the Rust SDK `suspend_session` and
  `resume_session` on the connection and `suspend` and `resume` on the session.
- Storage migration: the new state values need no schema or format change (the state is the integer
  already stored), so the session-level step has no migration. The snapshot step adds to sqlite a
  version column and a snapshot table; the
  filesystem engine persists them next to the session record; `none` keeps them in memory. Existing stores contain
  only `Open` and `Closed` and decode unchanged. An older client that lists a suspended session
  sees an unknown numeric state, which proto3 preserves.

### 4.9 Feasibility spike (first deliverable)

Before the shim work, prove outside Flame that the target runtime can checkpoint and restore a
stateful sandbox:

1. Run a counter agent in a `runsc` sandbox built from an OCI bundle, driven by a small Rust
   program standing in for `flmbox`.
2. `runsc checkpoint` it, then delete it, and restore it in a new sandbox on
   the same node and on another node.
3. Verify the memory state, that a plain `runsc checkpoint` restores the sandbox
   on an unchanged base image, the filesystem state (a file written and a package installed with
   `apt-get install` after the sandbox started), that `runsc exec` reaches the restored sandbox, and measure snapshot
   size, checkpoint time and restore time, including the upload to and download from the target
   store (S3) at the expected snapshot size.

4. Verify the failure paths: a checkpoint with `--leave-running` leaves the sandbox usable and a
   second kill call removes it; and that a sandbox cannot reach the control plane, the node's local
   services or the metadata address under the chosen network mode, and that a sandbox restored with a rebuilt network is reachable and
   keeps its address.

The result decides whether `flmbox` is viable and fixes how the image becomes an OCI bundle (7). If
`runsc` cannot restore the workload, suspend is not deliverable on this design.

### 4.10 Sandbox SDK: `flamepy.agent.Sandbox`

A new class next to `flamepy.agent.Session` (RFE499), exported from `flamepy.agent`. It is the only Flame surface an agent needs
to use a sandbox: no core `Session`, `Task` or `create_session`, and no application name.
`Sandbox` coexists with `Session`: `Session` stays the stateless script runner on `flmexec`, and
`Sandbox` is the stateful, suspendable one. Neither replaces the other, and `Session` and `flmexec`
are unchanged.

```python
from flamepy.agent import Sandbox

class Sandbox:
    id: str
    state: str                            # "open" or "suspended"

    @classmethod
    def create(
        cls,
        *,
        image: str | None = None,         # sandbox image; default "ubuntu:26.04"
        resreq: ResourceRequirement | None = None,
    ) -> Sandbox: ...

    @classmethod
    def connect(cls, sandbox_id: str) -> Sandbox: ...    # reopen an existing sandbox

    def run(self, cmd: str, *, cwd: str | None = None, envs: dict[str, str] | None = None,
            timeout: float | None = None) -> CommandResult: ...
    def submit(self, cmd: str, *, cwd: str | None = None, envs: dict[str, str] | None = None,
                   timeout: float | None = None) -> Future[CommandResult]: ...
    def run_code(self, code: str, *, language: str = "python", input: bytes | None = None,
                 timeout: float | None = None) -> CommandResult: ...
    def submit_code(self, code: str, *, language: str = "python", input: bytes | None = None,
                    timeout: float | None = None) -> Future[CommandResult]: ...
    def suspend(self) -> None: ...
    def resume(self, *, fresh: bool = False) -> None: ...
    def close(self) -> None: ...           # destroys the sandbox and its snapshots
    def __enter__(self) -> Sandbox: ...
    def __exit__(self, *exc) -> None: ...  # closes, as Session does
```

`create` and `connect` are class methods, as in E2B, so the network calls are named and not hidden
in a constructor. `CommandResult` is `exit_code: int`, `stdout: bytes`,
`stderr: bytes` with `text()` helpers, and is the result of both `run` and `run_code`.

`language` is chosen per call (`"python"`, `"shell"`), so one sandbox can run both. The sandbox
itself fixes the image: `image` is written into the session's `SandboxConfig` common data (4.4) and is the base
image of the OCI bundle (4.6); when omitted, the default is `ubuntu:26.04`. The
image cannot change after creation, and a reopened sandbox keeps the image recorded in its
snapshot (4.4). There are no file transfer calls in this version.

**Mapping to the session API**

| Sandbox call | Effect |
| ------------ | ------ |
| `Sandbox.create(...)` | Creates a session on the built-in sandbox application with `min = max = batch_size = 1`. |
| `Sandbox.connect(id)` | Reopens it (`OpenSession`); a suspended sandbox opens as `suspended` and is not resumed implicitly. |
| `run` / `submit` | A `run` task: `flmbox` executes the shell command in the sandbox with `runsc exec`. |
| `run_code` / `submit_code` | A `run_code` task on the session, with the call's `language`. |
| `suspend` / `resume` | `SuspendSession` / `ResumeSession`; `resume(fresh=True)` discards the snapshot. |
| `close` | `CloseSession`, which releases the snapshot (4.7). |
| `state` | Read from the session state (`Open` is `"open"`, `Suspended` is `"suspended"`; `Suspending` is reported as `"open"` until it completes, and `suspend()` returns only when the state is `Suspended` or fails). |

Calls that need the running sandbox (`run`, `run_code`, `submit_code`)
on a suspended sandbox raise `FlameError` with an invalid-state code, matching the
rejected tasks of 4.3. Errors use `FlameError`, as in `Session`.

**Backing application.** A built-in application (for example `flmbox`, registered like
`flmexec`) with `shim = Host`, `suspendable = true` and a longer `delay_release` (4.2), written with the Rust SDK. Its service runs `runsc` and
handles one task type, a script in `shell` (the default) or `python` with optional `cwd`, `envs` and stdin, which `run` and `run_code` both send. It is separate from
`flmexec`, which is stateless and has no sandbox.

**`run`.** Runs a shell command in the sandbox, as `commands.run` does in E2B. It returns a
`CommandResult` (`exit_code`, `stdout`, `stderr`); a nonzero exit code is a result and not an
error, and `FlameError` is raised only when the sandbox itself fails. Anything an agent needs,
including installing packages (`run("apt-get install -y jq")`), is a command, so Flame has no
package manager abstraction. The sandbox needs a route to the package repositories (7). Installed
packages are part of the sandbox filesystem state, so whether they survive suspend and resume is the
checkpoint's job (4.7), which the spike verifies.

**`run_code`.** Runs code in the sandbox. Process and filesystem state survive suspend and resume to
the extent the `runsc` checkpoint preserves them. This design does not add its own persistence layer
for interpreter state: a long-lived interpreter or shell per sandbox keeps its variables across
calls and across suspend and resume only because the process is checkpointed. Whether v1 keeps a
long-lived interpreter or starts a process per call is an implementation choice that the spike
informs.

**Resumed signal.** `SessionContext.resumed` (4.5) is available to the service inside the sandbox,
so the sandbox application can re-establish anything that cannot be checkpointed.

**As implemented in phase 1.** `Sandbox.state` is read from the server and is one of `"open"`, `"suspending"`,
`"suspended"` or `"closed"`. `suspend()` and `resume()` signal the session and then poll (every 0.2 s, default
timeout 60 s) until it is `suspended` or `open`; a timeout raises `FlameError(INTERNAL)` and a closed sandbox
raises `FlameError(INVALID_STATE)`. `run`, `run_code` and their `submit_` forms send the command without asking for the state: the
server rejects it with a `FlameError` ("Session <id> is closed") for a sandbox closed elsewhere and queues it for a
suspending or suspended one until the resume (the call waits), and a handle raises `FlameError(INVALID_STATE)` after `close()`. `resume(fresh=...)` is not implemented because there is no
snapshot to discard yet, and a resumed sandbox starts fresh.

## 5. Failure handling

| Failure | Behavior |
| ------- | -------- |
| Admission check fails | Error to the caller, no state change. |
| Checkpoint or upload fails or times out | Retried a bounded number of times, then reported. The sandbox is kept running, the session returns to `Open`, and the failed attempt's object is deleted. If the scheduler binds the same executor again the agent reattaches with its state (4.5). If the executor is released first, the session restarts from its last committed snapshot, or fresh, and state since is lost. |
| Session manager restarts in `Suspending` | The session stays `Suspending` when it is loaded, and the next scheduler cycle completes it once no executor serves it. |
| A task runs for a long time, or an executor never unbinds | The session stays `Suspending` until the executor stops serving it; new tasks still queue (4.3). |
| Tasks are submitted to a suspending or suspended session | Accepted and `Pending`; they run after the resume. |
| Executor manager or `flmbox` restarts during suspend | Treated as executor loss (next row). |
| Checkpoint exceeds its time limit (checkpoint step) | The session manager fails the suspend as above. A late completion has a stale attempt id and is discarded, and its object deleted. |
| Retry races a late completion | They carry different attempt ids and different object keys. Only the current attempt commits (4.4). |
| Close while `Suspending` | The close wins. The completion that arrives later is discarded and its object deleted (4.1). |
| Executor or node dies during suspend | At session level the executor no longer serves the session, and the suspend completes if it was the last one. With a checkpoint (later step) the sandbox is lost and the suspend failed: the session returns to `Open` and restarts from its last committed snapshot, or fresh if there is none. |
| Snapshot store unreachable on suspend | Same as a failed upload. |
| A suspended session is never resumed | It stays suspended and keeps its snapshot until closed. It counts in `limits.max_sessions` and is never evicted. |
| Session created with a disallowed image, or with `snapshot_path` or `checkpoint_path` set | Rejected with `InvalidConfig` (4.2). |
| Snapshot store unreachable on resume | Bind fails through `bind_executor_failed` and is retried with the session retry limit; the record is untouched. |
| Snapshot missing or checksum mismatch on resume | Bind fails through `bind_executor_failed`; the session stays `Open` and retries. `ResumeSession --fresh` discards it. |
| Close or delete while suspended | The snapshot is deleted after the state change commits. |
| Two executors restore the same snapshot | Resume is serialised by the session state, and the scheduler binds one executor to a session. The session version guards a stale restore. |

## 6. Verification

- Controller and storage: transitions and rejected transitions; suspend with a running task lets it
  finish and then unbinds; tasks can be submitted and completed in every state but `Closed`; suspend
  rejected for a non-host application, without `storage.checkpoint`, and for a non-suspendable one
  (checkpoint step); close and delete while suspending or suspended; recovery in `Suspending` (stays
  `Suspending`) and `Suspended`; no executor references a session missing from the scheduler view; a long-running
  task keeps the session `Suspending` until it finishes; migration of existing sessions (checkpoint step).
- Security: a session whose common data sets `snapshot_path` or `checkpoint_path`, or whose image is
  not in the allowlist, is rejected; the executor manager overwrites both paths in the copy it
  passes; `flmbox` refuses an image over `sandbox.max_image_size`; a sandbox cannot reach the
  control plane or the metadata address.
- Suspend attempts: a retry gets a new attempt id and key; a late completion of an older attempt,
  and a completion after close, are discarded and their objects deleted; the version increments
  only at commit.
- Failed suspend: the sandbox is kept running; if the same executor is bound again `flmbox`
  reattaches with its state intact; after the executor is released the session restarts from its
  last snapshot; a sandbox of another session is killed at `on_session_enter`.
- Executor states (both sides): no new state; a suspended session unbinds like a closed
  one and its executor is retained by `delay_release`; the unbind response carries the suspend target and the executor manager writes the marker; a stale completion is ignored; restart
  recovery while unbinding for suspend.
- Session context: built from the version (no snapshot at version 0 or with no executor in the
  record, the record's snapshot otherwise); the application manager downloads and verifies it before
  `on_session_enter`.
- Scheduler cost: `Storage::snapshot` excludes `Suspended` sessions, resume adds the session back
  before the scheduler is notified, and cycle time stays flat as suspended sessions grow.
- Scheduler: a suspended session has demand 0 and unbinds its executor, which stays warm for `delay_release`; resume rebinds an executor, which may be the warm one (no affinity in the MVP);
  `wait_for_task` unbinds on suspend; a suspended session is not charged by DRF.
- `flmbox` and the directory contract: `on_session_enter` with and without `snapshot_path`; checkpoint into
  `checkpoint_path` and failure; base digest or `runsc` version mismatch refused; `runsc exec` of
  tasks; network setup and teardown; the second `on_session_leave` call kills the sandbox and a failed suspend never makes it. Upload and unpack in the executor manager.
- Hook contract: `flmbox` keeps `SandboxConfig` in `self` and `on_session_leave` reads it there;
  the `suspend` marker selects checkpoint or kill; a suspend with no executor leaves the earlier
  snapshot and version untouched; allowlist entries pull by digest; `resumed` is false on a
  reattach; a restart removes leftover sandboxes and namespaces.
- Resource group: limits are set from the executor's requirement and inherited by sandboxes;
  `destroy_instance` and an executor-manager start kill a leftover sandbox; one executor's cleanup
  never touches another's; startup fails clearly without a delegated cgroup v2.
- Persistence: version and pointer ordering, deletion of the old, failed and discarded objects, SHA-256 mismatch,
  store errors on put, get and delete, and the `file://` backend. The S3 backend runs against a
  local S3-compatible server in CI.
- SDK: `Sandbox.create`, `Sandbox.connect`, `run`, `submit`, `run_code`, `suspend`, `resume`, `close` against a fake
  session API (unit), and the error for calls on a suspended sandbox.
- End to end: an agent runs `run("apt-get install -y ...")`, writes a file, suspends, resumes and
  then runs code that uses the package and reads the file back with `run_code`, including
  a resume on another node, against a local S3-compatible store (MinIO), and registration is rejected
  when no snapshot store is configured.

## 7. Open questions

1. Should a task created on a `Suspended` session resume it automatically, controlled per session?
2. Should `watch_task` keep working across a suspend? It does while the session is not closed.
3. How does `flmbox` pull and unpack an image (a Rust registry client, or `skopeo` and `umoci` on
   the node), and is `runsc` provisioned on every node? The spike answers this.
4. Client-side snapshot encryption and per-session access control, beyond bucket policy and
   server-side encryption.
5. How large are snapshots in practice, and what limits and timeouts do they need? The spike
   measures it, and the limits are deferred (section 9).
6. Should a `Suspended` session's tasks be kept out of memory until it resumes, to cut the memory
   and startup cost of many suspended sessions (4.3)?
7. How does the sandbox reach package repositories for `apt-get` and `pip` under the network rules
   of 4.6: direct egress, a cluster-configured mirror, or an egress proxy? Offline clusters need a
   mirror.
8. Is `object_store` (or another multi-cloud client) acceptable as a new dependency, and is a
   `file://` backend enough for single-node and test setups?
9. After a failed suspend the sandbox is kept, but nothing steers the session back to its executor
   in the MVP. Is the chance of losing it acceptable until placement is added (section 9)?
10. Which sandbox image and user does `flmbox` use, so that `apt-get install` works (root or
    a sudo-capable user) without weakening isolation?
11. Is a delegated cgroup v2 subtree available on the target nodes, and is Linux 5.14 or later
    (for `cgroup.kill`) a safe minimum?
12. Should a suspend have a deadline that returns the session to `Open`? The session-level step has
    none: a suspend that cannot complete stays `Suspending` (the shuffle action completes it each cycle).

## 8. Rollout and rollback

- Everything is opt-in through `suspendable`. Without it no new code path runs, and the new
  proto fields and enum values are unused.
- Land in order: (1) session suspend and resume (#603: states, storage, controller, API, `flmctl`, SDK
  calls), (2) spike, (3) the snapshot record, version, attempt and session context with a fake sandbox, (4) `SnapshotStore` with the `s3://`
  and `file://` backends, (5) `storage.checkpoint`, the snapshot and checkpoint directories and
  `flmbox` (with #604 the executor resource group), (6) the built-in sandbox application and the Python
  `flamepy.agent.Sandbox`, (7) end-to-end tests.
- The `UnbindExecutor` response type changes (4.8), so the session manager and all executor managers
  are upgraded together.
- Rollback: stop registering suspendable applications and resume or close the suspended sessions.
  A store that contains `Suspended` sessions cannot be read by a build without this change, so
  rollback requires resuming or closing them first.

## 9. Deferred beyond the MVP

Each item is independent and can follow once the MVP works.

- **Data-aware placement of a resumed session (RFE518).** `flmbox` would publish a locality key for
  each snapshot and for its live sandbox, sessions would gain generic session-level affinity that
  `DasPlugin::setup` unions with pending-task affinity, the controller would set it on resume and
  after a failed suspend, and a successful `OnSessionLeave` response would carry published
  attributes. It also makes the reattach after a failed suspend reliable.
- **Local snapshot cache.** The executor manager would keep the checkpoint or downloaded snapshot
  for the executor's life, skip the download on a hit, and evict under
  `executors.snapshot_cache_bytes`. A longer `delay_release` for `flmbox` would widen the window.
- **Limits and expiry.** `snapshot.max_size`, `snapshot.suspended_ttl`, `sandbox.max_image_size`,
  a size limit on common data, and a cap on suspended sessions.
- **Orphan sweep.** A periodic sweep of the snapshot store with a grace period.
- **Timeouts for session enter and leave.** Nothing bounds `on_session_enter` or `on_session_leave`
  today: `GrpcShim` awaits the call with no deadline, and the only timeouts on that path are 30
  seconds for the service socket and for the connection. A stalled pull, restore or checkpoint
  would keep the executor in `Binding` or `Unbinding` for ever. This predates suspend and affects
  every application. The MVP adds no deadline for them. A fix would add
  per-application deadlines in the executor manager and make `flmbox` stop its work when the request
  is cancelled.
- **GCS backend.**
- **Smaller snapshots.** Splitting the filesystem delta out of the checkpoint, once the sizes from
  the spike justify it.
- **Automatic resume** of a suspended session by a new task, and **automatic suspend** on idle.
- **Tasks out of memory** for suspended sessions (4.3), and per-session access control and
  client-side encryption of snapshots (7).

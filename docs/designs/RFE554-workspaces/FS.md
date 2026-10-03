# Design: Workspace-scoped names

Tracking issue: [#554](https://github.com/xflops/flame/issues/554).

## 1. Scope

Applications and sessions currently have global names. Storage, scheduler
maps, executor bindings, cache keys, and filesystem paths assume that one
local name identifies one resource in the cluster. A Workspace makes those
names local to a namespace, allowing two workspaces to use the same
application and session names.

This RFE adds persistent Workspaces, a workspace field in resource metadata,
workspace-local name indexes, storage and cache paths,
and initialization of a fresh installation with `default`. RPC references remain names.
There is no ApplicationGID, SessionGID, or TaskGID string contract.
Internally, `SessionGID { workspace, session }` and
`ExecutorGID { workspace, executor }` are typed map keys for filesystem locks;
`SessionGID` also keys event maps and is passed through session-scoped helpers
in the filesystem engine, event manager, controller, and storage `Engine` trait.
`Engine` session/task operations take `&SessionGID`; executor lookup/state/delete
operations take `&ExecutorGID`. Resource-bearing creation/update operations
carry scope in their existing models. They are not RPC
references or metadata IDs.
`TaskName` aliases the internal `u64` task name. `TaskFilter` stores one
mandatory `SessionGID`; use `TaskFilter::new(gid).by_state(state)` or
`.by_states(states)`, and `session()` borrows that owner. Task filtering is
always scoped to one session. `SessionFilter` requires a workspace and uses
`SessionFilter::new(ws).by_state(state).by_names(names).by_application(app)`.
Its `session()` helper returns explicitly named sessions in that workspace,
preserving multiple names and empty name lists. Queries never span workspaces.
`ApplicationFilter` also requires workspace and uses `ApplicationFilter::new(ws).by_state(state)`. Internal scheduler,
cleanup and cache GC enumerate workspaces before issuing scoped queries.
`Executor::gid()` returns its `ExecutorGID`.
`Session` and `SessionInfo` expose `gid()`; `Task`, `TaskInfo`, `Executor`, and
`ExecutorInfo` expose `session()`. Executor variants return `None` when unbound.

Resource-name grammar is validated only at frontend RPC ingress. Controllers,
filters, storage engines, and recovery consume those established names.

A Workspace is a namespace, not an access boundary. Any client that can
reach the current API can name, list, or operate on any workspace under the
existing deployment policy. Client authentication and authorization are
separate work under [#392](https://github.com/xflops/flame/issues/392).

## 2. Metadata and Workspace API

Add `optional string workspace = 3` to the existing `Metadata` protobuf
message; preserve field 1 (`id`) and field 2 (`name`). Workspace itself is a
plain record and does not use Metadata. For resources that do, apply this
contract:

| Resource | `metadata.id` | `metadata.name` | `metadata.workspace` |
| --- | --- | --- | --- |
| Application | Server-generated UUID | Local application name | Containing workspace name |
| Session | Server-generated UUID | Local session name | Containing workspace name |
| Task | Server-generated UUID | Decimal task number local to its session | Parent session's workspace name |
| Node | Server-generated UUID | Global node name | Unset |
| Executor | UUID (already generated for new executors) | Existing executor name | Executor's workspace name |

An application or session UUID is immutable for that resource lifetime and
serves only as debug metadata. Its name is also immutable in this RFE; a future
rename needs separate semantics. For tasks, `metadata.name` is the canonical
decimal representation of the task number, unique within its parent session.
A task response carries its parent session's local
name in `TaskSpec.session` and the workspace in `Task.metadata.workspace`.
The session manager indexes scoped resources by workspace and local name,
and tasks by workspace, session name, and task name. Every resource that has a `Metadata` message has a UUID
`metadata.id`; Workspace has no Metadata. Node remains global, with
`metadata.workspace` unset. Executor metadata carries its workspace. A Node
gets its UUID on first registration and retains it across updates, while its
global name is the lookup key. Executor UUIDs are persisted debug metadata;
its name is the lookup key.

Workspace, application, and session names use the same ASCII character rule:
`[a-z0-9]` at both ends and `[a-z0-9_-]` between. Workspace names are 1–63
characters; application and session names are 1–253 characters. Reject
uppercase, dots, slashes, backslashes, percent escapes, empty names, and
control characters. These names are safe as directory components. Bootstrap a
Workspace named `default`. Workspace deletion and renaming are outside this RFE.

Add frontend `CreateWorkspace` and `ListWorkspaces` RPCs. `CreateWorkspace`
takes a Workspace name, validates it, sets `create_at` to the creation
timestamp, and returns the new Workspace; duplicates return `AlreadyExists`.
The validated Workspace name is its primary key. `ListWorkspaces` returns
all Workspaces in stable name order. There is no `GetCurrentWorkspace` RPC:
this RFE defines no caller identity from which to infer one.

```protobuf
rpc CreateWorkspace(CreateWorkspaceRequest) returns (Workspace) {}
rpc ListWorkspaces(ListWorkspacesRequest) returns (WorkspaceList) {}

message CreateWorkspaceRequest { string name = 1; }
message ListWorkspacesRequest {}
message Workspace {
  string name = 1;
  int64 create_at = 2;
}
message WorkspaceList { repeated Workspace workspaces = 1; }
```

## 3. RPC names and conversion boundary

Keep the meaning and field numbers of existing RPC references. Fields such
as `RegisterApplicationRequest.name`, `SessionSpec.application`,
`ExecutorSpec.application`, and shim context
names continue to carry **local names**, not internal IDs. Add a separate
`workspace` field to requests and containing messages that need one. Storage,
controller, scheduler, and executor state use names as references and indexes,
with workspace carried alongside every scoped name. Responses populate
metadata UUIDs for debugging and local names for normal use.

Rename the following protobuf fields while preserving their field numbers and
local-name or local-number values. `CreateSessionRequest` uses `name` for the
new session's local name and `spec` for its configuration. These are generated
API name changes; regenerate Rust/Python bindings and update callers. Rename
remaining request fields that carry session names from `session_id` to
`session`, preserving their field numbers. Across RPC and internal models,
bare resource fields (`session`, `task`, `application`, `node`, `executor`)
contain names; `ssn_id`, `task_id`, `app_id`, `node_id`, and `executor_id`
contain `metadata.id` UUIDs.

```protobuf
message CreateSessionRequest {
  string name = 1;  // New session's local name.
  SessionSpec spec = 2;
  // A new workspace field uses a new number.
}
message TaskSpec {
  string session = 2;  // Local session name in the task's workspace.
  // Existing fields 3-5 keep their numbers.
}
message ExecutorStatus {
  ExecutorState state = 1;
  optional string session = 2;  // Local bound session name.
}
message GetTaskRequest {
  string task = 1;     // Local task number as decimal text.
  string session = 2;  // Parent session's local name.
  // A new workspace field uses a new number.
}
message WatchTaskRequest {
  string task = 1;     // Local task number as decimal text.
  string session = 2;  // Parent session's local name.
  // A new workspace field uses a new number.
}
// The existing field 1 in DeleteSessionRequest, OpenSessionRequest,
// CloseSessionRequest, GetSessionRequest, and ListTasksRequest becomes
// `session`, since its value is the local session name.
```

Add a required workspace field (a nonempty string validated on receipt) to
application create/update/delete/get requests, session create/open/close/
delete/get requests, and task create/get/watch/list requests. Keep the
remaining reference field numbers; use new field numbers for
workspace. `CreateTaskRequest.workspace` scopes its
`TaskSpec.session`. Every `WatchTaskRequest` registration carries the
same workspace and session name, and the stream cannot switch parent
sessions. `GetTaskRequest.task` and `WatchTaskRequest.task` are decimal
local task names, looked up within the parent Session;
their `session` fields name that task's parent session
within the request's workspace.

`ListApplicationsRequest.workspace`, `ListSessionsRequest.workspace`, and
`ListExecutorsRequest.workspace` are
optional filters. With no workspace filter, return all matching resources,
as current global list calls do. A `ListSessionsRequest.application` local
name filter requires a workspace filter, so it identifies one application.
Apply state filters together with workspace filters. These are selection
rules, not caller authorization.

Use `Executor.metadata.workspace` for the local application reference in
`ExecutorSpec.application` and the bound `ExecutorStatus.session`.
`ExecutorSpec` does not contain workspace. Executor registration resolves
workspace from the stored executor selected by its name. Add workspace fields to
`ApplicationContext`, `SessionContext`, and `TaskContext`, retaining their
existing local-name fields. Backend requests that identify only a global
node or executor name need no workspace. Bind responses include Application
and Session metadata so the receiver can recover workspace and local names.

On input, reject an unknown Workspace or a local name that violates the shared
name rule. When a full resource message is supplied, validate
that its `metadata.id`, `metadata.name`, and `metadata.workspace` agree with
the stored resource or the requested operation. Do not accept a caller ID as
authority to select a different resource.

Every structured Flame reference from an application, session, or task must
stay in that resource's workspace:

* Resolve a session's `SessionSpec.application` only inside the session's
  workspace, then store its local application name and workspace.
* Resolve a task's `TaskSpec.session` only inside the task's workspace,
  then store its local session name, workspace, and decimal task name.
* Bind executors to the stored application's workspace; resolve a session
  binding in that same workspace.
* For a Flame cache package/object URL attached to a resource, parse its
  workspace and reject a different workspace. External URL schemes and
  opaque application payloads are not Flame resource references.

Reject a cross-workspace structured reference as `InvalidArgument` before
persistence or bind. Repeat the invariant at internal write boundaries so a
backend caller cannot bypass frontend validation. A user may still address
another workspace as a top-level operation until authorization is added;
same-workspace references do not provide access control.

## 4. Name indexes and persistence

Application and Session are keyed by `(workspace, name)`, Task by
`(workspace, session, name)`, Node by global `name`, and Executor by its name.
The controller and scheduler use the same scoped names for references.
`metadata.id` is a generated, persisted UUID for debugging only; no operation
selects or links a resource by that UUID. Remove the legacy `TaskID` and
`SessionID` aliases. Application and Session records carry `id`, `name`, and
`workspace`; Session and SessionAttributes carry local `application` and no
`app_id`. Task carries UUID `id`, numeric local `name` (`u64`), local parent `session`,
and `workspace`, with no `ssn_id` or `TaskNumber`. Executor carries local
`application`, `session`, `task`, and `node` names and its `workspace`.
`EventOwner` contains workspace plus local `session` and optional local `task`.

An application name is unique among applications in its workspace; a session
name is unique among sessions in its workspace. The same local name may occur
in another workspace. Keep names reserved while disabled applications and
closed sessions still exist. Create atomically claims the scoped name and
returns `AlreadyExists` on a duplicate, including concurrent creates.
Deleting the resource releases the name; a later resource with that name gets
a new debug UUID. Within Flame, keep the task's local `name` as `u64` and key
the Session task lookup and ordered state indexes by that number. Encode it as
decimal text in RPC fields. The first pending index entry is the oldest task
(`2` precedes `10`).
`EventOwner.task = None` denotes a session-level event.

SQLite adds a `workspaces` table keyed by `name`, with `create_at`.
Applications and sessions have composite primary keys `(workspace, name)`;
tasks have `(workspace, session, name)`. Their `id` columns persist UUID debug
metadata without acting as foreign keys. `sessions.application` refers to
`applications(workspace, name)`, and tasks refer to
`sessions(workspace, name)`. Executor reference columns store local names with
workspace; nodes use global name keys. The in-memory and filesystem engines
implement the same atomic uniqueness and lookup contract. The filesystem
task data file may use a parsed numeric task name as its fixed-record offset;
task metadata persists its UUID separately.

Filesystem records are grouped by validated workspace and local names:

```text
<base>/workspaces/<workspace>/workspace.json
<base>/workspaces/<workspace>/applications/<application>/metadata
<base>/workspaces/<workspace>/sessions/<session>/metadata
<base>/workspaces/<workspace>/sessions/<session>/tasks.bin
<base>/workspaces/<workspace>/sessions/<session>/inputs.bin
<base>/workspaces/<workspace>/sessions/<session>/outputs.bin
```

Persist debug UUIDs inside metadata; do not derive a new UUID from a path on restart.
Keep other existing session data files under the same session directory.
Node directories remain global and Executor metadata carries its workspace.
Persist Node and Executor UUIDs in metadata; retain name-based directory
layouts. Validate each name before
joining filesystem paths.

## 5. Cache names and initialization

The canonical cache key is `<workspace>/<application>/<session>/<object>`.
Put prefixes use `<workspace>/<application>/<session>` and wildcard
deletion uses `<workspace>/<application>/*`; only the session component may
be `*`. Package keys use `<workspace>/<application>/pkg/<object>` and shared
keys use `<workspace>/<application>/shared/<object>`. These are workspace and
local names, not internal FSM IDs. Cache disk directories follow the same
validated components. Rust/Python ObjectKey helpers accept separate
workspace and local names and emit the workspace exactly once. ObjectRef
keys and object/package URLs carry that key; endpoint scheme and host stay
unchanged.

Cache GC obtains the existing full `ListApplications` response and indexes
its snapshot by `(metadata.workspace, metadata.name)`, not by local name
alone. It compares each cache key with the application in the same
workspace. Its creation-time and grace-period behavior remains as in
[RFE540](../RFE540-cache-owned-application-garbage-collection/FS.md).

This RFE targets a fresh installation. Initialize the final SQLite schema and
workspace filesystem/cache layouts directly, and create `default` with its
`create_at` timestamp. Bootstrap application manifests loaded by the session
manager belong to `default`. Existing data conversion and compatibility with
older persisted layouts are outside this RFE. Restart recovery reads data
written using the current workspace layout.

## 6. Clients and verification

Update Rust/Python SDK models, generated protobuf bindings, session/task
clients, and cache helpers for separate workspace and local-name fields.
SDK results expose `metadata.id`, `metadata.name`, and
`metadata.workspace`; callers continue to pass local names with workspace
when making RPCs. `flmctl` accepts local names with an explicit workspace
selection (defaulting to `default` in its client context), sends name and
workspace separately, and displays Workspace and Name columns. Add
workspace create/list commands. Update Host/CRI shim contexts, manifests,
Helm examples, and E2E fixtures. This RFE changes no certificate, listener,
or TLS configuration.

Verification covers:

1. Shared workspace/application/session name syntax and their distinct length
   limits; Workspace creation, duplicate handling, persistence, stable list
   order, and default bootstrap.
2. Equal local application and session names in two workspaces. Confirm
   `(workspace, local name)` selects the correct resource, and storage,
   controller, scheduler, event, and executor references use scoped names.
3. RPC round trips retain local reference names and separate workspace
   fields. Confirm `CreateSessionRequest.name`/`spec`,
   `TaskSpec.session`, `ExecutorStatus.session`, and both `task`/`session`
   fields in GetTaskRequest and WatchTaskRequest use their specified field
   numbers and values. Confirm RPC and internal `workspace` fields carry the
   same validated Workspace name.
   Confirm responses carry the debug UUID, local name, and
   workspace; reject inconsistent metadata or unknown workspaces.
   Confirm every Metadata.id, including Node and Executor, parses as UUID.
   Re-registering a Node by name retains its UUID.
4. Concurrent duplicate application and session creates in one workspace
   yield one resource and `AlreadyExists` for the others. Identical names
   in different workspaces succeed; deleting and recreating a name yields a
   new ID.
5. Reject session-to-application, task-to-session, executor-binding, and
   Flame cache URL references that cross workspace boundaries. Verify task
   watches keep one parent workspace/session.
6. Initialize empty SQLite, filesystem, and cache stores. Persist resources
   with the same local names in different workspaces, restart, and verify
   names, UUID metadata, references, task ordering, and payloads are preserved.
7. Exercise SDK, CLI, Host/CRI, and E2E paths, including cache URLs with
   exactly one workspace component and GC with repeated local app names.

## 7. Implementation map

| Area | Changes |
| --- | --- |
| `rpc/protos/{frontend,types,shim}.proto` and SDK proto copies | Workspace RPCs, metadata workspace, and separate request workspace fields |
| `common/src/apis`, `common/src/storage`, SQLite initialization schema | Scoped name indexes, Workspace persistence, and debug UUID metadata |
| `session_manager/src/apiserver`, controller, scheduler | Scoped name references and indexes |
| `executor_manager` and shim contexts | Local-name references with explicit workspace |
| `object_cache/src/{cache,gc,storage}` | Workspace key grammar, disk paths, and GC snapshot keys |
| `sdk/{rust,python}`, `flmctl`, `charts/flame`, `e2e` | Workspace/name fields, metadata, CLI commands, examples, and regressions |

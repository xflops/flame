# CRI executor shim

Tracking: [#525](https://github.com/xflops/flame/issues/525)

Status: **Implemented.**

### Review decisions

The implementation follows these reviewed decisions:

1. **Explicit CRI shim.** Add `Shim::Cri` and select it only through
   `cluster.executors.shim: cri`. Applications intended for container execution
   also declare `shim: cri`, preserving the existing exact-match scheduler and
   executor binding contract.
2. **Containerd owns runtime policy.** Flame has no `executors.cri` block. It
   uses the conventional local containerd CRI socket and leaves
   `runtime_handler` empty, so containerd selects its configured default
   runtime, CNI, cgroups, registry credentials, and gVisor policy. The platform
   must make the object-cache data plane and configured session-manager endpoint
   reachable from CRI sandboxes while keeping unrelated control-plane and VPC
   services unreachable; Flame does not configure routes, DNS, or network
   policy.
3. **Unix socket transport (recommended, with an explicit gVisor exception).**
   Preserve the current Rust/Python Instance API by mounting one private
   per-executor directory and allowing the sandbox to create its service UDS
   there. For gVisor this requires `runsc host-uds=create`; the default
   `host-uds=none` will not work. `create` is narrower than `all`: the sandbox
   may bind a host-visible UDS only below an exposed mount, but may not open
   arbitrary host UDS paths. The alternative is adding TCP support to both SDK
   service implementations and discovering the sandbox IP.
4. **Controller-driven terminal recovery (recommended for v1).** Session
   manager persists executor IDs. After an executor-manager restart, it sends
   missing IDs back as targeted `Releasing` work. The executor-manager run loop
   asks the configured shim to destroy that exact service once. Interrupted
   tasks fail in v1; neither component retries or replaces the service.
5. **One image container per executor (recommended for v1).** Reuse
   `ApplicationManager` for applications with a package URL, propagate its
   runtime environment, and expose the installed release, Python runtime, and
   installer caches at executor-local container paths. Image-only applications
   skip installation. GPU requirements remain unsupported in v1.
6. **Layered implementation.** Keep the reviewed CRI model/manager independent
   of executor-manager integration and test its runtime transaction directly.
7. **Applications are configuration.** Session manager seeds applications from
   manifests next to `flame-cluster.yaml`; it has no compiled-in application
   definitions. A manifest owns only the application name it declares, so it
   updates that stored definition without deleting unrelated stored
   applications.

## 1. Motivation

### Background

Before this work, executor manager launched Host applications as child
processes and Wasm applications in-process. The `cri-rs` crate could call a CRI
v1 runtime, but it was not connected to executor manager and its public model
was not suitable for recovery: desired container configuration and observed
runtime state shared one type, status conversion lost runtime identity, and
list calls were not scoped to Flame-owned workloads.

The CRI shim launches a containerized application which implements the same
Instance gRPC API as a Host application. `Shim::Cri` nevertheless remains an
explicit scheduling contract: it states that the application must be launched
as a CRI workload and that the executor supports that lifecycle.

### Target

Add `Shim::Cri` to executor manager and the application/executor shim contract.
One CRI workload is created for one application executor and reused across all
of that executor's session bindings. The implementation keeps the `cri-rs`
model/manager API separate from its executor-manager adapter even though both
are delivered by this change.

Success means:

- applications and executor manager can select `shim: cri` through the existing
  shim contract;
- partial creation is rolled back and deletion is idempotent;
- no operation can list or delete another CRI consumer's workload;
- CRI sandboxes can reach their configured session-manager endpoint, cache, and
  every owning cache endpoint carried by an `ObjectRef`, while unrelated
  control-plane services remain isolated; and
- an application container cannot acquire more host access than the explicit
  CRI shim policy allows.

## 2. Function Specification

### Configuration

CRI adds one value to the existing shim setting. The application and executor
manager both select it:

```yaml
cluster:
  executors:
    shim: cri

# Application specification
shim: cri
image: registry.example.com/flame/runner:latest
```

`host` remains the default. `cri` uses `/run/containerd/containerd.sock` and
sends an empty `runtime_handler`, selecting containerd's configured default.
Flame does not configure or override the runtime handler, CNI, DNS, cgroup
driver, registry credentials, or gVisor flags. Those are node prerequisites
owned by the platform/containerd administrator. Applications cannot override
them. In particular, the platform must provide routing, name resolution, and
firewall or network-policy access from the sandbox to the configured cache and
the cache endpoints stored in ObjectRefs. The configured session-manager
endpoint must also be reachable for recursive Flame operations such as Runner
or flmexec. Unrelated control-plane services and general VPC access remain
isolated by platform policy.

Executor manager injects `FLAME_CACHE_ENDPOINT` using an address reachable from
the sandbox network namespace and injects `FLAME_ENDPOINT` using the configured
cluster endpoint. The latter enables recursive Runner and flmexec operations;
normal Instance callbacks still use the executor-local UDS. Both configured
endpoints must therefore be non-loopback and reachable from the sandbox. An
ObjectRef is resolved against its owning cache endpoint unless the SDK is
configured with a cache proxy.

The CRI shim uses fixed, documented safety defaults: query CRI image status and
pull only when the image is absent, 30 seconds for startup, 10 seconds for stop,
and `/var/log/flame/executors` as the log root. Changing these policies requires
a separate design rather than an open-ended CRI configuration block.

For a Runner-created application, `ApplicationSpec.url` and `installer` are
handled by the existing `ApplicationManager` before the CRI workload starts.
The resulting release directory contains both package source and installed
dependencies. CRI mounts that directory read-only at
`/var/lib/flame/executors/<executor-id>/installation`. For Python packages, it
also mounts the selected Flame runtime site-packages read-only at
`.../runtime/site-packages` and mounts the shared UV and pip caches read/write
at `.../cache/uv` and `.../cache/pip`. Installer-produced `PYTHONPATH`, `PATH`,
`LD_LIBRARY_PATH`, `FLAME_APP_DIR`, `UV_CACHE_DIR`, and `PIP_CACHE_DIR` values
are rewritten to these pod paths. Host-only path entries outside the declared
mounts are discarded; application-defined container paths retain their normal
precedence. This keeps one installation implementation without exposing its
host paths inside the pod. All mount sources must be host paths visible to
containerd. An application with only `image` and no package URL does not invoke
an installer.

The image is responsible for the container-side Flame executable layout and
must define any values used by its command, such as `FLAME_HOME`. Commands and
arguments are expanded only from the final container environment; an undefined
variable is a configuration error. Because package installation occurs on the
host, the selected image must match the host architecture, libc, Python minor
version, and extension ABI. A separately built image with incompatible native
dependencies is unsupported by this v1 design.

### Application manifests

Applications are configured as YAML rather than compiled into session manager.
Session manager reads regular files ending in `.yaml` or `.yml` from
`${FLAME_HOME}/conf/applications/`, independent of the path passed to
`--config`. It sorts paths lexicographically and then processes YAML documents
in file order. Non-YAML files are ignored, and a missing applications directory
is equivalent to an empty manifest set.

Each YAML document uses the same strict manifest model as `flmctl`:

```yaml
metadata:
  name: flmrun
spec:
  shim: cri
  image: registry.example.com/xflops/flmrt:latest
  command: /usr/local/flame/bin/uv
  arguments:
    - run
    - --python
    - python${FLAME_PYTHON_VERSION}
    - python
    - -m
    - flamepy.runner.runpy
  environments:
    FLAME_HOME: /usr/local/flame
    FLAME_PYTHON_VERSION: "3.12"
  installer: python
  schema: {}
```

`metadata.name` is required and validated as an application name. `spec`
accepts the existing application attributes: `shim`, `image`, `description`,
`labels`, `command`, `arguments`, `environments`, `working_directory`,
`max_instances`, `delay_release`, `schema`, `url`, and `installer`. Omitted
fields use `ApplicationAttributes` defaults, including `shim: host`. Unknown
fields, malformed YAML, invalid field values, unreadable files, a non-directory
applications path, and duplicate application names across any files or YAML
documents are startup errors. Session manager validates the complete manifest
set before opening storage or serving either API, so these errors cannot produce
a partially applied manifest set.

After loading persisted storage and before starting providers, APIs, or the
scheduler, session manager reconciles manifests in deterministic order. A
manifest is authoritative for its declared name: a missing stored application
is registered, and an existing definition is updated when any configured
attribute differs. Stored applications whose names do not occur in the
manifest directory remain unchanged; removing a manifest does not unregister
the corresponding application. Runtime metadata such as application version
and creation time is not part of manifest equality.

There are no compiled default applications. The source tree provides standard
Host manifests for `flmexec`, `flmping`, and `flmrun` under
`config/applications/`. A control-plane `flmadm install` copies missing YAML/YML
files to `${PREFIX}/conf/applications/`. Existing destination files are always
preserved, including with `--force`, so operator customizations survive an
upgrade. A `--clean` installation removes the prior configuration and therefore
installs fresh shipped manifests. Worker-only and client-only installations do
not install control-plane application manifests.

### Internal API

Generated CRI protobuf types stay private. `cri-rs` exposes separate desired,
lifecycle, and observed models:

```text
WorkloadSpec     metadata + sandbox policy + ContainerSpec[]
WorkloadHandle   sandbox ID + container IDs
WorkloadStatus   sandbox status + ContainerStatus[]
```

The manager contract is:

```text
connect()
create(workload spec) -> workload handle
status(handle) -> workload status
list_workload(filter) -> workload handle[]
stop(handle)
delete(handle)
```

`stop` changes runtime state but retains resources. `delete` performs
best-effort stop followed by removal. Both are idempotent; CRI `NotFound`
during cleanup is success.

### Scope

In scope:

- Linux CRI v1 over a local Unix socket;
- one application container per sandbox;
- containerd's configured default runtime, including `runsc` when selected by
  the node configuration;
- CPU and memory limits, allowlisted mounts, logs, and non-privileged
  container security defaults;
- transactional lifecycle with label-scoped runtime operations; and
- reuse of the existing Instance gRPC protocol.

Out of scope for v1:

- Windows CRI;
- arbitrary host mounts, host network/PID/IPC, privileged containers, and
  bidirectional mount propagation;
- GPU workloads until Flame defines CDI/device allocation;
- arbitrary application-controlled host mounts; and
- adoption of a running workload after executor-manager process restart.

### Compatibility

Existing `shim: host` and `shim: wasm` behavior is unchanged. `cri` is an
additive `Shim` enum value propagated through the RPC, common model, storage,
SDK, CLI, scheduler, and executor manager in the same way as the existing
values. Applications registered as `host` are not silently moved into CRI.

## 3. Implementation Detail

### Architecture

```text
Session manager
      |
      | executor/session/task stream
      v
Executor manager
      |
      +-- Host shim --> child process
      |
      +-- CRI shim ---> CRI v1 --> containerd --> configured default runtime
                                                |
                                                +--> application Instance API
                                                     over per-executor UDS
```

`CriShim` owns an executor work directory, a CRI workload handle, and the
existing `GrpcShim`. Session enter, task invoke, and session leave continue to
use `GrpcShim`; only application launch, health, and cleanup differ.

Before any runtime path starts, session manager loads and reconciles the
installation-owned application manifests from
`${FLAME_HOME}/conf/applications`. This keeps Host and CRI application policy
outside the binary and lets the CRI E2E installation replace the standard Host
definitions with CRI definitions without a special registration API.

### Instance connectivity

CRI and containerd manage the application workload lifecycle; they do not
proxy the Flame Instance protocol. Executor manager connects directly to the
application's existing Instance gRPC service over a Unix domain socket in a
private, shared directory:

```text
Executor manager                 containerd/runsc sandbox
       |                                  |
       | RunPodSandbox/CreateContainer    |
       |--------------------------------->|
       |                                  |
       |   shared per-executor directory  |
       |<================================>|
       |                                  |
       |          Instance binds UDS      |
       |          instance.sock           |
       |                                  |
       | gRPC over the shared UDS         |
       |--------------------------------->|
       |  enter / invoke / leave          |
```

For executor `<executor-id>`, executor manager creates:

```text
/var/lib/flame/executors/<executor-id>/
```

It bind-mounts that directory read/write into the application container at the
identical absolute path and injects:

```text
FLAME_INSTANCE_ENDPOINT=/var/lib/flame/executors/<executor-id>/instance.sock
```

The identical path is a deployment requirement, not a configurable CRI option.
CRI mount sources are resolved in the containerd host namespace. When executor
manager itself runs in a container, its service definition must therefore use
a same-path bind mount:

```text
/var/lib/flame/executors:/var/lib/flame/executors
```

A remapped mount such as `/var/tmp/flame/var:/var/flame` is insufficient: a
path visible only inside the executor-manager container is not a valid source
path for containerd. The worker deployment must create the host directory with
permissions that allow only the executor-manager service and the intended
application container identity to access it.

The application starts its existing Rust or Python Instance server and binds
the injected socket path. Because the bind mount is shared, the socket appears
at the same path for executor manager. No host networking, host port, or CRI
streaming API is used for Instance communication.

`CriShim` waits for readiness by concurrently:

1. watching CRI container status and failing immediately if the application
   container exits;
2. waiting, with the fixed startup deadline, for `instance.sock` to exist;
3. verifying that the path is a Unix socket rather than a regular file or
   symlink; and
4. completing a real gRPC connection before reporting the shim ready.

Readiness must use timer-backed asynchronous polling or runtime events. The
current unbounded self-waking socket poll is not suitable for CRI startup.
Before launch, executor manager removes a stale socket only from the validated
directory it owns for that executor. It never follows an application-created
symlink while preparing or cleaning the directory.

For gVisor, the containerd-configured `runsc` runtime must serve the bind mount
in shared mode and set `host-uds=create`. The default `host-uds=none` prevents
the sandbox from creating the host-visible Instance socket. `host-uds=create`
allows binding a socket below a mount exposed to the sandbox; Flame must not
require or enable the broader `host-uds=open` or `host-uds=all` modes.

Flame does not mutate containerd or runsc configuration. Each
`create_service` performs a real bind/connect readiness check through the
configured default runtime. If shared UDS creation is unavailable, that bind
fails with an actionable prerequisite error and does not fall back to Host.

Session unbind invokes `on_session_leave` without stopping the application.
During executor release, executor manager closes the gRPC channel, stops and
removes the CRI workload, and only then removes the per-executor directory.
Constructor failure follows the same cleanup ordering for every resource that
was successfully created.

### Object-cache connectivity

`CriShim` injects `FLAME_CACHE_ENDPOINT` using the existing Flame context; it
does not rewrite cache addresses for the sandbox. That endpoint is used for
initial cache operations. An `ObjectRef` also carries the endpoint of the cache
replica that owns its object, and the Instance SDK normally connects directly
to that endpoint. The owning replica may be on another host.

Consequently, the platform network is required to make every advertised
`ObjectRef.endpoint` routable from every CRI sandbox. Alternatively, operators
may use the existing `grpcs-proxy://` cache mode: the SDK connects to the
configured proxy and uses the owning endpoint from the `ObjectRef` as its gRPC
authority. The proxy must itself be reachable from the sandbox and able to
route that authority to the owning cache replica.

Loopback cache endpoints such as `127.0.0.1` or `localhost` are invalid for the
CRI shim because they address the application sandbox, not the executor-manager
host. `CriShim` rejects such a configured cache endpoint before creating a
workload. Flame does not add host networking, endpoint NAT, DNS overrides, or
cache-replica discovery to compensate for an unreachable platform network.
The same rejection applies to the cluster endpoint injected as
`FLAME_ENDPOINT`.

### Ownership and identity

Every sandbox and container carries these stable labels:

- `io.xflops.flame.managed-by=executor-manager`;
- `io.xflops.flame.executor-id`;
- `io.xflops.flame.application`; and
- `io.xflops.flame.workload-uid`.

`WorkloadFilter` contains the persisted executor ID. `list_workload` translates
it to an exact managed-by plus executor-ID CRI label selector and validates the
returned labels again. Each create generates one CRI metadata UID and copies it
to `workload-uid`, allowing an ambiguous failed `RunPodSandbox` call to clean up
only that creation. Flame does not locally restart CRI workloads, so the CRI
sandbox and container metadata attempt is always zero and is not exposed in the
public model. The CRI endpoint is node-local, and site administrators must run
only one Flame environment against a host's CRI service; installation and node
IDs therefore do not participate in ownership. Names are human-readable only
and do not prove ownership. The sandbox name and hostname use
`<application-name>-<executor-id>` to aid runtime inspection. Tests must create
a foreign sandbox and prove it is never returned or removed.

### Model

`ContainerSpec` contains only desired values: name, image, command, arguments,
environment, working directory, mounts, resources, and security context.
`ContainerStatus` contains only values reported by CRI: ID, sandbox ID, name,
image and resolved image reference, state, timestamps, exit code, reason, and
message. Missing required response fields and invalid enum/timestamp values are
errors; the library never invents desired values from a list response.

`WorkloadHandle` is returned as soon as all requested containers are created
and started. It is the only input needed for normal cleanup. Status includes
both sandbox readiness and container health: a ready sandbox with an exited
application container is not a healthy Flame workload.

Container `log_path` is relative to the sandbox log directory. Each workload
uses a unique sandbox directory below the configured log root. Environment
entries are sorted before request construction to keep requests and tests
deterministic.

### Create transaction

The call sequence is:

1. Connect to `/run/containerd/containerd.sock`; verify CRI version and runtime
   readiness.
2. Query each image through the CRI image service and pull it only when absent.
3. Run the pod sandbox with an empty runtime handler so containerd uses its
   configured default.
4. Create each container in the sandbox.
5. Start each container.
6. Wait until the application container is running and its Instance socket is
   connectable, bounded by `startup_timeout`.

After step 3, each successful operation is recorded in a cleanup journal. Any
later failure unwinds in reverse order:

1. stop started containers;
2. remove created containers;
3. stop the sandbox; and
4. remove the sandbox.

The primary error is returned with cleanup failures attached as context. Each
operation is attempted once; the shim does not run a local retry loop.

### Executor lifecycle

On bind, executor manager constructs the selected shim and immediately calls
`create_service`, passing the shared ApplicationManager. CRI invokes the same
installation path as Host when the application has a package URL. It mounts
the returned release and Python runtime read-only, mounts the managed UV/pip
caches read/write, and rewrites the installer's environment to executor-local
container paths. It pulls a missing image, rejects GPU combinations, creates
a per-executor directory, and injects the cluster, cache, TLS, log, and Instance
socket environment required by the application. The per-executor directory is
mounted read/write. `FLAME_CA_FILE` is the SDK trust setting used for both
cluster and cache clients. When either endpoint uses a configured private CA,
CRI builds a PEM bundle from the independently configured `cluster.tls.ca_file`
and `cache.tls.ca_file`, copies it into the private directory, and exposes the
bundle as `FLAME_CA_FILE`. This avoids incorrectly using one service's CA as
the other service's sole trust root while retaining a single SDK trust-file
contract.

`create_service` owns rollback because a failed instance is never stored in
the executor state. The runtime instance is application-scoped and shares the
executor lifetime: session unbind calls only `on_session_leave`, while executor
release deletes the workload. A session-leave error is reported but does not
block unbind or destroy the retained application service. Async cleanup cannot
rely on `Drop`.

If the application container exits, status exposes its exit code/reason and
task invocation fails clearly instead of repeatedly reporting a socket error.

### Recovery

Recovery is part of the normal `ExecutorManager::run` state flow. When an
application instance fails, executor manager reports the failure to session
manager and does not retry or recreate that service locally. Session manager
must release the executor; the normal release path calls `destroy_instance`
once before a replacement executor is created.

A stream reconnect without process restart retains in-memory shims and does
not delete workloads. After an executor-manager process restart, registration
reports only the manager's in-memory executors. Session manager retains every
persisted executor missing from that report and sends it to the run loop in
`Releasing`. The unified shim factory receives no application context and
returns a cleanup-only CRI shim; Host and WASM require no recovery shim. CRI
performs one targeted deletion using the persisted executor ID and managed-by
ownership label. After the
normal unregister acknowledgement, session manager marks an interrupted task
failed and deletes the executor record. There is no broad startup sweep,
adoption, service replacement, or local retry loop in v1.

### Security

Access to the CRI socket is equivalent to node-root runtime control. Executor
manager must be node-local, the socket must not be exposed over TCP, and socket
permissions must be limited to its service identity.

Privilege defaults off at both sandbox and container levels. V1 does not allow
host namespaces, arbitrary devices, application-provided host paths, or mount
propagation. Image credentials come from node configuration and are never
stored in application YAML or logs. Environment and auth values are redacted
from logs. Production deployments should prefer digest-pinned images and
record the resolved image reference.

Writable host bind mounts are limited to the private executor directory and
the ApplicationManager-owned UV/pip caches; application YAML cannot add mount
sources. The executor directory is created with a service-owned mode, contains
no pre-existing sockets, and is removed after workload deletion. Package and
runtime mounts are read-only. gVisor is configured with
`host-uds=create`—not `open` or `all`—so the workload can bind its own Instance
socket without connecting to host services exposed elsewhere.

Sandbox network policy allows DNS, required internet egress, the configured
session-manager endpoint, and the object-cache port on the explicit Flame
worker set. It denies metadata, unrelated control-plane and host services, and
all other VPC destinations. Cache and session-manager access do not imply
general worker-to-worker connectivity.

### Observability

Workload lifecycle logs include node, executor, application, sandbox, and
container IDs; session RPC logs additionally include the current session ID.
Logs never include environment values or credentials. Metrics cover image
pull, create, startup, stop, and remove latency/failures, rollback failures,
owned orphan count, and running/exited workload count.

### Test strategy

Active unit tests cover model conversion, missing response fields, state and
timestamp mapping, deterministic environment order, labels and filters, log
paths, security, resources, and mounts.

A fake CRI service over a temporary Unix socket verifies call order and injects
failures/timeouts at pull, sandbox run, container create/start, readiness,
stop, and remove. Every post-sandbox failure must prove reverse rollback.

Executor tests use fake CRI and Instance services to cover environment/mount
translation (including writable UV/pip caches), bind/reuse/unbind, early exit,
startup timeout, cleanup after bind failure, reconnect, controller-driven
restart, rejection of loopback cluster and cache endpoints, recursive
`FLAME_ENDPOINT` injection, and cluster/cache CA bundle staging.

The `CRI Shim E2E` Linux CI job provisions a real containerd CRI v1 service and
bridge CNI, configures `runsc` as containerd's default CRI runtime with
`host-uds=create`, and publishes `flmrt` to a local registry. `flmrt` is a
production runtime image assembled through the normal worker installation: its
final stage contains the installed Flame services, Python SDK, runtimes, and
tools under `/usr/local/flame`, but no E2E source tree and no executor-manager
binary. Test code is therefore not baked into the runtime image.

The job first runs the `WorkloadManager` boundary cases directly against the
real CRI service. A dedicated workload verifies gVisor's in-sandbox boot marker,
proving that the empty runtime handler selected the configured default instead
of silently falling back to `runc`. The suite covers the operations used by
`CriShim` (`create`, `list_workload`, `status`, and `delete`) with ownership
metadata, image, command and arguments, environment, working directory,
read-only and writable mounts, resource limits, security context, and log
directory. It reconstructs the handle by executor labels before deletion,
verifies that an unrelated executor filter cannot discover the workload, and
covers running and exited status, rebased installation paths, reconnect/list
recovery, and idempotent deletion. Cases run serially and always attempt
targeted cleanup after a successful create.

The same job then starts the actual session manager, object cache, and executor
manager with CRI manifests for `flmping`, `flmrun`, and `flmexec`. It runs the
shared E2E Runner, flmexec, and Sandbox suites; every application service
executes inside gVisor through the production CRI shim. Runner packages and
uploads its working directory through the configured package/cache backend.
flmexec and Sandbox exercise the configured `flmexec` service, including a
flmexec script that recursively creates Runner applications and installs a
native NumPy dependency. The generic Agent/Session service suite is omitted
because Runner already covers packaged dynamic services and Sandbox covers the
agent execution path through flmexec. This validates image-only services,
Runner package delivery, recursive `FLAME_ENDPOINT` access, object-cache
traffic, and real Instance UDS lifecycle under gVisor.

The benchmark workflow exposes `Host Shim Benchmark` and `CRI Shim Benchmark`
as peer jobs. The CRI job uses the BareMetal containerd and gVisor topology and
pre-pulls both `flmrt` and containerd's configured sandbox image before timing.
Both jobs execute the shared `(session count, tasks per session)` matrix:
`1 × 1`, `1 × 1000`, `10 × 1`, and `10 × 1000`. This reports cold single-task
round trip followed by warm single-session throughput, concurrent round trip,
and concurrent throughput through one code path. Only the endpoint/runtime
environment and cluster setup differ, so workloads and reported metrics remain
directly comparable.

The `BareMetal E2E` workflow follows the same runtime split: `Host Shim E2E`
and `CRI Shim E2E` are peer jobs. They retain separate provisioning because
the CRI variant must configure containerd, gVisor, CNI, and the runtime image,
then run the same application-level Runner, flmexec, and sandbox E2E cases
against that environment. `cri-rs` unit tests remain part of normal Code
Verify coverage; there is no separate CRI-only application test suite.

## 4. Use Cases

### Configure applications without rebuilding Flame

An operator edits `${PREFIX}/conf/applications/flmrun.yaml` to select a CRI
image and restarts session manager. Startup validates all application YAML/YML
files before opening the service, then updates the persisted `flmrun`
definition to match the manifest. Other applications previously registered
through the API remain stored because they have no matching manifest. A later
`flmadm install --force` preserves the customized file.

### Run with gVisor

An operator configures containerd's default CRI runtime as `runsc`, then sets
the Flame application and executor manager to `shim: cri`. On bind, executor
manager creates the sandbox and container through containerd, connects to the
Instance socket, and reuses the container across sessions for the lifetime of
the application executor. Session unbind leaves the workload running; executor
release removes it.

### Roll back a failed start

Image pull, sandbox run, and container creation succeed, but start fails. The
manager removes the created container, stops the sandbox, and removes it. The
original start error is returned and no owned resources remain.

### Recover after process restart

Executor manager restarts and enters its normal run loop with no recovered
in-memory executors. Session manager sends each persisted missing executor as
`Releasing`. Executor manager delegates its exact ID to the configured shim,
which removes only the matching owned workload and acknowledges unregister.
Session manager then fails any interrupted task and deletes the executor
record. The service is not retried or replaced in v1.

## 5. References

- [Issue #525](https://github.com/xflops/flame/issues/525)
- [`RFE368` shim configuration](../RFE368-shim-config/FS.md)
- [`RFE379` shim selection](../RFE379-shim-selection/HLD.md)
- [`RFE384` recovery](../RFE384-flame-recovery/FS.md)
- [Kubernetes CRI v1 API](../../../cri/protos/cri.proto)
- [containerd runtime v2](https://github.com/containerd/containerd/blob/main/docs/runtime-v2.md)

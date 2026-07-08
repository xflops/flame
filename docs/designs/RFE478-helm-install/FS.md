---
Issue: #478
Author: Flame Team
Date: 2026-05-29
---

# RFE478: Helm Install for Flame

GitHub issue: https://github.com/xflops/flame/issues/478

## Summary

Add a first-party Helm chart for installing a static Flame cluster on
Kubernetes. The chart packages the same core components that are currently
deployed by `flmadm` and `docker compose`:

- `flame-session-manager`
- `flame-object-cache`
- `flame-executor-manager`

The first version is a static deployment: Kubernetes schedules the Flame
control plane, object cache, and a configured number of executor-manager pods.
It does not implement the future Kubernetes provider that creates workload pods
or autoscales executors per application demand.

## 1. Motivation

**Background:**

Flame currently has two supported cluster install paths:

- `flmadm`, which installs Flame binaries, configuration, data directories, and
  systemd services on bare metal or VMs.
- `docker compose`, which starts the session manager, object cache, executor
  managers, and a console container for local development.

Many users run production and shared development infrastructure on Kubernetes.
For those users, `flmadm` is not a natural packaging boundary, and
`docker compose` is not an operational deployment mechanism. They need a chart
that can be installed, upgraded, configured, and rolled back with standard Helm
and Kubernetes workflows.

**Target:**

Provide a Helm chart under `charts/flame` that:

1. Deploys a usable Flame cluster with one `helm install`.
2. Maps existing Flame component configuration into Kubernetes ConfigMaps,
   Secrets, Services, Deployments, StatefulSets, and PVCs.
3. Supports static worker capacity through a configurable executor-manager
   replica count.
4. Supports persistent storage for the session-manager state and object-cache
   data.
5. Supports optional TLS using existing Kubernetes Secrets.
6. Keeps the initial chart independent from the future Kubernetes provider and
   autoscaling work.

Success criteria:

- A user can run:

  ```bash
  helm install flame ./charts/flame --namespace flame --create-namespace
  ```

- The install creates a session manager reachable inside the cluster at:

  ```text
  http://flame-session-manager:8080
  ```

- The install creates an object cache reachable inside the cluster at:

  ```text
  grpc://flame-object-cache:9090
  ```

- The configured executor-manager replicas register as Flame nodes and execute
  sessions without manual pod creation.
- `helm upgrade` applies configuration changes through normal Kubernetes
  rollouts.

## 2. Function Specification

### Configuration

The chart is configured through `values.yaml`. Defaults prioritize a working
single-namespace development install while allowing production overrides.

```yaml
global:
  imageRegistry: xflops
  imageTag: latest
  imagePullPolicy: IfNotPresent

nameOverride: ""
fullnameOverride: ""

cluster:
  name: flame
  resreq: "cpu=1,mem=2g"
  policies:
    - priority
    - drf
  scheduleInterval: 100
  storage: "sqlite:///var/lib/flame/session/flame.db"
  executors:
    shim: host
  limits:
    maxSessions: 1000
    maxExecutors: 128
  recovery:
    session:
      retryLimits: 5

sessionManager:
  enabled: true
  image:
    repository: flame-session-manager
    tag: ""
  replicas: 1
  service:
    type: ClusterIP
    frontendPort: 8080
    backendPort: 8081
  persistence:
    enabled: true
    storageClassName: ""
    size: 10Gi
  resources: {}

objectCache:
  enabled: true
  image:
    repository: flame-object-cache
    tag: ""
  replicas: 1
  service:
    type: ClusterIP
    port: 9090
  networkInterface: eth0
  storage: "/var/lib/flame/cache"
  persistence:
    enabled: true
    storageClassName: ""
    size: 100Gi
  eviction:
    policy: lru
    maxMemory: 1G
    maxObjects: null
  resources: {}

executorManager:
  enabled: true
  image:
    repository: flame-executor-manager
    tag: ""
  replicas: 3
  runtimeStorage:
    medium: ""
    sizeLimit: ""
  resources: {}
  nodeSelector: {}
  tolerations: []
  affinity: {}
  extraEnv: []
  extraVolumes: []
  extraVolumeMounts: []

tls:
  enabled: false
  mountPath: /etc/flame/certs
  cluster:
    secretName: ""
    certKey: tls.crt
    privateKeyKey: tls.key
    caKey: ca.crt
  cache:
    secretName: ""
    certKey: tls.crt
    privateKeyKey: tls.key
    caKey: ca.crt

serviceAccount:
  create: true
  name: ""

podSecurityContext: {}
securityContext: {}

clientConfig:
  enabled: true
```

Image resolution:

- Component-specific `image.tag` overrides `global.imageTag`.
- Component image names are rendered as:

  ```text
  <global.imageRegistry>/<component.image.repository>:<tag>
  ```

Rendered cluster configuration:

```yaml
cluster:
  name: flame
  endpoint: "http://flame-session-manager:8080"
  resreq: "cpu=1,mem=2g"
  policies:
    - priority
    - drf
  storage: "sqlite:///var/lib/flame/session/flame.db"
  schedule_interval: 100
  executors:
    shim: host
  limits:
    max_sessions: 1000
    max_executors: 128
  recovery:
    session:
      retry_limits: 5
cache:
  endpoint: "grpc://flame-object-cache:9090"
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"
  eviction:
    policy: "lru"
    max_memory: "1G"
```

When TLS is enabled, the chart changes endpoint schemes and renders explicit
TLS sections:

```yaml
cluster:
  endpoint: "https://flame-session-manager:8080"
  tls:
    cert_file: "/etc/flame/certs/cluster/tls.crt"
    key_file: "/etc/flame/certs/cluster/tls.key"
    ca_file: "/etc/flame/certs/cluster/ca.crt"
cache:
  endpoint: "grpcs://flame-object-cache:9090"
  tls:
    cert_file: "/etc/flame/certs/cache/tls.crt"
    key_file: "/etc/flame/certs/cache/tls.key"
    ca_file: "/etc/flame/certs/cache/ca.crt"
```

TLS validation rules:

- If `tls.enabled=true`, both `tls.cluster.secretName` and
  `tls.cache.secretName` are required.
- The chart must not synthesize production certificates.
- A development-only self-signed certificate helper is out of scope for this
  RFE.
- The chart-level TLS settings are convenience values only; they are rendered
  into the existing `cluster.tls` and `cache.tls` config sections rather than
  changing Flame's runtime config schema.

### Kubernetes Resources

The chart creates the following resources by default:

| Resource | Kind | Purpose |
|----------|------|---------|
| `flame-config` | ConfigMap | Renders `flame-cluster.yaml` and optional in-cluster `flame.yaml`. |
| `flame-session-manager` | Deployment | Runs the Flame control plane. |
| `flame-session-manager` | Service | Exposes frontend port `8080` and backend port `8081`. |
| `flame-session-manager-data` | PVC | Stores SQLite database and event directory when persistence is enabled. |
| `flame-object-cache` | StatefulSet | Runs one or more standalone object-cache servers. |
| `flame-object-cache` | Service | Exposes Arrow Flight port `9090`. |
| `flame-object-cache-data` | VolumeClaimTemplate/PVC | Stores per-replica object-cache data when persistence is enabled. |
| `flame-executor-manager` | Deployment | Runs static worker capacity. |
| `flame` | ServiceAccount | Shared service account for pods. |

The object cache is a `StatefulSet` because each replica owns cache data and
needs stable pod lifecycle semantics. The session manager remains a
single-replica `Deployment` for this RFE because Flame does not yet define
active-active session-manager semantics.

The executor manager is a `Deployment` with configurable replicas. Each pod
registers as a Flame node using the pod hostname detected by the runtime. This
matches the static scope: Kubernetes provides pod placement, while Flame's
scheduler assigns sessions to the already-running executor-manager pods.

### API

No Flame API changes are required.

The chart uses existing component entrypoints and the existing
`flame-cluster.yaml` configuration schema. The session manager continues to
serve:

- Frontend gRPC API on `cluster.endpoint` port, default `8080`.
- Backend gRPC API on `cluster.endpoint + 1`, default `8081`.

The object cache continues to serve Arrow Flight on `cache.endpoint`, default
port `9090`.

### CLI

Install:

```bash
helm install flame ./charts/flame \
  --namespace flame \
  --create-namespace
```

Upgrade:

```bash
helm upgrade flame ./charts/flame \
  --namespace flame \
  --values values.yaml
```

Uninstall:

```bash
helm uninstall flame --namespace flame
```

Local access through port forwarding:

```bash
kubectl -n flame port-forward svc/flame-session-manager 8080:8080
kubectl -n flame port-forward svc/flame-object-cache 9090:9090
```

Client environment for port-forwarded access:

```bash
export FLAME_ENDPOINT=http://127.0.0.1:8080
export FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090
```

Port forwarding is intended for local inspection, `flmctl list`, and simple
health checks. Application deployment commands that write package URLs for
executor-manager pods, such as `flmctl deploy` and Runner package upload, must
use a cache endpoint that is reachable by both the client and the executor
pods. For the default ClusterIP install, run those clients inside the cluster or
expose the object-cache Service through an operator-managed network path.

TLS install with existing Secrets:

```bash
helm install flame ./charts/flame \
  --namespace flame \
  --create-namespace \
  --set tls.enabled=true \
  --set tls.cluster.secretName=flame-session-manager-tls \
  --set tls.cache.secretName=flame-object-cache-tls
```

### Other Interfaces

If `clientConfig.enabled=true`, the chart renders a client-facing `flame.yaml`
ConfigMap for in-cluster clients:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://flame-session-manager:8080"
    cache:
      endpoint: "grpc://flame-object-cache:9090"
    package:
      excludes:
        - "*.log"
        - "*.pkl"
        - "*.tmp"
```

`NOTES.txt` prints both in-cluster endpoints and port-forward instructions.

### Scope

**In Scope:**

- Helm chart scaffolding under `charts/flame`.
- Static Kubernetes deployment for session manager, object cache, and executor
  managers.
- ConfigMap rendering for `flame-cluster.yaml`.
- Optional in-cluster client config rendering for `flame.yaml`.
- ClusterIP Services for internal component communication.
- PVC-backed persistence for session manager and object cache.
- Optional TLS Secret mounts and endpoint scheme rendering.
- Helm tests that verify the core Services are reachable.
- Documentation for install, upgrade, uninstall, TLS, and troubleshooting.

**Out of Scope:**

- Kubernetes provider implementation in `session_manager/src/provider/k8s.rs`.
- Application pod creation, per-application sidecars, or sidecar shim support.
- Autoscaling based on sessions, tasks, queues, or custom metrics.
- Horizontal high availability for the session manager.
- Ingress, Gateway API, or cloud load balancer configuration beyond standard
  Service values.
- Certificate generation, cert-manager integration, or mTLS.
- Publishing the chart to an OCI chart registry.
- Building or publishing component images.

**Limitations:**

- Session manager is single-replica.
- Object-cache replicas are static and change only when `objectCache.replicas`
  changes. With persistence enabled, each StatefulSet replica gets its own PVC.
- Executor capacity is static and changes only when
  `executorManager.replicas` changes.
- Executor-manager pods report runtime-detected CPU, memory, and GPU capacity.
  Kubernetes resource requests and limits still control pod scheduling, but
  this RFE does not add a Flame-specific resource override mechanism.
- No Kubernetes RBAC permissions are required for the static chart because
  Flame does not watch or mutate Kubernetes objects in this version.
- The chart does not add a stable object-cache advertised endpoint. The current
  object-cache server derives metadata endpoints from its runtime network
  interface, which is suitable for in-cluster pod networking but not a complete
  external-access story.

### Feature Interaction

**Related Features:**

- `flmadm` installation profiles: define the same component split that the
  chart deploys as containers.
- Docker Compose deployment: provides current container names, mounted
  configuration, ports, and cache persistence behavior.
- RFE234 TLS: defines `cluster.tls`, `cache.tls`, and endpoint scheme
  conventions.
- RFE318 Object Cache: defines the standalone object-cache service and storage
  behavior.
- RFE420 Application Installer: defines executor-manager package installation
  behavior and Python/`uv` runtime expectations.
- RFE429 Object Cache Upload/Download: defines object-cache package URLs using
  `grpc://` and `grpcs://`.
- RFE384 Recovery: defines restart and reconnection behavior used when pods
  restart or roll during upgrades.
- RFE323 Runner v2: defines session-level `autoscale` behavior. That behavior
  allocates Flame executors from already-registered worker capacity in this
  static chart; Kubernetes pod autoscaling remains future provider work.
- RFE458 `flmctl deploy`: depends on the object-cache endpoint being reachable
  from clients and executor-manager pods.

**Updates Required:**

- Add chart files under `charts/flame`.
- Add chart documentation under `charts/flame/README.md` or
  `docs/tutorials/helm-install.md`.
- Add CI chart validation with `helm lint` and `helm template`.
- Optionally add a Kind-based smoke test that installs the chart and verifies
  the Services and pods become ready.

**Integration Points:**

- Pods mount `flame-cluster.yaml` from a ConfigMap at
  `/usr/local/flame/conf/flame-cluster.yaml`.
- Pods set `FLAME_HOME=/usr/local/flame` and `RUST_LOG=info` by default.
- Component commands run with:

  ```text
  /usr/local/flame/bin/flame-session-manager --config /usr/local/flame/conf/flame-cluster.yaml
  /usr/local/flame/bin/flame-object-cache --config /usr/local/flame/conf/flame-cluster.yaml
  /usr/local/flame/bin/flame-executor-manager --config /usr/local/flame/conf/flame-cluster.yaml
  ```

- The session-manager and object-cache pods use `/usr/local/flame` as their
  working directory. This preserves relative paths such as SQLite migrations
  under `migrations/sqlite`.
- The executor-manager pod uses `/usr/local/flame/work` as its working
  directory, matching the existing worker service behavior.
- Executor-manager pods connect to the session-manager backend by deriving port
  `8081` from the rendered `cluster.endpoint`.
- Clients and deployed applications use the rendered object-cache Service on
  port `9090`.

**Compatibility:**

- Existing `flmadm` and Docker Compose deployment paths continue unchanged.
- Existing Flame client configuration remains valid; users can point clients at
  the Helm-installed endpoints.
- Existing TLS configuration semantics remain unchanged.

**Breaking Changes:**

- None.

## 3. Implementation Detail

### Architecture

```text
                 Kubernetes namespace: flame

  Client / flmctl / SDK
          |
          | http(s)://flame-session-manager:8080
          v
  +-----------------------------+
  | Service: flame-session-     |
  | manager                     |
  | ports: 8080, 8081           |
  +-------------+---------------+
                |
                v
  +-----------------------------+       sqlite/events PVC
  | Deployment: flame-session-  |------ /var/lib/flame/session
  | manager replicas: 1         |
  |                             |------ /usr/local/flame/events
  +-----------------------------+
                ^
                |
                | backend gRPC, port 8081
                |
  +-------------+---------------+
  | Deployment: flame-executor- |
  | manager replicas: N         |
  +-------------+---------------+
                |
                | grpc(s)://flame-object-cache:9090
                v
  +-----------------------------+       cache PVC
  | StatefulSet: flame-object-  |------ /var/lib/flame/cache
  | cache replicas: M           |
  +-----------------------------+
```

### Components

**Chart Metadata (`charts/flame/Chart.yaml`):**

- `apiVersion: v2`
- `type: application`
- `appVersion`: Flame release version
- `version`: chart version

**Values (`charts/flame/values.yaml`):**

- Provides the documented defaults.
- Keeps component-specific overrides separate from shared global defaults.
- Uses lower camelCase for chart values and renders snake_case fields expected
  by Flame config.

**Values Schema (`charts/flame/values.schema.json`):**

- Validates common mistakes before install:
  - `sessionManager.replicas` must be `1`.
  - `objectCache.replicas` must be `>= 1`.
  - `executorManager.replicas` must be `>= 0`.
  - Service ports must be valid TCP ports.
  - TLS Secrets are required when `tls.enabled=true`.

**ConfigMap Templates:**

- Render `flame-cluster.yaml` for server components.
- Render optional `flame.yaml` for in-cluster clients.
- Include checksums as pod template annotations so ConfigMap changes trigger
  rollouts.

**Session Manager Templates:**

- Deployment with one replica.
- Mounts the shared config.
- Mounts persistent data at `/var/lib/flame/session` for SQLite state when
  enabled.
- Mounts the same persistent volume at `/usr/local/flame/events` for the event
  directory written relative to the service working directory.
- Sets `workingDir: /usr/local/flame`.
- Exposes named ports:
  - `frontend`: `8080`
  - `backend`: `8081`
- Uses TCP startup/readiness probes on the frontend port until Flame exposes a
  dedicated health endpoint.

**Object Cache Templates:**

- StatefulSet with one replica.
- Mounts the shared config.
- Mounts persistent data at `/var/lib/flame/cache` when enabled.
- Sets `workingDir: /usr/local/flame`.
- Exposes named port `flight` on `9090`.
- Uses TCP startup/readiness probes on the Flight port.

**Executor Manager Templates:**

- Deployment with configurable replicas.
- Mounts the shared config.
- Supports `nodeSelector`, `affinity`, and `tolerations` for static placement.
- Supports `extraEnv`, `extraVolumes`, and `extraVolumeMounts` for site-specific
  GPU, model-cache, or package-cache setup.
- Does not expose a Service because executor managers initiate outbound
  connections to the session manager.
- Sets `workingDir: /usr/local/flame/work`.
- Mounts writable `emptyDir` volumes for runtime paths that host-shim and app
  installation code write to:

  | Path | Purpose |
  |------|---------|
  | `/usr/local/flame/work` | Per-executor working directories. |
  | `/usr/local/flame/data/apps` | Installed application releases. |
  | `/usr/local/flame/data/cache` | `uv` and package caches. |
  | `/usr/local/flame/logs` | Install and service logs when files are used. |
  | `/tmp/flame` | Executor-manager shim scratch directory. |
  | `/var/flame` | Host-shim Unix socket directory. |

  These volumes are pod-local in the static chart. Durable app-install caches
  can be added later with an optional PVC, but they are not required for a
  functional first version.

**TLS Secret Mounts:**

- Mount session-manager TLS material under
  `/etc/flame/certs/cluster`.
- Mount object-cache TLS material under `/etc/flame/certs/cache`.
- Mount CA files in executor-manager pods because they act as clients for both
  the session manager and object cache.

**Helm Test Pod:**

- Runs a small connectivity check.
- Verifies the session-manager Service port `8080` is reachable.
- Verifies the object-cache Service port `9090` is reachable.
- If a client image with `flmctl` is configured for tests, optionally runs:

  ```bash
  flmctl --config /etc/flame/flame.yaml list -a
  ```

### Data Structures

Rendered `flame-cluster.yaml` is the main data contract. The chart must map
values to Flame's existing config structure without requiring runtime changes:

| Chart value | Rendered config |
|-------------|-----------------|
| `cluster.name` | `cluster.name` |
| `cluster.resreq` | `cluster.resreq` |
| `cluster.policies` | `cluster.policies` |
| `cluster.scheduleInterval` | `cluster.schedule_interval` |
| `cluster.storage` | `cluster.storage` |
| `cluster.executors.shim` | `cluster.executors.shim` |
| `cluster.limits.maxSessions` | `cluster.limits.max_sessions` |
| `cluster.limits.maxExecutors` | `cluster.limits.max_executors` |
| `cluster.recovery.session.retryLimits` | `cluster.recovery.session.retry_limits` |
| `objectCache.replicas` | StatefulSet `spec.replicas` |
| `objectCache.networkInterface` | `cache.network_interface` |
| `objectCache.storage` | `cache.storage` |
| `objectCache.eviction.maxMemory` | `cache.eviction.max_memory` |
| `objectCache.eviction.maxObjects` | `cache.eviction.max_objects` |

The chart should centralize name and endpoint construction in helper templates:

```text
flame.fullname
flame.sessionManager.name
flame.objectCache.name
flame.clusterEndpoint
flame.cacheEndpoint
flame.image
```

### Algorithms

**Endpoint Rendering:**

1. Resolve the session-manager Service DNS name.
2. Use `http://` when `tls.enabled=false`; use `https://` when
   `tls.enabled=true`.
3. Render `cluster.endpoint` with the frontend port.
4. Resolve the object-cache Service DNS name.
5. Use `grpc://` when `tls.enabled=false`; use `grpcs://` when
   `tls.enabled=true`.
6. Render `cache.endpoint` with the object-cache port.

**Config Rollout:**

1. Render `flame-cluster.yaml` in a ConfigMap.
2. Compute a checksum of the rendered ConfigMap content.
3. Put the checksum in each pod template annotation.
4. Let Kubernetes perform a rolling restart when configuration changes.

**Static Executor Scaling:**

1. User changes `executorManager.replicas`.
2. Kubernetes scales the executor-manager Deployment.
3. New pods start `flame-executor-manager`.
4. Each executor manager registers as a Flame node through the backend API.
5. Removed pods stop heartbeating; existing recovery logic releases stale
   executors and retries tasks according to the configured recovery policy.

**Static Object-Cache Scaling:**

1. User changes `objectCache.replicas`.
2. Kubernetes scales the object-cache StatefulSet.
3. Each replica starts with the same rendered cache endpoint config and serves
   through the object-cache Service.
4. When persistence is enabled, each replica receives its own PVC from the
   StatefulSet volume claim template.

**Install Validation:**

1. `values.schema.json` catches structural value errors.
2. Template `required` and `fail` calls catch cross-field errors, especially
   TLS Secret requirements.
3. `helm lint` validates chart syntax.
4. `helm template` validates renderability without cluster access.
5. Optional Kind smoke tests validate Kubernetes resource admission and basic
   service readiness.

### System Considerations

**Performance:**

- The chart introduces no new runtime data path.
- ClusterIP Services add standard Kubernetes service routing between clients,
  executor managers, session manager, and object cache.
- Users should set CPU and memory requests for executor-manager pods so
  Kubernetes places static capacity predictably.

**Scalability:**

- Executor capacity scales horizontally by changing
  `executorManager.replicas`.
- Object-cache serving capacity scales horizontally by changing
  `objectCache.replicas`; cache storage remains per replica by default.
- Session-manager scale-up is vertical only in this RFE.
- Future Kubernetes-provider work can add per-application pod scaling without
  changing this chart's core control-plane resources.

**Reliability:**

- Kubernetes restarts failed pods.
- Session-manager persistence should be enabled for non-ephemeral clusters.
- Object-cache persistence should be enabled when cached objects must survive
  pod restarts.
- Rolling config changes restart pods through checksum annotations.
- Executor-manager reconnection and Flame recovery handle pod restarts at the
  Flame control-plane layer.

**Resource Usage:**

- Session manager needs persistent storage for SQLite and event files when
  `cluster.storage` is not `none`; the chart mounts both the SQLite directory
  and the relative event directory.
- Object cache storage size should be chosen based on expected package and
  object-cache volume.
- Object-cache `eviction.maxMemory` should be less than the pod memory limit.
- Executor-manager pod-local `emptyDir` storage must be large enough for app
  packages, installed dependencies, working directories, and Unix sockets.
- GPU workloads require Kubernetes GPU scheduling configuration outside this
  chart, plus executor-manager pod placement on GPU-capable nodes.

**Security:**

- Plaintext is the default for local development.
- Production installations should enable TLS and provide existing Secrets.
- The static chart does not require Kubernetes API write privileges.
- Pods should run with restrictive `securityContext` values where the runtime
  environment permits it.
- External exposure should be done deliberately through Service overrides,
  Ingress, or Gateway resources outside this first chart scope.

**Observability:**

- Logs are emitted to stdout/stderr and collected by Kubernetes logging.
- `RUST_LOG` defaults to `info` and is configurable with `extraEnv`.
- TCP probes validate process-level readiness until HTTP/gRPC health endpoints
  are available.
- Future chart versions can add ServiceMonitor resources after Flame exposes
  metrics or a stable profiling endpoint policy.

**Operational:**

- `helm upgrade` is the supported upgrade path.
- PVC retention follows Kubernetes and Helm defaults; uninstalling the chart
  should not silently delete persistent data unless users delete PVCs.
- `NOTES.txt` must include commands for checking pods, port-forwarding, and
  configuring local clients.
- The chart should include a documented "ephemeral development" mode with
  persistence disabled and a "persistent static cluster" mode with PVCs enabled.

### Dependencies

External:

- Kubernetes with `apps/v1` Deployments and StatefulSets.
- Helm v3.
- A StorageClass when persistence is enabled.
- Container images for:
  - `xflops/flame-session-manager`
  - `xflops/flame-object-cache`
  - `xflops/flame-executor-manager`

Internal:

- `common/src/ctx.rs` for `flame-cluster.yaml` schema.
- `session_manager` frontend and backend server port behavior.
- `object_cache` Arrow Flight endpoint and storage behavior.
- `executor_manager` WatchNode registration and recovery behavior.
- Existing Dockerfiles and Compose deployment for image and volume conventions.

### Verification Plan

Static validation:

- `helm lint charts/flame`
- `helm template flame charts/flame`
- `helm template flame charts/flame --set tls.enabled=true --set tls.cluster.secretName=cluster-tls --set tls.cache.secretName=cache-tls`
  to verify TLS rendering and validation.
- `helm template flame charts/flame --set sessionManager.persistence.enabled=false --set objectCache.persistence.enabled=false`
  to verify ephemeral development mode.
- `helm template flame charts/flame --set executorManager.runtimeStorage.medium=Memory`
  to verify runtime `emptyDir` rendering.
- Validate rendered manifests with the repository's chosen Kubernetes schema
  validator if one is added to CI.

Chart unit checks:

- Default values render one session-manager Deployment, one object-cache
  StatefulSet, one executor-manager Deployment, and the expected Services.
- Rendered `flame-cluster.yaml` uses service DNS names and expected ports.
- TLS-enabled values render `https://` and `grpcs://` endpoints plus Secret
  mounts.
- ConfigMap checksum annotations change when `flame-cluster.yaml` changes.
- Session-manager and object-cache pods render `workingDir: /usr/local/flame`.
- Executor-manager pods render `workingDir: /usr/local/flame/work` and writable
  runtime volumes.
- `values.schema.json` rejects unsupported session-manager or object-cache
  replica counts.

Kind smoke test:

1. Create a Kind cluster with the existing `ci/kind.yaml`.
2. Load or pull the Flame component and console images.
3. Install the chart with default values.
4. Wait for session-manager, object-cache, and executor-manager pods.
5. Run the Helm test pod to verify service reachability.
6. Run `flmctl list -a` from an in-cluster client container using the rendered
   `flame.yaml`.
7. Run `flmping` and the Python Pi Runner example from the in-cluster
   `flame-console` image.

Manual verification:

- Scale `executorManager.replicas` up and down and confirm Flame node count
  follows the static pod count.
- Scale `objectCache.replicas` up and down and confirm the object-cache
  StatefulSet reaches the requested ready replica count.
- Restart the session-manager pod with persistence enabled and confirm
  applications and sessions recover according to existing recovery behavior.
- Restart the object-cache pod with persistence enabled and confirm cached
  objects survive.
- Install with TLS Secrets and confirm plaintext clients fail while TLS clients
  succeed.

## 4. Use Cases

### Example 1: Local Kind Cluster

Description:

Run Flame on a local Kind cluster for development and SDK testing.

Workflow:

1. Build or load Flame images into Kind.
2. Install the chart with default values.
3. Wait for pods to become ready.
4. Port-forward the session manager and object cache.
5. Run `flmctl list -a` from the local machine.

Expected outcome:

- One session-manager pod is running.
- One object-cache pod is running.
- Three executor-manager pods are running by default.
- Local clients can inspect cluster state through port-forwarded endpoints.

### Example 2: Static Shared Development Cluster

Description:

Run a persistent namespace-scoped Flame cluster with fixed worker capacity.

Workflow:

1. Create a namespace, StorageClass, and optional image pull secret.
2. Set persistent volume sizes for session manager and object cache.
3. Set `executorManager.replicas` to the desired static capacity.
4. Install or upgrade the chart.
5. Give users the rendered in-cluster `flame.yaml` or external endpoints.

Expected outcome:

- Flame state survives pod restarts.
- Object-cache data survives object-cache pod restarts.
- Executor capacity remains fixed until an operator changes the replica count.

### Example 3: TLS-Enabled Cluster

Description:

Install Flame with encrypted traffic between clients, executor managers,
session manager, and object cache.

Workflow:

1. Create TLS Secrets for the session manager and object cache.
2. Install the chart with `tls.enabled=true`.
3. Verify rendered endpoints use `https://` and `grpcs://`.
4. Configure clients with the CA certificate.

Expected outcome:

- Session-manager frontend and backend use server-side TLS.
- Object cache uses TLS for Arrow Flight.
- Executor managers mount CA files and connect to TLS endpoints.

### Example 4: Scale Static Workers

Description:

Increase static worker capacity for a busy development namespace.

Workflow:

1. Update `executorManager.replicas` from `3` to `10`.
2. Run `helm upgrade`.
3. Wait for new executor-manager pods to run.
4. Confirm Flame node count increases.

Expected outcome:

- Kubernetes creates seven additional executor-manager pods.
- The new executor managers register as Flame nodes.
- New sessions can be scheduled onto the added static capacity.

## 5. References

**Related Documents:**

- [RFE234 TLS](../RFE234-tls/FS.md)
- [RFE318 Object Cache](../RFE318-cache/FS.md)
- [RFE420 Application Installer](../RFE420-app-installer/FS.md)
- [RFE429 Object Cache Upload/Download](../RFE429-cache-upload-download/FS.md)
- [RFE333 flmadm](../RFE333-flmadm/FS.md)
- [RFE384 Flame Recovery](../RFE384-flame-recovery/FS.md)
- [RFE323 Runner v2](../RFE323-runner-v2/FS.md)
- [RFE458 flmctl deploy](../RFE458-flmctl-deploy/FS.md)
- [README](../../../README.md)
- [Local Development Tutorial](../../tutorials/local-development.md)

**External References:**

- [GitHub issue #478](https://github.com/xflops/flame/issues/478)

**Implementation References:**

- `compose.yaml`
- `ci/flame-cluster.yaml`
- `ci/flame.yaml`
- `ci/kind.yaml`
- `docker/Dockerfile.fsm`
- `docker/Dockerfile.foc`
- `docker/Dockerfile.fem`
- `flmadm/src/types.rs`
- `flmadm/src/managers/config.rs`
- `flmadm/src/managers/systemd.rs`
- `common/src/ctx.rs`
- `session_manager/src/apiserver/mod.rs`
- `object_cache/src/cache.rs`
- `executor_manager/src/stream_handler.rs`

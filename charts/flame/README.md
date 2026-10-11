# Flame Helm Chart

This chart installs a static Flame cluster on Kubernetes:

- one `flame-session-manager`
- a configurable number of `flame-object-cache` replicas
- one privileged Host-shim `flame-executor-manager` per selected worker node

Kubernetes manages persistent FEM workers; Flame schedules application services
and tasks within those workers. The chart always installs executor managers as a DaemonSet,
with host networking, host PID access, and root privileges. Object-cache capacity follows
`objectCache.replicas`; each StatefulSet replica owns its own cache volume when
persistence is enabled.

## Host workers

Select a dedicated worker pool using node labels and, when needed, taints and
tolerations. FEM discovers the whole host's CPU, memory, and GPU resources.
The Host shim launches services inside the FEM container.
For example:

```yaml
cluster:
  executors:
    shim: host
    devices: [gpu]
executorManager:
  hostNetwork: true
  hostPID: true
  securityContext:
    privileged: true
    runAsNonRoot: false
    runAsUser: 0
    runAsGroup: 0
  nodeSelector:
    kubernetes.io/os: linux
    flame.xflops.io/worker: "true"
  terminationGracePeriodSeconds: 300
```

`cluster.executors.devices` selects FEM device managers and defaults to `[gpu]`.
Set it to `[]` to disable device discovery and advertise CPU and memory only.
GPU is currently the implemented manager; configuring an unsupported device
name (such as `tpu`) fails FEM startup rather than silently skipping it.

FEM registers using its Pod hostname. Host-networked workers use
`ClusterFirstWithHostNet` DNS so the session-manager and cache Services resolve.
Workers assume the whole host is available to Flame. Use dedicated worker nodes
and leave FEM CPU and memory limits unset. Host discovery does not subtract
resources used by Kubernetes or other services. GPU workers require a compatible
image and host driver libraries.

For GPU workers, expose the host NVIDIA devices and driver libraries to FEM.
With GPU Operator drivers installed under `/run/nvidia`, add:

```yaml
executorManager:
  extraVolumes:
    - name: host-devices
      hostPath:
        path: /dev
        type: Directory
    - name: nvidia-driver
      hostPath:
        path: /run/nvidia
        type: DirectoryOrCreate
  extraVolumeMounts:
    - name: host-devices
      mountPath: /dev
    - name: nvidia-driver
      mountPath: /run/nvidia
      readOnly: true
  extraEnv:
    - name: LD_LIBRARY_PATH
      value: /run/nvidia/driver/usr/lib/x86_64-linux-gnu
    - name: PATH
      value: /run/nvidia/driver/usr/bin:/usr/local/flame/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
```

These paths match x86_64 GPU Operator hosts; adapt them to the host driver
installation. NVML provides GPU discovery, and Host services inherit the devices
and library paths. CPU workers in the same DaemonSet report zero GPUs.

SRE must arrange a maintenance window and ensure workers have no tasks before
updating or deleting FEM Pods. Kubernetes enforces `terminationGracePeriodSeconds`;
FEM does not automatically drain tasks on termination. DaemonSet updates use
`OnDelete`, so config or image changes are applied by deleting worker Pods during
the maintenance window.

Session-manager and object-cache Pods retain their own security and networking
settings. The chart does not create application Pods or use a Kubernetes provider.

Image versions are configurable through `global.imageTag`; each component can
also set its own `image.tag`. For testing with `latest`, use
`global.imagePullPolicy=Always` to pull the current image on each Pod start.

## Install

Executor limits have explicit application, session, and node scopes:

```yaml
cluster:
  limits:
    maxSessions: 1000
    application:
      maxExecutors: null
    session:
      maxExecutors: null
    node:
      maxExecutors: 128
```

`maxSessions` limits the total session count. Optional application and session
`maxExecutors` values are hard ceilings: the scheduler applies the lower of the
cluster ceiling and the application's or session's requested `max_instances`.
A `null` ceiling leaves the requested limit in effect. Node `maxExecutors`
limits active executors on each node. Idle executors continue to count toward
application and node limits; binding executors count toward session limits.
An application or node ceiling of `0` prevents creating new executors; retained
executors can still bind within the session limit. A session ceiling of `0`
prevents binding or reserving executors for that session.

The previous `cluster.limits.maxExecutors` value has moved to
`cluster.limits.node.maxExecutors`. Update existing values files and `--set`
arguments before installing; the chart rejects the old key to avoid silently
discarding an existing limit.

Use `flame-system` as the default namespace. For each test iteration, uninstall
the existing release, update the Helm values, and install again:

```bash
helm uninstall flame --namespace flame-system --ignore-not-found --wait
# Update your Helm values for the next test.
helm install flame ./charts/flame --namespace flame-system --create-namespace \
  --set global.imageTag=latest --set global.imagePullPolicy=Always
```

For local Kind testing without persistent volumes:

```bash
helm install flame ./charts/flame \
  --namespace flame-system \
  --create-namespace \
  --set sessionManager.persistence.enabled=false \
  --set objectCache.persistence.enabled=false
```

To install multiple object-cache replicas:

```bash
helm install flame ./charts/flame \
  --namespace flame-system \
  --create-namespace \
  --set objectCache.replicas=3
```

## Verify

```bash
helm test flame --namespace flame-system
kubectl -n flame-system get pods,svc
```

Port-forward for local inspection:

```bash
kubectl -n flame-system port-forward svc/flame-session-manager 8080:8080
kubectl -n flame-system port-forward svc/flame-object-cache 9090:9090
```

Then configure local clients:

```bash
export FLAME_ENDPOINT=http://127.0.0.1:8080
export FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090
```

Port-forwarded cache endpoints are suitable for local inspection. Package
deployment flows should use a cache endpoint that is reachable by both the
client and executor-manager pods.

## External access

The chart creates and configures only in-cluster Services. It does not create,
configure, or record external LoadBalancers, gateways, certificates, DNS
names, or routes. A site administrator who wants external access must provide
both external paths independently:

- a frontend-only LoadBalancer for the session manager; and
- a TLS-terminating gRPC proxy and LoadBalancer for the object cache.

The session-manager LoadBalancer must expose only the frontend port and route
it to the Flame session-manager Service. The backend/executor-facing port must
remain internal.

The object-cache proxy must:

- accept HTTP/2 gRPC connections over TLS at its public address;
- present a certificate valid for that address;
- route initial operations carrying the public proxy authority to the Flame
  object-cache Service on `objectCache.service.port`; and
- accept an owning cache endpoint from an object reference in the
  `x-flame-object-cache` gRPC metadata header and route it to that specific
  cache replica, while keeping the public proxy name in the client request's
  HTTP/2 `:authority` header.

The gateway must restrict replica routing to endpoints belonging to the Flame
object-cache Service; it must not act as an unrestricted dynamic forward proxy.
The object-cache Service remains `ClusterIP`. The external LoadBalancer must
not directly expose the cache StatefulSet or its pods.

The external cache endpoint is
`grpcs-proxy://<address>:<proxy-port>`. Clients dial that proxy over TLS and
send the owning cache replica endpoint as `x-flame-object-cache` metadata for
operations on a returned object reference. The chart-managed client
configuration always uses the in-cluster Services. The site administrator
creates a separate external configuration with the session-manager
LoadBalancer endpoint
and this `grpcs-proxy://` endpoint; both configurations use the same schema.

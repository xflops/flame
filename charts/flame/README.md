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

FEM registers using its Pod hostname. Host-networked workers use
`ClusterFirstWithHostNet` DNS so the session-manager and cache Services resolve.
Workers assume the whole host is available to Flame. Use dedicated worker nodes
and leave FEM CPU and memory limits unset. Host discovery does not subtract
resources used by Kubernetes or other services. GPU workers require a compatible
image and host driver libraries.

SRE must arrange a maintenance window and ensure workers have no tasks before
updating or deleting FEM Pods. Kubernetes enforces `terminationGracePeriodSeconds`;
FEM does not automatically drain tasks on termination. DaemonSet updates use
`OnDelete`, so config or image changes are applied by deleting worker Pods during
the maintenance window.

Session-manager and object-cache Pods retain their own security and networking
settings. The chart does not create application Pods or use a Kubernetes provider.

## Install

```bash
helm install flame ./charts/flame --namespace flame --create-namespace
```

For local Kind testing without persistent volumes:

```bash
helm install flame ./charts/flame \
  --namespace flame \
  --create-namespace \
  --set sessionManager.persistence.enabled=false \
  --set objectCache.persistence.enabled=false
```

To install multiple object-cache replicas:

```bash
helm install flame ./charts/flame \
  --namespace flame \
  --create-namespace \
  --set objectCache.replicas=3
```

## Verify

```bash
helm test flame --namespace flame
kubectl -n flame get pods,svc
```

Port-forward for local inspection:

```bash
kubectl -n flame port-forward svc/flame-session-manager 8080:8080
kubectl -n flame port-forward svc/flame-object-cache 9090:9090
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

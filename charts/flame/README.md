# Flame Helm Chart

This chart installs a static Flame cluster on Kubernetes:

- one `flame-session-manager`
- a configurable number of `flame-object-cache` replicas
- a configurable number of `flame-executor-manager` replicas

It does not implement the future Kubernetes provider or application pod
autoscaling. Executor capacity is static and follows
`executorManager.replicas`. Object-cache capacity follows
`objectCache.replicas`; each StatefulSet replica owns its own cache volume when
persistence is enabled.

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
- accept an owning cache endpoint from an object reference in the HTTP/2
  `:authority` header and route it to that specific cache replica.

The gateway must restrict replica routing to endpoints belonging to the Flame
object-cache Service; it must not act as an unrestricted dynamic forward proxy.
The object-cache Service remains `ClusterIP`. The external LoadBalancer must
not directly expose the cache StatefulSet or its pods.

The external cache endpoint is
`grpcs-proxy://<address>:<proxy-port>`. Clients dial that proxy over TLS and
forward the owning cache replica endpoint as the gRPC authority for operations
on a returned object reference. The chart-managed client configuration always
uses the in-cluster Services. The site administrator creates and distributes a
separate external configuration with the session-manager LoadBalancer endpoint
and this `grpcs-proxy://` endpoint; both configurations use the same schema.

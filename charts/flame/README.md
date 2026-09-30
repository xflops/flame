# Flame Helm Chart

This chart installs a static Flame cluster on Kubernetes:

- one `flame-session-manager`
- a configurable number of `flame-object-cache` replicas
- a configurable number of `flame-executor-manager` replicas in plaintext mode

It does not implement the future Kubernetes provider or application pod
autoscaling. Executor capacity is static and follows
`executorManager.replicas` (default 3 in plaintext mode). Object-cache capacity
follows `objectCache.replicas`; each StatefulSet replica owns its own cache
volume when persistence is enabled. Secure mode supports
`executorManager.replicas=1` only.

## Install

By default the chart omits `context.security`, so Flame uses `NoneSecManager`
and serves plaintext `http://` and `grpc://` endpoints:

```bash
helm install flame ./charts/flame --namespace flame --create-namespace
```

To enable `RBACSecManager`, create three TLS Secrets: one each for the session
manager, object cache, and executor manager. Each Secret must contain a
certificate (`tls.crt`), its private key (`tls.key`), and the shared issuing
CA (`ca.crt`). The object-cache certificate needs both server and client use,
its in-cluster DNS name, and a `flame/system/cache/<node-name>` URI SAN: the
cache uses it to serve clients and list applications from the session manager.
The executor-manager certificate carries a `flame/system/node/<node-name>` URI
SAN and is used to connect to both the session manager and the cache. This
identity may read application packages but cannot write them or read session
data without an app token. Tenant users use `flame/user/<user-id>` certificates
and configured Roles. All components verify client certificates against the
configured CA.

The executor-manager SystemNode URI SAN must end with the chart's exact
executor-manager hostname (for the default release, `flame-executor-manager`).
The secure chart sets that pod hostname explicitly and rejects more than one
executor-manager replica. Plaintext installations can scale it above one.

All object-cache replicas must use the same TLS private key because app tokens
are derived from it. Rotating that key invalidates existing tokens, including
tokens held by running executors. Renewing only the certificate while keeping
the private key preserves the tokens.

The cache currently assumes a single-user cluster: any verified tenant mTLS
identity can call `Delegate(app)` for any application. The resulting user token
authorizes permitted cache data across applications independently of session-manager
Roles. Configure tenant certificates with this trust boundary in mind.

```bash
helm install flame ./charts/flame --namespace flame --create-namespace \
  --set security.enabled=true \
  --set executorManager.replicas=1 \
  --set security.trustDomain=flame.local \
  --set tls.sessionManager.secretName=flame-sm-tls \
  --set tls.objectCache.secretName=flame-cache-tls \
  --set tls.executorManager.secretName=flame-em-tls
```

The chart mounts each workload's Secret at the same path and writes one
top-level `security.tls` path into the shared service ConfigMap. The cache
uses its certificate when listing applications from the session manager, and
the executor manager uses its own certificate when reading app packages from
the cache. Use distinct service Secrets so each certificate carries only its
intended system identity.

For local Kind testing without persistent volumes:

```bash
helm install flame ./charts/flame \
  --namespace flame \
  --create-namespace \
  --set security.enabled=true \
  --set executorManager.replicas=1 \
  --set tls.sessionManager.secretName=flame-sm-tls \
  --set tls.objectCache.secretName=flame-cache-tls \
  --set tls.executorManager.secretName=flame-em-tls \
  --set sessionManager.persistence.enabled=false \
  --set objectCache.persistence.enabled=false
```

To install multiple object-cache replicas:

```bash
helm install flame ./charts/flame \
  --namespace flame \
  --create-namespace \
  --set security.enabled=true \
  --set executorManager.replicas=1 \
  --set tls.sessionManager.secretName=flame-sm-tls \
  --set tls.objectCache.secretName=flame-cache-tls \
  --set tls.executorManager.secretName=flame-em-tls \
  --set objectCache.replicas=3
```

## Verify

```bash
kubectl -n flame get pods,svc
```

Port-forward for local inspection:

```bash
kubectl -n flame port-forward svc/flame-session-manager 8080:8080
kubectl -n flame port-forward svc/flame-object-cache 9090:9090
```

Then configure local clients for the default plaintext deployment:

```bash
export FLAME_ENDPOINT=http://127.0.0.1:8080
export FLAME_CACHE_ENDPOINT=grpc://127.0.0.1:9090
```

For RBAC deployments, use `https://` and `grpcs://` with a user certificate.
The certificates must also cover the hostnames used for port forwarding, or
the client must connect through DNS names covered by the certificates. Package
deployment flows should use a cache endpoint that is reachable by both the
client and executor-manager pods. Clients need their own user certificate,
private key, and CA file. `clientConfig.enabled` is disabled by default; in
RBAC mode, provide `clientConfig.tls.certFile`, `keyFile`, and `caFile` paths
for the intended user when enabling it.
The generated client `flame.yaml` places these paths under both
`contexts[].cluster.tls` and `contexts[].cache.tls`, using the keys
`cert_file`, `key_file`, and `ca_file`. `CacheStorage` obtains an app token
to upload and clean up `app/pkg/*`; the executor manager downloads packages
using its SystemCache mTLS identity.

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

`Delegate(app)` verifies the tenant certificate at the object-cache server. The
chart does not configure trusted identity forwarding through a TLS-terminating
proxy, so delegation requires direct mTLS access to the cache Service. Data RPCs
over TLS accept either a verified tenant certificate or a user token in
`x-flame-delegation-token`. Either may put, patch, or delete `app/pkg/*`,
while package reads, bootstrap paths, and cache listing require SystemCache
mTLS identity.

The chart-managed client configuration uses in-cluster Services. A separate
external client configuration must use endpoints reachable by the client and
provide that user's TLS identity and CA file.

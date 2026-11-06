# RFE523: Helm External Access and Object-Cache gRPC Proxy

> The cache-owner routing contract follows
> [RFE552](../RFE552-cache-client-mtls/FS.md): SDKs send the owning cache
> `host:port` in `x-flame-object-cache` metadata.

GitHub issue: https://github.com/xflops/flame/issues/523

## Summary

Define how a site administrator can expose the Flame session manager and
object cache to clients outside the Kubernetes cluster without changing their
in-cluster endpoints. The administrator publishes the session-manager frontend
through a separately managed LoadBalancer and the object cache through a
gRPC proxy with a plaintext HTTP/2 or TLS listener. The Python and Rust
object-cache clients support `grpc-proxy://` and `grpcs-proxy://` endpoint forms
that separate the public connection address from the owning object-cache
replica target sent in routing metadata for reference-based operations.

Flame does not create, attach to, or configure any external-access resources.
A site administrator provisions both external LoadBalancers, the cache proxy,
the selected transport listener, DNS, and routes, then generates an external
client configuration.

## Goals

1. Preserve the chart's internal-only Service and client configuration model.
2. Preserve the existing `grpc://` object-cache behavior.
3. Add an explicit proxy endpoint whose TCP destination is the administrator's
   gateway and whose `x-flame-object-cache` metadata identifies the owning
   cache endpoint from an `ObjectRef`.
4. Define the networking contract an administrator-provisioned gRPC gateway
   must satisfy without coupling the Flame chart to a gateway implementation.
5. Keep every installation internal-only and free of new external or
   cluster-scoped resources.

## Non-goals

- Implementing a generic HTTP, SOCKS, or CONNECT proxy.
- Automatically detecting whether a compatible gateway is installed.
- Managing DNS records or cloud-provider load balancers outside Kubernetes.
- Terminating session-manager traffic at the cache gateway.
- Changing the object-cache server protocol or Arrow Flight API.
- Making the object-cache StatefulSet itself externally reachable when proxy
  mode is selected.
- Installing, configuring, or validating Gateway API, Envoy Gateway, Envoy,
  certificates, public DNS, or routes from the Flame chart.
- Creating or configuring an external session-manager Service or LoadBalancer.
- Describing external addresses through Helm values or release notes.

## Current state

The chart creates internal Services for the session manager and object cache.
They are `ClusterIP` Services and the chart exposes no external-access API.

The chart-generated client configuration always points at the internal cache
Service. The Helm connection test calls the session manager only and therefore
does not verify cache traffic.

The Rust client parses cache endpoints in `sdk/rust/src/object.rs`. It accepts
`grpc://`, `grpcs://`, and the `grpc+tls://` alias for direct connections.
Proxy endpoints dial the configured proxy and carry the owning replica target
in per-request `x-flame-object-cache` metadata.

## Architecture

Clients use the Flame-owned in-cluster Service for initial cache operations.
The replica selected by that Service advertises its own internal endpoint in
each returned `ObjectRef`, because objects are replica-local. The internal data
path is:

```text
session/executor managers --> flame-object-cache ClusterIP Service:9090 (initial operation)
session/executor managers --> owning cache replica from ObjectRef (reference operation)
executor managers --> flame-session-manager ClusterIP:8080/8081
```

External traffic follows two independent paths:

```text
external client --> admin-owned session-manager LoadBalancer --> session-manager Service

external client --> admin-owned external LB Service --> Envoy --> cache Service or owning replica
                  public proxy dial target                 routing metadata selects the target
```

The administrator's session-manager LoadBalancer exposes only the frontend
port. The Flame-owned internal Service continues to expose both ports for
component-to-component communication. The external resource is not part of the
Flame release and must not publish the backend port.

The object cache remains behind its Flame-owned `ClusterIP` Service. The
administrator owns a separate external `LoadBalancer` Service fronting Envoy,
plus its listener, any TLS certificate, and routing. Outside clients use that
load balancer only as the `grpc-proxy://` or `grpcs-proxy://` connection target.
Envoy reads the owning cache endpoint advertised in an `ObjectRef` from the
`x-flame-object-cache` request header and forwards the request to that replica
and gRPC port. Initial operations omit that header and go to the cache Service.
The external load balancer must not directly expose the cache StatefulSet or
its pods. Flame
neither renders nor owns resources in either public path.

## Helm ownership boundary

There are no external-access Helm values. Both Flame Services are `ClusterIP`,
and the chart always generates an in-cluster client configuration. It accepts
no public addresses, provider annotations, LoadBalancer settings, certificates,
or gateway configuration.

The site administrator owns both external entry points and creates a separate
client configuration using the same Flame configuration structure. That
configuration names the session-manager LoadBalancer and uses
`grpc-proxy://<proxy-host>:<proxy-port>` or
`grpcs-proxy://<proxy-host>:<proxy-port>` for the cache. The administrator's
gateway must route the `x-flame-object-cache` target from an `ObjectRef`
to the cache replica that produced it; routing reference operations through
the Service is incorrect for replica-local objects.

## Proxy endpoint semantics

### URI format

The configured proxy formats are:

```text
grpc-proxy://<proxy-host>:<proxy-port>
grpcs-proxy://<proxy-host>:<proxy-port>
```

For example:

```text
grpcs-proxy://cache.example.com:9090
```

`proxy-host` and `proxy-port` are the TCP connection destination. The client
establishes plaintext HTTP/2 gRPC for `grpc-proxy://` or HTTP/2 gRPC over TLS
for `grpcs-proxy://`. The scheme selects the transport explicitly; TLS never
falls back to plaintext. Both schemes require an explicit port. For an
operation on an `ObjectRef`, the client takes the owning cache `host:port`
from `ObjectRef.endpoint` and sends it in the `x-flame-object-cache` gRPC
metadata field, represented as an HTTP/2 request header. Gateways route by
this header:

| Operation | `x-flame-object-cache` | Gateway destination |
| --- | --- | --- |
| Initial upload/put | Absent | Cache Service |
| Reference operation | Owner `host:port` | Owning cache replica |

For `grpcs-proxy://`, the client sends the proxy host as TLS SNI and validates
the proxy certificate against that host. Thus a certificate for
the internal object-cache endpoint is neither required nor sufficient;
the proxy certificate must cover the public proxy DNS name. IP proxy hosts
require a matching IP subject alternative name. The client uses its configured
TLS trust roots (the platform roots by default); private proxy CAs must be
added to that trust configuration. Disabling certificate or hostname
verification is not part of this design.

The owning cache routing target includes a port. For example, an object at
`grpc://10.0.1.42:9090` used with this configured proxy dials the proxy and
sends `x-flame-object-cache: 10.0.1.42:9090`:

```text
grpcs-proxy://cache.example.com:443
```

User information, query parameters, fragments, and non-root paths are
rejected for both proxy schemes.

### Direct endpoint compatibility

`grpc://host:port` continues to connect directly. `grpcs://` and `grpc+tls://` retain their existing behavior.
No existing serialized `ObjectRef` changes format.

Initial operations configured with either proxy scheme dial the proxy directly
using the selected transport, without an
`x-flame-object-cache` header; the gateway routes them to the cache Service.
The server-returned `ObjectRef` contains the accepting cache replica's internal endpoint. In-cluster clients connect to that replica
directly.
Reference-based operations consult the current cache configuration; when it is
`grpc-proxy://` or `grpcs-proxy://`, they dial that proxy and use the reference
address as the `x-flame-object-cache` metadata value. Without proxy
configuration, the same reference connects directly.

The client pool key, if a pool is used, includes both proxy destination and
owner target when retaining a routed client. A shared transport channel may
serve multiple owners, provided each RPC carries the correct owner metadata.

## Administrator-provisioned gateway contract

Before users select external cache access, a cluster administrator must deploy
and configure a suitable gateway. Envoy Gateway and standalone Envoy are
supported examples, but neither is required by or installed with Flame. The
gateway must:

- expose a plaintext HTTP/2 listener for `grpc-proxy://`, or a TLS listener
  for `grpcs-proxy://`, at the configured host and port;
- for TLS, present a certificate whose DNS or IP subject alternative name
  covers that public proxy address and use that address for TLS SNI and
  verification;
- accept HTTP/2 gRPC traffic using the selected transport;
- route requests without `x-flame-object-cache` to the
  `flame-object-cache` ClusterIP Service;
- route requests with `x-flame-object-cache` to the owning cache `host:port`
  named in that header and its gRPC port; and
- speak the backend protocol expected by that Service (HTTP/2 gRPC, normally
  cleartext h2c inside the cluster unless the deployment separately enables
  and configures backend TLS).

An `ObjectRef` owner target identifies an individual cache replica. The gateway
may discover eligible cache-pod endpoints from the Service or use an
equivalent implementation-specific allow-list, but it must forward only to
replicas belonging to the intended Flame object-cache Service. It must not
treat the routing header as an unrestricted dynamic-forward-proxy target.

Gateway readiness, route status, certificates,
DNS, source restrictions, observability, and lifecycle are entirely the
administrator's responsibility. Flame documentation supplies this contract
but no deployable Gateway API or Envoy configuration.

## Security and TLS

Administrator-provided external access is optional. Administrators should set
load-balancer source ranges or equivalent provider policy whenever public
Internet access is not required.

`grpcs-proxy://` always uses gRPC over TLS from the client to the gateway. The
gateway terminates TLS using its listener certificate. It forwards initial
operations to the internal cache Service and reference operations to the cache
replica selected by the `x-flame-object-cache` header derived from the original
`ObjectRef.endpoint`. The gateway-to-cache hop remains an in-cluster connection governed by the
administrator's backend policy. `grpc-proxy://` explicitly selects plaintext
HTTP/2 from the client to the gateway and does not require a certificate.

Authentication is out of scope; deployments requiring it should apply
operator-managed security policies for the selected gateway.

The administrator-managed session-manager frontend uses the server's existing
HTTP/HTTPS configuration. Flame does not generate its external certificates.
For HTTPS, the certificate must be valid for the public name clients use.

## Compatibility

- Existing Service names, types, ports, selectors, and internal endpoints are
  unchanged and remain internal.
- Existing `grpc://`, `grpcs://`, and `grpc+tls://` configurations remain
  valid.
- Installations never require Gateway API CRDs or an Envoy installation to
  render or install the Flame chart.
- The chart must not require cluster-admin permissions in its default mode.
- A rollback affects only resources owned by the Flame release and never
  modifies or removes either administrator-managed external entry point.

## Failure modes

| Failure | Expected behavior and diagnosis |
| --- | --- |
| Administrator gateway is absent or misconfigured | The proxy address is unreachable or returns a gateway error. Flame installation and internal cache access remain unaffected. |
| Gateway ignores the owner routing header | Reference requests may reach the wrong replica. Administrators compare `x-flame-object-cache` in gateway logs with `ObjectRef.endpoint` and route by that header. |
| Owner target is malformed | The client rejects the object reference before connecting. |
| Proxy address is unreachable | Client reports a connection error naming the proxy destination. |
| Origin backend is unavailable | The gateway reports an upstream failure; internal cache readiness and gateway backend status guide diagnosis. |
| Public address is not ready | Administrators inspect their gateway/load-balancer status and cloud-controller events. Internal access remains available. |
| Malformed `grpc-proxy://` or `grpcs-proxy://` URI | Client returns `InvalidConfig` before attempting a connection. |
| Selected transport does not match the gateway listener | The connection fails; the client does not automatically switch between plaintext and TLS. |
| Proxy certificate is untrusted or does not cover the proxy host | TLS fails before any gRPC request; the client does not fall back to cleartext. |
| Gateway certificate or listener is absent or invalid | The administrator-managed TLS listener does not become ready; gateway status and logs identify the failure. |
| Multiple cache replicas lack shared semantics | Existing cache consistency constraints remain; the proxy does not claim to add replication or affinity. |

Client errors should distinguish invalid endpoint configuration, proxy
connection failure, and gRPC upstream failure. They must not log query values
other than the validated owner target if future sensitive options are added.

## Rollout plan

1. Add parser and channel-construction tests for direct and proxy endpoints.
2. Document the chart's internal-only ownership boundary and add render tests
   proving that it creates no external resources.
3. Ask a site administrator to deploy the frontend LoadBalancer and a
   conforming cache gateway with a plaintext HTTP/2 listener or a TLS listener
   and certificate for the external proxy name.
4. Verify its listener, owner-header handling, backend routing, and transport behavior
   against the contract in this document.
5. Generate a staging external client configuration from the two
   administrator-managed endpoints.
6. Exercise upload, download, get, put, delete, and package deployment from an
   external client.
7. Roll back the release and confirm internal traffic remains functional and
   both administrator-managed external paths remain present and unchanged.

## Verification

### Unit tests

Rust tests cover:

- unchanged parsing and connection construction for `grpc://`, `grpcs://`,
  and `grpc+tls://`;
- parsing both proxy schemes and deriving the owner routing header from an object reference;
- plaintext HTTP/2 for `grpc-proxy://` and TLS for `grpcs-proxy://`;
- default and explicit ports, DNS names, and IPv4/IPv6;
- rejection of paths, queries, fragments, user information, and unsupported
  schemes;
- a connector test proving the socket target is the proxy while request
  `x-flame-object-cache` identifies the owner; and
- preservation of the accepting cache replica endpoint in returned `ObjectRef`
  values.

Run:

```bash
cargo test -p flame-rs
```

### Helm rendering

```bash
helm lint charts/flame
helm template flame charts/flame
```

The render contains no external LoadBalancer, Gateway API, Envoy, cache route,
or external TLS resources. Its client configuration names only in-cluster
Services. Render assertions prevent external resources and public endpoint
configuration from being introduced into the chart.

### Kubernetes integration

In a disposable Kubernetes cluster:

1. Have the site administrator install a frontend-only session-manager
   LoadBalancer and configure a cache gateway satisfying the contract above,
   including a plaintext HTTP/2 listener or a TLS listener and certificate,
   routing requests without an owner header to the cache Service, and routing
   `x-flame-object-cache` targets to their owning cache replicas.
2. Install Flame normally; no external-access Helm options are used.
3. Wait for the administrator-managed listener and backend route to become
   ready using the gateway implementation's own status mechanisms.
4. Resolve the public proxy address and, when using TLS, arrange for the test
   client to trust the proxy certificate's issuing CA.
5. Generate an external configuration with the session-manager LoadBalancer
   endpoint and the cache proxy's selected `grpc-proxy://` or `grpcs-proxy://`
   endpoint.
6. Run `flmctl list -a`, then upload and download an object/package and execute
   a task that reads it.
7. Capture gateway access logs or a test upstream assertion showing that an
   initial request reaches a Service-selected replica and a subsequent request
   carries that replica's `host:port` in `x-flame-object-cache` and returns to
   the same replica. Verify that initial requests omit the header. When using
   TLS, also verify the public proxy name as SNI.
8. Repeat cache operations with the other proxy transport and its matching
   listener, then run `helm test`, upgrade the release, and roll it back.
9. Uninstall Flame and prove the administrator-managed gateway, listener,
   certificate, and route were not deleted or modified.

Success requires both external flows to work, direct internal cache access to
remain functional, and the default chart behavior to be byte-for-byte
equivalent in resource topology apart from deliberate metadata changes.

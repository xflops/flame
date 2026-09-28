# Design: Client mTLS for Object-Cache Proxy Access

Tracking issue: [#552](https://github.com/xflops/flame/issues/552).

## Status and scope

This design updates the `grpcs-proxy://` contract in
[RFE523](../RFE523-helm-external-access-and-cache-proxy/FS.md). It covers the
Python and Rust SDK connections from a client to an administrator-managed
object-cache gateway. The gateway's connection to the cache replicas, gateway
deployment, certificate issuance, and authorization policy are separate work.

## 1. Motivation

A cache gateway may require a client certificate. The SDK configuration has a
CA file for verifying the gateway but no client identity. Reference operations
also need two different names: the public gateway name for TLS and the internal
cache owner from `ObjectRef.endpoint` for routing. A gateway certificate
should need only the public name in its subject alternative names.

The Python implementation currently sets `grpc.default_authority` to the
internal owner on a secure channel. gRPC's secure naming check can reject the
RPC when the gateway certificate covers only the public proxy name. The owner
is a routing value, so it can be sent as gRPC metadata while the HTTP/2
authority retains its normal public proxy name.

### Goals

1. Configure a client certificate and private key alongside the existing CA.
2. Verify the public proxy certificate against its public DNS name or IP while
   presenting the client certificate during the TLS handshake.
3. Keep HTTP/2 `:authority` at the public proxy name, and pass the owner
   `host:port` in a routing metadata field for reference operations.
4. Preserve direct `grpc://`, `grpcs://`, and `grpc+tls://` behavior and clients
   with no configured client certificate.
5. Fail on an incomplete or unreadable client identity without downgrading to
   an unauthenticated connection.

## 2. Function Specification

### Client configuration

The existing client context supports the same TLS fields in `cluster.tls` and
`cache.tls`. The cache proxy uses `cache.tls`:

```yaml
current-context: external
contexts:
  - name: external
    cluster:
      endpoint: https://session.example.com
    cache:
      endpoint: grpcs-proxy://cache.example.com:443
      tls:
        ca_file: /etc/flame/cache-gateway-ca.pem
        cert_file: /etc/flame/client-cert.pem
        key_file: /etc/flame/client-key.pem
```

`ca_file` supplies the trust bundle for gateway verification. With no CA file,
the TLS library uses its default roots. `cert_file` is a PEM certificate chain
and `key_file` is its PEM private key. Both identity paths must be present
together; neither is required when the gateway does not require client
authentication. The client reads files at connection creation, reports missing,
unreadable, or invalid material as an error, and does not fall back to anonymous
TLS after an identity error. Protect the private-key file with filesystem
permissions; the key is not embedded in an `ObjectRef` or application manifest.

`FLAME_CERT_FILE` and `FLAME_KEY_FILE` provide the same environment-based
configuration path as `FLAME_CA_FILE`. As with the existing CA setting, they
fill unset TLS fields in the client context; explicit YAML fields take
precedence. A deployment that uses different cluster and cache identities
should set their paths separately in the YAML context. Environment and YAML
values must still form a complete certificate and key pair.

Rotating a file in place affects new TLS connections only. Existing channels
retain their negotiated identity. A Python caller closes cache channels with
`await flamepy.core.aio.cache.close()` before reuse; a process restart is also
sufficient. Rust cache operations create connections as needed and read the
files when creating each connection. Gateway trust-bundle rotation follows the
same connection boundary. Deploy the new trust and identity files before
retiring the old credentials to avoid a gap in connectivity.

### Proxy connection and routing

For a configured `grpcs-proxy://cache.example.com:443` endpoint:

| Operation | Public dial target and `:authority` | Target metadata |
| --- | --- | --- |
| Initial upload/put | `cache.example.com:443` | Absent |
| Reference get/write/download | `cache.example.com:443` | Owner `host:port` |

The TLS client sends the public proxy host as SNI and verifies the peer
certificate against that host; a proxy specified by IP needs an IP subject
alternative name. The client certificate is sent when requested by the gateway.
HTTP/2 `:authority` remains the public proxy `host:port` for every RPC.
`x-flame-object-cache` is a gRPC metadata field, represented as a normal
HTTP/2 request header, containing only the validated owning cache `host:port`.
Initial operations omit it; the gateway sends them to the cache Service.
Reference operations include it; the gateway sends them to the named replica.
The gateway restricts accepted targets to known cache replicas. It may forward
the routing header upstream; the cache service ignores unknown gRPC metadata.
`ObjectRef` serialization does not change.

An absent client identity retains ordinary server-authenticated TLS. A gateway
configured to require client certificates rejects that connection. An
untrusted gateway certificate or hostname mismatch fails before any cache RPC
is accepted. The client must never disable certificate or hostname checks to
make proxy routing work.

Direct `grpc://` continues as plaintext gRPC. Direct `grpcs://` and
`grpc+tls://` continue to verify their own dial host and may present the same
configured client identity. Direct endpoints do not send proxy routing metadata.

## 3. Implementation Detail

### Gateway fixture and administrator contract

The Kubernetes E2E gateway uses Envoy Gateway's `HTTPRouteFilter` to rewrite
the upstream hostname from `x-flame-object-cache`, then a `DynamicResolver`
backend to reach the owner. A header match chooses this route only when the
target is present; otherwise the bootstrap route selects the cache Service.
The owner route's `SecurityPolicy` permits only current object-cache pod
addresses. This allow-list is required because the target is client supplied.
The E2E TLS listener has a certificate for the public proxy name only and
requires a client certificate signed by its configured CA.

### Rust SDK

Extend `FlameClientTls` in `sdk/rust/src/apis/ctx.rs` with optional
`cert_file` and `key_file`. Validate their pairing, read PEM content, and set a
tonic `Identity` on `ClientTlsConfig`. `sdk/rust/src/object.rs` already builds a
tonic endpoint from the public proxy host. Keep its normal public origin and
TLS domain. Attach `x-flame-object-cache` metadata to reference RPCs using the
validated owner `host:port`; omit it for initial operations and direct
connections. Propagate malformed configuration rather than silently dropping
TLS identity on reference operations.

### Python SDK

Extend `FlameClientTls` and context parsing in
`sdk/python/src/flamepy/core/types.py` with the same PEM paths. Pass the PEM
chain and key into the gRPC credentials for ordinary direct TLS channels.

For proxy operations, create a normal secure gRPC channel to the public proxy
host. Do not set `grpc.default_authority` to the internal owner. Reference RPCs
carry `x-flame-object-cache` as per-call metadata. Keep the native gRPC
transport, including its TLS verification, streaming, cancellation, and
channel lifecycle.

The pool key includes the dial endpoint, owner target, and TLS file paths, so
a cached wrapper cannot route to a different owner. Closing the pool releases
channels and causes rotated credentials to be read on the next connection.
Configuration errors must be surfaced rather than swallowed when a reference
is resolved.

The route is trusted only as a gateway input. A client must validate the
`ObjectRef.endpoint` URI and extract its `host:port`; the gateway remains
responsible for enforcing which internal hosts are routable. The gateway
should reject invalid or unrecognized targets before forwarding.

## 4. Verification

1. Start a test gateway with a certificate whose SAN contains only the public
   proxy name. Require a client certificate signed by a distinct test CA.
2. Exercise initial upload and reference get, update, patch, and download with
   each SDK. Record TLS SNI, HTTP/2 `:authority`, and
   `x-flame-object-cache` at the gateway and confirm every reference operation
   reaches its owning replica.
3. Confirm a missing client identity is rejected by the mTLS gateway; a
   missing key, invalid PEM pair, untrusted gateway CA, or wrong gateway
   hostname fails closed with a useful error.
4. Repeat against a server-authenticated gateway with no client identity,
   then run direct `grpc://`, `grpcs://`, and `grpc+tls://` regressions.
5. Replace certificate and trust files and recreate the relevant connections;
   confirm the new identity and roots are used and stale channels are closed.

## References

- [RFE523: external cache proxy](../RFE523-helm-external-access-and-cache-proxy/FS.md)
- [gRPC A81 authority rewriting proposal](https://github.com/grpc/proposal/blob/master/A81-xds-authority-rewriting.md)

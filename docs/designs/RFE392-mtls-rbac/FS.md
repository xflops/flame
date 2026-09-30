# Design: mTLS, Application Roles, and Cache Identity Tokens

Tracking issue: [#392](https://github.com/xflops/flame/issues/392).

## Scope and resource names

Flame uses globally unique application names and session IDs. The existing
frontend requests continue to use `name` for application operations and
`session_id` for session and task operations. `SessionSpec.application` names
the application. There is no workspace or separate resource UID in this design.
Cache object paths are `<app>/<session>/<object>`; a write can use the
`<app>/<session>` prefix and let the cache assign an object ID. `pkg` and
`bootstrap` are reserved session path components for service data.

Security is optional. With top-level `security` configured, the session
manager uses mTLS to authenticate clients and applies application Roles. The
object cache accepts a verified tenant mTLS identity or TLS plus a delegated
user token for workload data requests. With security absent, `NoneSecManager` permits the
existing plaintext `http://` and `grpc://` flows.

This rollout does not migrate existing storage. The `common::storage`
interface and engines retain application names and session IDs as their
resource keys; the new SQLite migration stores Roles.

## Identity and transport

The security configuration names a trust domain, server certificate and key,
and CA. The session manager's frontend and backend require client certificates
that chain to that CA. A verified leaf certificate must contain one Flame URI
SAN in the configured trust domain:

```text
spiffe://<trust-domain>/flame/user/<username>
spiffe://<trust-domain>/flame/system/node/<node-name>
spiffe://<trust-domain>/flame/system/cache/<cache-name>
```

External identity management provisions certificates and revokes them. Flame
has no user registry and does not accept a request field, common name, or
metadata header as an identity. SDK clients use `https://` for the session
manager and `grpcs://` for the cache. A cache proxy can use
`grpcs-proxy://`; its TLS identity is checked against the public proxy name,
while `x-flame-object-cache` carries the cache replica address for routing.
The cache accepts a client certificate optionally so an executor workload can
connect with server-authenticated TLS and its identity token.
The executor manager supplies the CA certificate to each workload; workloads
do not receive a client certificate. Recursive calls from a workload to the
session-manager frontend are unsupported in secure mode because that endpoint
requires a tenant mTLS identity.

In secure mode the session-manager backend accepts node and executor control
RPCs only from `system/node/<node-name>` certificates. The certificate name
must match the node being registered, watched, or released, or the stored node
of the executor being controlled. A node may control executors for different
applications assigned to it. A tenant or cache certificate cannot act as a
node, even if a Role contains a wildcard grant. A node watch is bound to the
node named in its first heartbeat and rejects a later name change.

The secure Helm chart fixes the executor-manager pod hostname to the release's
executor-manager name and supports `executorManager.replicas=1` only; its
SystemNode certificate URI SAN must contain that exact hostname. Plaintext
installations may use more executor-manager replicas.

The cache system identity is separate from the node identity. It can list
applications through the session manager for cache garbage collection and
read packages from the cache over mTLS. It cannot edit Roles or control nodes.

## Application Roles

The session manager stores `Role { name, users, rules }`. Internally, `rules`
maps each object kind (`application`, `node`, or `*`) to a list of grants; each
grant pairs one object ID with its operations. The gRPC Role message keeps its
existing list of rules with `verbs` and `objects`, translated at the API
boundary. Grant matching is additive. Supported operations are `view`, `list`,
`update`, `delete`, and `*`. gRPC object selectors are:

| Selector | Meaning |
| --- | --- |
| `application:<name>` | One globally named application |
| `node:*` | All nodes |
| `node:<name>` | One node |
| `*` | All application and node objects |

The `application:` prefix keeps an application named `nodes` distinct from node
selectors. The built-in `Admin` Role initially grants `root` `*` on `*`.
The Role can be edited, but cannot be deleted. `SetRole`, `GetRole`,
`ListRoles`, and `DeleteRole` require `*` on `*`. Frontend application,
session, and task requests check the parent application's name. List responses
include only resources whose application is granted for the corresponding
verb. `GetNode` and `ListNodes` require a grant in the stored `Admin` Role;
other Roles do not grant node reads.

Missing or inaccessible individual resources return `NotFound`. A task watch
checks authorization when it starts and remains within that session. In a
deployment without security, the None security manager permits requests and
uses `root` as its local identity.

## Cache signing and data access

The object cache exposes `Delegate(CacheDelegateRequest { key: <app> })`. In the
current single-user cluster policy (each user's `flame.yaml` selects one cluster,
and clusters are not shared across users), **any verified tenant mTLS identity may
request a token with any valid application name**. The cache validates the name;
it does not fetch Roles or ask the session manager for permission. The security
manager delegates the verified tenant username into a signed JSON identity
payload with `name` and Unix-second `signed_at` fields.
The `Delegate` request's application name remains validated, but the token is not
bound to that name; it authorizes permitted cache data operations across
applications. The request does not need a session ID or object ID. This policy
does not isolate cache data between tenant users; application Roles govern
session-manager RPCs, not cache token issuance.

Secure cache data operations accept either a verified tenant certificate or
`x-flame-delegation-token` over TLS. The security manager verifies the signature
and recovers the tenant username. Both identities permit
`Put`, `Patch`, `Get`, `GetMetadata`, and `Delete` for normal session paths in
any application. For `<app>/pkg/...`, a valid token permits `Put`, `Patch`, and
`Delete`, so a tenant can deploy and clean up its package. Package `Get` and
`GetMetadata` require a system cache or
system node mTLS identity. Every operation on `<app>/bootstrap/...` requires
the system cache identity. `List` is system-cache-only. A system cache certificate can access
all cache paths without a user delegation token. Each stored object also has
a signature over its exact key, returned with metadata and carried in
`ObjectRef`. `Get` checks that signature against the requested key, including
conditional reads. Object version is not part of the signature.

The cache derives its Ed25519 token signing key from `security.tls.key_file`
using HKDF with a dedicated domain. Every cache replica must use the same TLS
private key so each can verify tokens signed by another. Rotating that key
invalidates existing tokens;
renewing the certificate with the same private key preserves them. Deleting a
session does not revoke an identity token
because it may serve other sessions and applications. Tokens are bearer credentials:
keep them out of URLs, command lines, and logs. `signed_at` is recorded but
does not enforce expiry or revocation in this version.

The executor manager downloads cache-backed application packages with its
system node mTLS identity and the package key signature. The application stores
`package.url` and `package.signature` separately; frontend application reads
redact the signature. Python `CacheStorage` requests a user delegation through
cache `Delegate(app)` for package upload and cleanup, then sends that token on
`<app>/pkg/...` writes or deletes.

## Verification

1. Reject missing, expired, untrusted, wrong-SAN, and wrong-trust-domain
   certificates at the session manager. Verify that tenant and cache identities
   cannot call backend node or executor controls.
2. Exercise application Roles with exact `application:<name>` and wildcard selectors,
   including an application named `nodes`. Check list filtering and denied
   direct reads, edits, and task watches.
3. Request a token with a tenant mTLS certificate. Verify it resolves to the
   original username and permits data requests across applications over TLS;
   verify the certificate alone permits the same data requests. Verify package upload,
   patch, and cleanup, but deny package reads and every bootstrap operation
   without the system cache identity. Check that cache List also requires it.
4. Test signing-key rotation, cache replica key sharing, and session deletion.
   Rotation invalidates old tokens; session deletion alone does not.

## References

- [RFE234 TLS design](../RFE234-tls/FS.md)
- [RFE552 cache client mTLS design](../RFE552-cache-client-mtls/FS.md)
- [SPIFFE ID specification](https://spiffe.io/docs/latest/spiffe-specs/spiffe-id/)
- `rpc/protos/frontend.proto`, `rpc/protos/cache.proto`, `rpc/protos/types.proto`
- `session_manager/src/apiserver/`, `object_cache/src/cache/grpc.rs`

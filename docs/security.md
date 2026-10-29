# Security

This document describes how Flame is designed with security in mind, the kinds
of errors that the project guards against, and what an operator has to do to
deploy it safely. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Deploying Flame safely

Flame runs user code, so the deployment decides most of what an attacker can
do.

- **Enable TLS and client authentication.** The session manager frontend,
  the executor manager and the object cache support TLS, and the external
  access of the Helm chart authenticates clients with mutual TLS
  ([RFE234 TLS](designs/RFE234-tls/FS.md),
  [RFE552 cache client mTLS](designs/RFE552-cache-client-mtls/FS.md)). A
  cluster without it trusts anyone who can reach its ports; keep those ports on
  a private network and restrict the source addresses of any load balancer.
- **Treat a client as trusted with the code it submits.** The Python SDK
  exchanges objects with `cloudpickle`, which runs code when it loads an
  object. Only connect clients and workers that you trust to a cluster, and do
  not load objects from an untrusted source.
- **Run untrusted code in a sandbox.** `flmbox` runs commands and code in
  [gVisor](https://gvisor.dev) sandboxes. A sandbox has no network by
  default (`network: none`); the built-in `flmbox` application
  (`config/applications/flmbox.yaml`) enables host networking, and the comment
  there says what that gives up, so decide it deliberately.
  The images a sandbox uses come from a registry that you choose.
- **Limit resources.** Executors run in their own cgroup, and sessions,
  applications and nodes have configurable limits.
- **Keep secrets out of the repository and the images.** Credentials are
  passed through the environment or the secret store of the platform, never
  committed.

## Secure design principles

The design of Flame follows these principles, and a change is reviewed against
them.

- **Least privilege.** Components only get what they need: sandboxes are
  confined by gVisor and a cgroup, a sandbox has no network unless asked,
  and a session is given only its own session token.
- **Fail-safe defaults.** The defaults are the restrictive choice: no sandbox
  network, an unknown shim or a malformed configuration is an error and not a
  fallback, and a request that cannot be validated is refused.
- **Complete mediation.** Every request to the session manager and the object
  cache goes through its API, where it is validated and, with TLS enabled,
  authenticated.
- **Defense in depth.** A sandbox sits inside a container or a cgroup, behind
  an authenticated endpoint, on a network that the operator restricts.
- **Economy of mechanism and open design.** The protocols are public
  (`rpc/protos`), the designs are published in `docs/designs`, and security
  does not depend on secrecy of the design.
- **Validate input and bound resources.** Data from outside is checked before
  it is used: the sizes, counts and formats of requests, objects, images and
  layers have limits.

## Kinds of errors that we guard against

The project is written in Rust and Python and reviewed for the common
classes of vulnerabilities (the OWASP Top 10 and the CWE Top 25):

- **Memory safety** (CWE-119, CWE-416): the crates of Flame contain no
  `unsafe` code (the only use in the repository is in an example).
- **Path traversal and unsafe extraction** (CWE-22, CWE-59): the image
  unpacker of `flmbox` refuses absolute paths and `..`, resolves directories
  without following a symlink out of the root, and requires hard links to point
  inside it; `flmctl deploy` rejects unsafe archive paths. The checks have
  tests.
- **Injection** (CWE-78, CWE-89): Flame starts its own helper programs
  (`runsc`, `containerd` clients) with an argument vector, not through a
  shell. A shell command that a user submits to a sandbox runs in the sandbox,
  which contains it.
- **Unsafe deserialization** (CWE-502): see the note on `cloudpickle` above;
  messages between components are protobuf.
- **Improper certificate validation** (CWE-295): TLS clients verify the server
  certificate and use the platform trust roots or the configured CA.
- **Resource exhaustion** (CWE-400): `flmbox` limits the layers, the size of an
  image and of its unpacked tree, and the output of a command; the object cache
  has memory and object-count limits (`cache.eviction`); sessions and nodes have
  configurable limits.
- **Hard-coded credentials and leaks** (CWE-798, CWE-200): the repository is
  scanned for secrets on every pull request.
- **Integrity of downloaded content** (CWE-494): `flmbox` verifies the digests
  of images and layers, and release artifacts are fetched over HTTPS from
  GitHub, PyPI, crates.io and Docker Hub.

## Cryptography

Flame does not implement cryptographic algorithms. TLS is provided by
[rustls](https://github.com/rustls/rustls) (with the `ring` provider) and
the platform's trust roots; digests are SHA-256 (the `sha2` crate); identifiers
and nonces come from the operating system's random number generator through
`rand` and `uuid`. Flame does not store user passwords.

## Static analysis and dependencies

Every pull request runs `cargo clippy -D warnings`, `cargo fmt --check`, `ruff`
and the test suites, and a secret scanner. Dependencies are pinned by
`Cargo.lock` and updated deliberately.

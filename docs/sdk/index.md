# Flame SDKs

Flame provides SDKs for building clients that submit work to Flame and services that run inside Flame executors.

| SDK | Use it for | Documentation |
|-----|------------|---------------|
| Rust SDK | Typed async clients, compiled services, high-throughput task submission, and object-cache workflows | [Rust SDK](rust.md) |
| Python SDK | Python services, scripts, agent and RL workflows, and dynamic packaging through App | [Python SDK](python.md) |

Both SDKs use the same Flame concepts:

- An application is the service registered in Flame.
- A session is a group of tasks for one application.
- A task is one unit of work inside a session.
- Common data is shared session input sent to every service instance.
- Object cache stores larger shared values, files, packages, and versioned objects.

## Configuration

Both SDKs read Flame configuration from `~/.flame/flame.yaml` and accept
environment settings from the Flame runtime:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://127.0.0.1:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
```

Common environment settings:

- `FLAME_ENDPOINT`
- `FLAME_CACHE_ENDPOINT`
- `FLAME_CA_FILE`
- `FLAME_CERT_FILE`
- `FLAME_KEY_FILE`

Use `https://` for the session-manager endpoint when TLS is enabled. Use `grpcs://` for the object-cache endpoint when cache TLS is enabled.
For a client certificate, configure its matching private key under the
relevant `cluster.tls` or `cache.tls` section, using
[file or embedded credentials](#portable-tls-configuration). An external cache proxy uses
`grpc-proxy://host:port` for plaintext HTTP/2 or `grpcs-proxy://host:port`
for TLS. Both require an explicit port and send the owning cache address in
`x-flame-object-cache` metadata for reference requests. Gateways route these
requests to the owner named in the header and initial requests without the
header to the cache Service. With TLS, certificate verification uses the public
proxy host. The administrator must configure a gateway listener matching the
selected transport.

### Portable TLS Configuration

`flmctl`, the Rust SDK, and Flamepy's synchronous and asynchronous clients accept
the same TLS fields for cluster and cache connections:

| Credential | PEM file path | Base64-encoded PEM |
|------------|---------------|--------------------|
| Server trust bundle | `ca_file` | `ca_data` |
| Client certificate chain | `cert_file` | `cert_data` |
| Client private key | `key_file` | `key_data` |

Embed the credentials to carry a single `~/.flame/flame.yaml` between machines:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: https://flame.example.com:8443
      tls: &client_tls
        ca_data: <base64-encoded-ca-pem>
        cert_data: <base64-encoded-client-certificate-pem>
        key_data: <base64-encoded-client-private-key-pem>
    cache:
      endpoint: grpcs-proxy://flame.example.com:9443
      tls: *client_tls
```

Replace each placeholder with a single-line standard base64 string, including
padding. For example, encode the CA PEM using Python's standard library:

```bash
python3 -c 'import base64, pathlib; print(base64.b64encode(pathlib.Path("ca.crt").read_bytes()).decode("ascii"))'
```

Encode the client certificate and its unencrypted private key the same way.
Base64-encoded private keys remain sensitive; restrict access to the resulting
configuration, for example with `chmod 600 ~/.flame/flame.yaml`.

Choose at most one source for each credential: setting both `ca_file` and
`ca_data`, for example, is an error. File and embedded sources can be mixed
across credentials, such as `cert_data` with `key_file`. A client identity
requires both a certificate and a matching private key. For CA-only TLS, omit
both identity credentials. With neither CA field, the TLS backend uses its
default trust roots. Server certificate and hostname verification remain
enabled for every TLS connection.

Both SDKs decode and validate embedded PEM in memory. Empty values, malformed
base64 or PEM, incomplete identities, and mismatched certificate/key pairs are
errors. Existing file-based configurations continue to work. The
`FLAME_CA_FILE`, `FLAME_CERT_FILE`, and `FLAME_KEY_FILE` environment variables
fill only credentials with neither source configured in the YAML.

## Typical Workflow

1. Start a Flame cluster with Docker Compose or the install scripts (`scripts/README.md`).
2. Deploy or register an application service.
3. Create a session for that application from a client.
4. Run tasks with `run()` or submit them with `submit()`.
5. Close the session when no more work will be submitted.

The SDKs wrap the gRPC APIs documented in [Flame API Reference](../api/index.md). Use the SDK pages for common application code and the API reference when implementing lower-level protocol integrations.

## Examples

- [Rust Pi example](../../examples/pi/rust/README.md)
- [Rust API tutorial](../tutorials/rust-api.md)
- [Rust Candle Based example](../../examples/candle/based/README.md)
- [Python Pi App example](../../examples/pi/python/README.md)
- [Python SDK API reference](../../sdk/python/docs/API.md)

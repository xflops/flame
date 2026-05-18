# Flame SDKs

Flame provides SDKs for building clients that submit work to Flame and services that run inside Flame executors.

| SDK | Use it for | Documentation |
|-----|------------|---------------|
| Rust SDK | Typed async clients, compiled services, high-throughput task submission, and object-cache workflows | [Rust SDK](rust.md) |
| Python SDK | Python services, scripts, agent and RL workflows, and dynamic packaging through Runner | [Python SDK](python.md) |

Both SDKs use the same Flame concepts:

- An application is the service registered in Flame.
- A session is a group of tasks for one application.
- A task is one unit of work inside a session.
- Common data is shared session input sent to every service instance.
- Object cache stores larger shared values, files, packages, and versioned objects.

## Configuration

Both SDKs read Flame configuration from `~/.flame/flame.yaml` and accept environment overrides from the Flame runtime:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "http://127.0.0.1:8080"
    cache:
      endpoint: "grpc://127.0.0.1:9090"
```

Common environment overrides:

- `FLAME_ENDPOINT`
- `FLAME_CACHE_ENDPOINT`
- `FLAME_CA_FILE`

Use `https://` for the session-manager endpoint when TLS is enabled. Use `grpcs://` for the object-cache endpoint when cache TLS is enabled.

## Typical Workflow

1. Start a Flame cluster with Docker Compose or `flmadm`.
2. Deploy or register an application service.
3. Create a session for that application from a client.
4. Submit tasks with `invoke()` or `run()`.
5. Close the session when no more work will be submitted.

The SDKs wrap the gRPC APIs documented in [Flame API Reference](../api/index.md). Use the SDK pages for common application code and the API reference when implementing lower-level protocol integrations.

## Examples

- [Rust Pi example](../../examples/pi/rust/README.md)
- [Rust API tutorial](../tutorials/rust-api.md)
- [Rust Candle Based example](../../examples/candle/based/README.md)
- [Python Pi Runner example](../../examples/pi/python/README.md)
- [Python SDK API reference](../../sdk/python/docs/API.md)

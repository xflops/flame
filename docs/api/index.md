# Flame API Reference

This document provides the API reference for Flame's gRPC services.

## Services Overview

Flame exposes three gRPC services:

| Service | Description | Proto File |
|---------|-------------|------------|
| [Frontend](frontend.md) | Client-facing API for sessions, tasks, and applications | `frontend.proto` |
| [Backend](backend.md) | Executor-facing API for node and executor management | `backend.proto` |
| [Instance](shim.md) | Application instance lifecycle management | `shim.proto` |

## Quick Links

- [Type Definitions](types.md) - Common message types used across all services
- [Frontend Service](frontend.md) - For client SDK developers
- [Backend Service](backend.md) - For executor/node developers
- [Instance Service](shim.md) - For application shim developers
- [SDK Guides](../sdk/index.md) - For application developers using Rust or Python

## Package

All services and messages are defined in the `flame.v1` package.

```protobuf
package flame.v1;
```

## Authentication

Flame supports TLS for secure communication. Configure TLS certificates in your client configuration:

```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "https://flame-session-manager:8080"
      tls:
        ca_file: "/etc/flame/certs/ca.crt"
    cache:
      endpoint: "grpcs://flame-object-cache:9090"
      tls:
        ca_file: "/etc/flame/certs/ca.crt"
```

## Error Handling

gRPC status codes are used for error reporting:

| Code | Description |
|------|-------------|
| `OK` | Operation succeeded |
| `NOT_FOUND` | Resource not found |
| `ALREADY_EXISTS` | Resource already exists |
| `INVALID_ARGUMENT` | Invalid request parameters |
| `FAILED_PRECONDITION` | Operation not allowed in current state |
| `INTERNAL` | Internal server error |

## Related Documentation

- [Flame README](../../README.md)
- [Rust SDK](../sdk/rust.md)
- [Python SDK](../sdk/python.md)
- [Local Development](../tutorials/local-development.md)
- [Runner Setup Guide](../tutorials/runner-setup.md)
- [Python SDK README](../../sdk/python/README.md)

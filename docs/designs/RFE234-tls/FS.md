---
Issue: #234
Author: Flame Team
Date: 2026-03-28
---

# Design Document: Enable TLS for All Components

## 1. Motivation

**Background:**

Currently, all connections between Flame components are insecure (plaintext HTTP/gRPC). This includes:

1. **Client SDK ↔ Session Manager**: Frontend API for session/task management
2. **Executor Manager ↔ Session Manager**: Backend API for executor registration and task execution
3. **Client/Executor ↔ Object Cache**: Arrow Flight protocol for object storage

In production environments, especially when Flame is deployed across network boundaries or in multi-tenant clusters, unencrypted traffic poses security risks:

- Data in transit can be intercepted (session data, task inputs/outputs)
- Man-in-the-middle attacks are possible
- Compliance requirements (SOC2, HIPAA, etc.) often mandate encryption

**Target:**

Enable TLS encryption for all gRPC connections in Flame to ensure:

1. **Encryption**: All data in transit is encrypted
2. **Server Authentication**: Clients can verify they're connecting to legitimate Flame services
3. **Backward Compatibility**: Existing deployments can continue using plaintext connections
4. **Ease of Use**: Simple configuration with reasonable defaults

> **Note:** This design covers server-side TLS only. Mutual TLS (mTLS) for client certificate authentication is out of scope and will be addressed in a future issue. RBAC is also tracked separately.

## 2. Function Specification

**Configuration:**

TLS is configured via the `cluster.tls` section in `flame-cluster.yaml`. When the `tls` section is absent, Flame operates in plaintext mode (current behavior).

The Session Manager and Object Cache can have **separate TLS configurations**, allowing:
- Different certificates for each service
- Independent TLS enablement (e.g., TLS for Session Manager only)
- Different CAs for different security zones

```yaml
# flame-cluster.yaml
cluster:
  name: flame
  endpoint: "https://flame-session-manager:8080"  # Use https:// for TLS
  resreq: "cpu=1,mem=2g"
  policy: priority
  storage: sqlite://flame.db
  
  # Executors configuration (moved under cluster)
  executors:
    shim: host
    limits:
      max_executors: 128
  
  # TLS Configuration for Session Manager (optional - omit for plaintext)
  tls:
    # Server certificate and private key (required when tls section present)
    cert_file: "/etc/flame/certs/session-manager.crt"
    key_file: "/etc/flame/certs/session-manager.key"
    
    # CA certificate for client verification (optional, for future mTLS)
    ca_file: "/etc/flame/certs/ca.crt"

cache:
  endpoint: "grpcs://127.0.0.1:9090"  # Use grpcs:// for TLS
  network_interface: "eth0"
  storage: "/var/lib/flame/cache"
  
  # TLS Configuration for Object Cache (optional - independent from Session Manager)
  # Required if endpoint uses grpcs://
  tls:
    cert_file: "/etc/flame/certs/object-cache.crt"
    key_file: "/etc/flame/certs/object-cache.key"
    ca_file: "/etc/flame/certs/ca.crt"
```

**Configuration Options:**

*Cluster Configuration (`cluster.*`):*

| Option                            | Type   | Required | Description                                              |
| --------------------------------- | ------ | -------- | -------------------------------------------------------- |
| `cluster.name`                    | string | Yes      | Cluster name                                             |
| `cluster.endpoint`                | string | Yes      | Session Manager endpoint URL                             |
| `cluster.resreq`                  | string | No       | Default per-session resreq (e.g., "cpu=1,mem=2g")        |
| `cluster.policy`                  | string | No       | Scheduling policy                                        |
| `cluster.storage`                 | string | No       | Storage backend URL                                      |
| `cluster.executors.shim`          | string | No       | Shim type: "host" or "wasm" (default: "host")            |
| `cluster.executors.limits.max_executors` | int | No  | Max executors per node (default: 128)                    |
| `cluster.tls.cert_file`           | string | Yes*     | Path to PEM-encoded server certificate                   |
| `cluster.tls.key_file`            | string | Yes*     | Path to PEM-encoded private key                          |
| `cluster.tls.ca_file`             | string | No       | Path to PEM-encoded CA certificate for chain validation  |

\* Required when `cluster.tls` section is present

*Object Cache TLS (`cache.tls.*`):*

| Option                | Type   | Required | Description                                              |
| --------------------- | ------ | -------- | -------------------------------------------------------- |
| `cache.tls.cert_file` | string | Yes*     | Path to PEM-encoded server certificate for cache         |
| `cache.tls.key_file`  | string | Yes*     | Path to PEM-encoded private key for cache                |
| `cache.tls.ca_file`   | string | No       | Path to PEM-encoded CA certificate for cache             |

\* Required when `cache.tls` section is present

*TLS Configuration (No Inheritance):*

> **Note:** TLS configuration does **not** inherit from `cluster.tls` to `cache.tls`. Each must be configured independently.

| `cluster.tls` | `cache.tls` | `cache.endpoint` scheme | Cache TLS Behavior |
| ------------- | ----------- | ----------------------- | ------------------ |
| Present       | Absent      | `grpcs://`              | ❌ Error: cache.tls required |
| Present       | Present     | `grpcs://`              | Uses `cache.tls` |
| Present       | Absent      | `grpc://`               | No TLS for cache |
| Absent        | Present     | `grpcs://`              | Uses `cache.tls` |
| Absent        | Absent      | `grpc://`               | No TLS |
| Absent        | Absent      | `grpcs://`              | ❌ Error: TLS required but not configured |

**Protocol Scheme Conventions:**

| Scheme     | Protocol     | TLS |
| ---------- | ------------ | --- |
| `http://`  | gRPC         | No  |
| `https://` | gRPC         | Yes |
| `grpc://`  | Arrow Flight | No  |
| `grpcs://` | Arrow Flight | Yes |

**Client Configuration:**

Clients (SDK, CLI tools) configure TLS via their connection settings. Client-side config uses a context-based structure:

```yaml
# Client configuration (~/.flame/flame.yaml)
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "https://flame-session-manager:8080"
      tls:
        # CA certificate for server verification (optional)
        # If not specified, system CA bundle is used
        ca_file: "/etc/flame/certs/ca.crt"
    cache:
      endpoint: "grpcs://flame-object-cache:9090"
      tls:
        # Separate CA for cache if using different certificate chain
        ca_file: "/etc/flame/certs/cache-ca.crt"
```

> **Note:** To disable TLS for development, use `http://` instead of `https://` in the endpoint URL.

```rust
// sdk/rust/src/apis/ctx.rs

/// Client TLS configuration for connecting to Flame services.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlameClientTls {
    /// Path to CA certificate for server verification
    #[serde(default)]
    pub ca_file: Option<String>,
}

impl FlameClientTls {
    /// Load client TLS config for tonic.
    ///
    /// If ca_file is specified, use it; otherwise use system CA bundle.
    /// The domain parameter is used for server name verification.
    pub fn client_tls_config(&self, domain: &str) -> Result<ClientTlsConfig, FlameError> {
        let mut config = ClientTlsConfig::new().domain_name(domain);
        
        if let Some(ref ca_file) = self.ca_file {
            let ca = std::fs::read_to_string(ca_file)?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }
        
        Ok(config)
    }
}
```

**API:**

No changes to the gRPC API definitions (protobuf files). TLS is a transport-layer concern handled by tonic's configuration.

**CLI:**

*`flmctl` changes:*

TLS configuration is read from the config file (`~/.flame/flame.yaml`). No CLI flags needed.

```bash
# Connect with TLS (automatic when endpoint uses https:// and tls config present)
flmctl --config ~/.flame/flame.yaml list session
```

**Environment Variables:**

> **Note:** Environment variable overrides for TLS configuration are not currently implemented. TLS settings must be configured via the YAML configuration files.

| Variable              | Description                                              | Status      |
| --------------------- | -------------------------------------------------------- | ----------- |
| `FLAME_TLS_CERT_FILE` | Override `tls.cert_file`                                 | Not implemented |
| `FLAME_TLS_KEY_FILE`  | Override `tls.key_file`                                  | Not implemented |
| `FLAME_TLS_CA_FILE`   | Override `tls.ca_file`                                   | Not implemented |

**Certificate Trust Model:**

This design uses **server-only TLS** where the server presents a certificate and clients verify it. Understanding the trust model is critical for proper deployment.

*How Server-Only TLS Works:*

```
┌──────────┐                              ┌──────────────────┐
│  Client  │                              │  Session Manager │
│          │  1. Client connects          │                  │
│          │ ───────────────────────────► |                  │
│          │                              │                  │
│          │  2. Server sends certificate │                  │
│          │ ◄─────────────────────────── |  server.crt      │
│          │                              │  (signed by CA)  │
│ ca.crt   │  3. Client verifies cert     │                  │
│          │     using CA certificate     │                  │
│          │                              │                  │
│          │  4. Encrypted connection     │                  │
│          │ ◄──────────────────────────► |                  │
└──────────┘                              └──────────────────┘
```

*Trust Requirement:* **All clients must trust the CA that signed the server's certificate.**

*Common Scenarios:*

| Scenario              | Server Cert Signed By | Client CA Config            | Result                         |
| --------------------- | --------------------- | --------------------------- | ------------------------------ |
| Same CA               | Private CA-A          | `ca_file: ca-A.crt`         | ✅ Works                        |
| Different CA          | Private CA-A          | `ca_file: ca-B.crt`         | ❌ Fails - cannot verify server |
| Public CA             | Let's Encrypt         | (none - uses system bundle) | ✅ Works                        |
| Private CA, no config | Private CA-A          | (none - uses system bundle) | ❌ Fails - CA-A not trusted     |

*Deployment Recommendations:*

1. **Single Private CA (Recommended for Bare-Metal)**:
   - Generate one CA for the entire Flame cluster
   - Sign all server certificates with this CA
   - Distribute `ca.crt` to all clients (executor managers, SDK users, CLI users)
   - Simple, consistent, and secure

   ```
   Flame Cluster CA (ca.crt, ca.key)
        │
        ├── signs → session-manager.crt
        ├── signs → object-cache.crt (if separate)
        │
        └── distributed to:
              ├── All Executor Manager nodes
              ├── All SDK/CLI client machines
              └── Any system connecting to Flame
   ```

2. **Public CA (For Internet-Facing Deployments)**:
   - Use Let's Encrypt or commercial CA
   - Requires public DNS name for the Session Manager
   - Clients use system CA bundle (no `ca_file` needed)
   - Simpler client config but more server setup

3. **Certificate Chain (For Complex PKI)**:
   - Root CA → Intermediate CA → Server Cert
   - Server presents full chain (`server.crt` includes intermediate)
   - Clients only need root CA
   - Standard enterprise PKI pattern

*What Happens with Mismatched CAs:*

```
Error: certificate verify failed
  - The server's certificate was signed by CA-A
  - But the client only trusts CA-B
  - Client cannot establish trust chain
```

*Troubleshooting:*

```bash
# Verify server certificate
openssl s_client -connect session-manager:8080 -CAfile /etc/flame/certs/ca.crt

# Check certificate details
openssl x509 -in /etc/flame/certs/server.crt -text -noout | grep -A1 "Issuer"

# Verify CA can validate server cert
openssl verify -CAfile /etc/flame/certs/ca.crt /etc/flame/certs/server.crt
```

**Scope:**

*In Scope:*
- Server-side TLS for Session Manager (Frontend and Backend APIs)
- Server-side TLS for Object Cache (Arrow Flight)
- Client-side TLS verification for Executor Manager
- Client-side TLS verification for SDK (Rust, Python)
- CLI tool TLS support (`flmctl`)
- Configuration schema and validation

*Out of Scope:*
- Mutual TLS (mTLS) / client certificate authentication
- Certificate rotation (manual restart required)
- RBAC / authorization
- Automatic certificate management (cert-manager integration)
- TLS for internal shim communication (Host/Wasm shim to application)

*Limitations:*
- Certificate rotation requires service restart
- No automatic certificate renewal
- Self-signed certificates require manual CA distribution to all nodes

**Feature Interaction:**

*Related Features:*

| Feature           | Interaction                                       |
| ----------------- | ------------------------------------------------- |
| Session Manager   | Serves TLS-encrypted Frontend/Backend APIs        |
| Executor Manager  | Connects to Session Manager with TLS verification |
| Object Cache      | Serves TLS-encrypted Arrow Flight API             |
| SDK (Rust/Python) | Connects with TLS verification                    |
| CLI (flmctl)      | Connects with TLS verification                    |
| Docker Compose    | Mount certificates as volumes                     |

*Updates Required:*

| Component        | File                                    | Change                                                     |
| ---------------- | --------------------------------------- | ---------------------------------------------------------- |
| Common           | `common/src/ctx.rs`                     | Add `FlameTls` struct, move `executors` under `cluster`    |
| Session Manager  | `session_manager/src/apiserver/mod.rs`  | Configure tonic server with TLS                            |
| Executor Manager | `executor_manager/src/client.rs`        | Configure tonic client with TLS                            |
| Object Cache     | `object_cache/src/cache.rs`             | Configure Arrow Flight server with TLS                     |
| SDK Rust         | `sdk/rust/src/client/mod.rs`            | Configure client with TLS via `connect_with_tls()`         |
| SDK Rust         | `sdk/rust/src/apis/ctx.rs`              | Add `FlameClientTls` and `FlameContext` config structs     |
| SDK Python       | `sdk/python/src/flamepy/core/client.py` | Configure grpcio with TLS                                  |
| CLI              | `flmctl/src/*.rs`                       | Use TLS config from flame.yaml                             |
| Installer        | `installer/flame-cluster.yaml`          | Update example config with new structure                   |
| CI               | `ci/flame-cluster.yaml`                 | Update CI config with new structure                        |
| CI               | `ci/generate-certs.sh`                  | Shell script to generate TLS certificates                  |

*Breaking Changes (Alpha Release):*

Since Flame is in alpha release, backward compatibility is not required. The following breaking changes are made directly:

- **Config Structure Change**: `executors` section moved from top-level to `cluster.executors`
- **Config Structure Change**: `tls` section added under `cluster.tls` (not top-level)
- **Existing configs will fail** to parse until updated to new structure

**Migration for Existing Users:**

```yaml
# BEFORE (old structure)
cluster:
  name: flame
  endpoint: "http://localhost:8080"
executors:
  shim: host
  limits:
    max_executors: 128

# AFTER (new structure)
cluster:
  name: flame
  endpoint: "http://localhost:8080"
  executors:
    shim: host
    limits:
      max_executors: 128
```

## 3. Implementation Detail

**Architecture:**

```
┌─────────────┐         TLS          ┌──────────────────┐
│   Client    │◄────────────────────►│  Session Manager │
│  (SDK/CLI)  │      Frontend API    │                  │
└─────────────┘      (port 8080)     │  ┌────────────┐  │
                                     │  │  Frontend  │  │
                                     │  │   Server   │  │
┌─────────────┐         TLS          │  └────────────┘  │
│  Executor   │◄────────────────────►│                  │
│   Manager   │      Backend API     │  ┌────────────┐  │
│             │      (port 8081)     │  │  Backend   │  │
│ ┌─────────┐ │                      │  │   Server   │  │
│ │ Object  │ │         TLS          │  └────────────┘  │
│ │ Cache   │◄┼──────────────────────┼──────────────────┤
│ └─────────┘ │    Arrow Flight      └──────────────────┘
│  (port 9090)│
└─────────────┘
```

**Components:**

1. **FlameTls (`common/src/ctx.rs`)**
   - Reusable TLS configuration struct (cert_file, key_file, ca_file)
   - Provides methods to load certificates and create tonic TLS configs
   - Validates certificate paths and formats on load
   - Used by both Session Manager and Object Cache

2. **Session Manager TLS Server**
   - Configures `tonic::transport::Server` with `ServerTlsConfig`
   - Loads certificate and key from top-level `tls` config
   - Serves both Frontend and Backend on TLS when configured

3. **Executor Manager TLS Client**
   - Configures `tonic::transport::Channel` with `ClientTlsConfig`
   - Loads CA certificate for server verification
   - Connects to Session Manager Backend API

4. **Object Cache TLS Server**
   - Configures Arrow Flight server with TLS
   - Uses `cache.tls` config if present, otherwise falls back to top-level `tls`
   - Can have completely independent certificate from Session Manager

5. **SDK TLS Client**
   - Rust: Uses tonic's `ClientTlsConfig`
   - Python: Uses grpcio's SSL credentials

**Data Structures:**

Following Flame's existing pattern of YAML structs with `TryFrom` conversion to runtime structs:

```rust
// common/src/ctx.rs

// ============================================================
// YAML deserialization structs (serde layer)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameTlsYaml {
    /// Path to PEM-encoded server certificate
    pub cert_file: Option<String>,
    /// Path to PEM-encoded private key  
    pub key_file: Option<String>,
    /// Path to PEM-encoded CA certificate (for certificate chain validation)
    pub ca_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameExecutorsYaml {
    pub shim: Option<String>,
    pub limits: Option<FlameExecutorLimitsYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameClusterYaml {
    pub name: String,
    pub endpoint: String,
    pub resreq: Option<String>,
    pub policy: Option<String>,
    pub storage: Option<String>,
    pub schedule_interval: Option<u64>,
    pub executors: Option<FlameExecutorsYaml>,  // NEW: moved under cluster
    pub tls: Option<FlameTlsYaml>,              // NEW: TLS for Session Manager
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameCacheYaml {
    pub endpoint: Option<String>,
    pub network_interface: Option<String>,
    pub storage: Option<String>,
    pub eviction: Option<FlameEvictionYaml>,
    pub tls: Option<FlameTlsYaml>,  // Separate TLS config for cache
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlameClusterContextYaml {
    pub cluster: FlameClusterYaml,
    pub cache: Option<FlameCacheYaml>,
    // Note: executors and tls are now under cluster, not top-level
}

// ============================================================
// Runtime structs (validated, with helper methods)
// ============================================================

/// TLS configuration for Flame services.
/// 
/// When this struct is present and valid (cert_file + key_file configured),
/// TLS is enabled for the service.
#[derive(Debug, Clone, Default)]
pub struct FlameTls {
    /// Path to PEM-encoded server certificate
    pub cert_file: String,
    /// Path to PEM-encoded private key
    pub key_file: String,
    /// Path to PEM-encoded CA certificate (optional)
    pub ca_file: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FlameExecutors {
    pub shim: Shim,
    pub limits: FlameExecutorLimits,
}

#[derive(Debug, Clone)]
pub struct FlameCluster {
    pub name: String,
    pub endpoint: String,
    pub resreq: Option<ResourceRequirement>,
    pub policy: String,
    pub storage: String,
    pub schedule_interval: u64,
    pub executors: FlameExecutors,      // NEW: moved under cluster
    pub tls: Option<FlameTls>,          // NEW: TLS for Session Manager
}

#[derive(Debug, Clone, Default)]
pub struct FlameCache {
    pub endpoint: String,
    pub network_interface: String,
    pub storage: Option<String>,
    pub eviction: FlameEviction,
    pub tls: Option<FlameTls>,  // Separate TLS config for cache
}

/// Updated FlameClusterContext - simplified top-level structure
#[derive(Debug, Clone, Default)]
pub struct FlameClusterContext {
    pub cluster: FlameCluster,
    pub cache: Option<FlameCache>,
    // Note: executors and tls are accessed via cluster.executors and cluster.tls
}

impl FlameCache {
    /// Check if cache endpoint requires TLS (grpcs:// scheme)
    pub fn requires_tls(&self) -> bool {
        self.endpoint.starts_with("grpcs://")
    }
}

impl FlameCluster {
    /// Check if cluster endpoint requires TLS (https:// scheme)
    pub fn requires_tls(&self) -> bool {
        self.endpoint.starts_with("https://")
    }
}

impl FlameTls {
    /// Load server TLS config for tonic.
    /// Returns ServerTlsConfig with identity loaded from cert/key files.
    pub fn server_tls_config(&self) -> Result<ServerTlsConfig, FlameError> {
        let cert = std::fs::read_to_string(&self.cert_file)
            .map_err(|e| FlameError::InvalidConfig(
                format!("failed to read cert_file <{}>: {}", self.cert_file, e)
            ))?;
        let key = std::fs::read_to_string(&self.key_file)
            .map_err(|e| FlameError::InvalidConfig(
                format!("failed to read key_file <{}>: {}", self.key_file, e)
            ))?;
        
        let identity = Identity::from_pem(cert, key);
        let config = ServerTlsConfig::new().identity(identity);
        
        // Note: ca_file is reserved for future mTLS support
        // Currently not used for server-only TLS
        
        Ok(config)
    }

    /// Load client TLS config for tonic.
    /// If ca_file is specified, use it; otherwise use system CA bundle.
    pub fn client_tls_config(&self) -> Result<ClientTlsConfig, FlameError> {
        let mut config = ClientTlsConfig::new();
        
        if let Some(ca_file) = &self.ca_file {
            let ca = std::fs::read_to_string(ca_file)
                .map_err(|e| FlameError::InvalidConfig(
                    format!("failed to read ca_file <{}>: {}", ca_file, e)
                ))?;
            config = config.ca_certificate(Certificate::from_pem(ca));
        }
        
        Ok(config)
    }
}

impl TryFrom<FlameTlsYaml> for FlameTls {
    type Error = FlameError;
    
    fn try_from(yaml: FlameTlsYaml) -> Result<Self, Self::Error> {
        let cert_file = yaml.cert_file.ok_or_else(|| 
            FlameError::InvalidConfig("tls.cert_file is required".to_string())
        )?;
        let key_file = yaml.key_file.ok_or_else(|| 
            FlameError::InvalidConfig("tls.key_file is required".to_string())
        )?;
        
        // Note: File existence is validated when loading certificates in server_tls_config()
        // and client_tls_config() methods, which provide more descriptive error messages.
        
        Ok(FlameTls {
            cert_file,
            key_file,
            ca_file: yaml.ca_file,
        })
    }
}
```

**Key Code Changes:**

*Session Manager Server (`session_manager/src/apiserver/mod.rs`):*

```rust
// Before
Server::builder()
    .tcp_keepalive(Some(Duration::from_secs(1)))
    .add_service(FrontendServer::new(frontend_service))
    .serve(address)
    .await?;

// After
let mut builder = Server::builder()
    .tcp_keepalive(Some(Duration::from_secs(1)));

// Apply TLS if configured
if let Some(ref tls_config) = ctx.cluster.tls {
    let tls = tls_config.server_tls_config()?;
    builder = builder
        .tls_config(tls)
        .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;
    tracing::info!("TLS enabled for frontend apiserver");
}

builder
    .add_service(FrontendServer::new(frontend_service))
    .serve(address)
    .await?;
```

*Executor Manager Client (`executor_manager/src/client.rs`):*

```rust
// Before
let channel = Channel::from_shared(endpoint.clone())?
    .connect()
    .await?;

// After
let mut channel_builder = Channel::from_shared(endpoint.clone())?;

// Apply TLS if endpoint uses https://
if endpoint.starts_with("https://") {
    let tls_config = if let Some(ref tls) = ctx.cluster.tls {
        tls.client_tls_config()?
    } else {
        // Use default TLS config (system CA bundle)
        tonic::transport::ClientTlsConfig::new()
    };
    channel_builder = channel_builder
        .tls_config(tls_config)
        .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;
    tracing::info!("TLS enabled for backend client");
}

let channel = channel_builder.connect().await?;
```

*Object Cache Server (`object_cache/src/cache.rs`):*

```rust
// The run() function takes cache_config: &FlameCache directly
let mut builder = tonic::transport::Server::builder();

// Apply TLS if cache endpoint requires it (grpcs:// scheme) and TLS is configured
if cache_config.requires_tls() {
    let tls_config = cache_config.tls.as_ref().ok_or_else(|| {
        FlameError::InvalidConfig(
            "cache endpoint uses grpcs:// but cache.tls is not configured".to_string(),
        )
    })?;

    let tls = tls_config.server_tls_config()?;
    builder = builder
        .tls_config(tls)
        .map_err(|e| FlameError::InvalidConfig(format!("TLS config error: {}", e)))?;

    tracing::info!("TLS enabled for object cache");
}

builder
    .add_service(FlightServiceServer::new(server))
    .serve(addr)
    .await?;
```

**Certificate Generation:**

Use standard OpenSSL commands to generate certificates. A helper script is provided at `ci/generate-certs.sh`:

```bash
#!/bin/bash
# ci/generate-certs.sh - Generate self-signed TLS certificates
# Usage: ./generate-certs.sh [output_dir] [san_list]

OUTPUT_DIR="${1:-./certs}"
SAN_LIST="${2:-localhost,127.0.0.1}"

# Generate CA
openssl genrsa -out "$OUTPUT_DIR/ca.key" 4096
openssl req -new -x509 -days 365 -key "$OUTPUT_DIR/ca.key" \
    -out "$OUTPUT_DIR/ca.crt" -subj "/CN=Flame CA/O=Flame"

# Generate server key and CSR
openssl genrsa -out "$OUTPUT_DIR/server.key" 4096
openssl req -new -key "$OUTPUT_DIR/server.key" \
    -out "$OUTPUT_DIR/server.csr" -subj "/CN=flame-server/O=Flame"

# Sign server certificate with CA (includes SANs)
openssl x509 -req -in "$OUTPUT_DIR/server.csr" \
    -CA "$OUTPUT_DIR/ca.crt" -CAkey "$OUTPUT_DIR/ca.key" \
    -CAcreateserial -out "$OUTPUT_DIR/server.crt" -days 365 \
    -extfile <(echo "subjectAltName=DNS:localhost,IP:127.0.0.1")
```

**System Considerations:**

*Performance:*
- TLS handshake adds ~1-2ms latency on connection establishment
- Encryption/decryption overhead is negligible for modern CPUs with AES-NI
- Connection reuse (keep-alive) amortizes handshake cost
- No measurable impact on task throughput

*Scalability:*
- TLS does not affect horizontal scaling
- Each service instance needs access to certificates
- Certificate files must be distributed to all nodes (use shared storage or config management)

*Reliability:*
- Certificate expiry causes connection failures → monitor expiry
- Missing/invalid certificates fail fast at startup
- Graceful degradation not supported (TLS is all-or-nothing per endpoint)

*Security:*
- TLS 1.2+ enforced (tonic default)
- Strong cipher suites only (tonic default)
- Server authentication prevents MITM attacks
- Certificate chain validation when CA provided

*Observability:*
- Log TLS status at startup: "TLS enabled for apiserver"
- Log certificate expiry warnings: "Certificate expires in N days"
- Metrics: `flame_tls_enabled{component="session_manager"} 1`

*Operational:*
- Certificate rotation requires restart (no hot reload)
- Self-signed certs for development, CA-signed for production
- Bare-metal: copy certificate files to each node or use shared filesystem

**Dependencies:**

*External Dependencies:*
- `tonic` (0.12): Already used, has native TLS support via `tls` feature
- `tokio-rustls` (transitive): TLS implementation
- `openssl` (CLI tool): For certificate generation (not a Rust dependency)

*Cargo.toml changes:*
```toml
[dependencies]
tonic = { version = "0.12", features = ["tls"] }
```

## 4. Use Cases

**Example 1: Development with Self-Signed Certificates**

*Description:* Quick setup for local development with TLS

*Workflow:*

1. Generate self-signed certificates using the provided script:
```bash
ci/generate-certs.sh /tmp/flame-certs "localhost,127.0.0.1"
```

2. Configure Session Manager (`flame-cluster.yaml`):
```yaml
cluster:
  name: flame
  endpoint: "https://localhost:8080"
  tls:
    cert_file: "/tmp/flame-certs/server.crt"
    key_file: "/tmp/flame-certs/server.key"
```

3. Configure client (`~/.flame/flame.yaml`):
```yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "https://localhost:8080"
      tls:
        ca_file: "/tmp/flame-certs/ca.crt"
```

*Expected outcome:* Encrypted connections with self-signed certificates

**Example 2: Production Bare-Metal Deployment**

*Description:* Multi-node production deployment with CA-signed certificates

*Workflow:*

1. Obtain certificates from corporate CA or use OpenSSL to create a private CA:
```bash
# Create CA (once)
openssl genrsa -out ca.key 4096
openssl req -new -x509 -days 3650 -key ca.key -out ca.crt \
  -subj "/CN=Flame CA/O=MyOrg"

# Create server certificate
openssl genrsa -out server.key 2048
openssl req -new -key server.key -out server.csr \
  -subj "/CN=flame-session-manager/O=MyOrg"

# Sign with CA (include SANs for all hostnames/IPs)
cat > server.ext << EOF
authorityKeyIdentifier=keyid,issuer
basicConstraints=CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = flame-session-manager
DNS.2 = session-manager.example.com
IP.1 = 192.168.1.100
EOF

openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out server.crt -days 365 -extfile server.ext
```

2. Distribute certificates to all nodes:
```bash
# On each node
sudo mkdir -p /etc/flame/certs
sudo cp ca.crt server.crt server.key /etc/flame/certs/
sudo chmod 600 /etc/flame/certs/server.key
sudo chown flame:flame /etc/flame/certs/*
```

3. Configure Session Manager (`/etc/flame/flame-cluster.yaml`):
```yaml
cluster:
  name: flame
  endpoint: "https://session-manager.example.com:8080"
  storage: sqlite:///var/lib/flame/flame.db
  tls:
    cert_file: "/etc/flame/certs/server.crt"
    key_file: "/etc/flame/certs/server.key"
```

4. Configure Executor Manager on worker nodes:
```yaml
cluster:
  name: flame
  endpoint: "https://session-manager.example.com:8080"
  tls:
    ca_file: "/etc/flame/certs/ca.crt"
```

5. Configure client machines:
```yaml
# ~/.flame/flame.yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "https://session-manager.example.com:8080"
      tls:
        ca_file: "/etc/flame/certs/ca.crt"
```

*Expected outcome:* Production-grade TLS with proper certificate chain validation across all nodes

**Example 3: Multi-Node Bare-Metal with Systemd**

*Description:* TLS-enabled Flame cluster using systemd services

*Workflow:*

1. Generate certificates using OpenSSL or the provided script:
```bash
ci/generate-certs.sh /etc/flame/certs "localhost,192.168.1.100,session-manager.local"
```

2. Install and configure Session Manager on the head node:
```bash
# Install
sudo flmadm install --enable

# Edit config
sudo vim /etc/flame/flame-cluster.yaml
```

```yaml
# /etc/flame/flame-cluster.yaml (head node)
cluster:
  name: flame
  endpoint: "https://192.168.1.100:8080"
  storage: sqlite:///var/lib/flame/flame.db
  tls:
    cert_file: "/etc/flame/certs/server.crt"
    key_file: "/etc/flame/certs/server.key"
  executors:
    shim: host
```

3. Copy CA certificate to worker nodes:
```bash
# On head node
for worker in 192.168.1.{101..110}; do
  scp /etc/flame/certs/ca.crt $worker:/etc/flame/certs/
done
```

4. Configure Executor Manager on worker nodes:
```yaml
# /etc/flame/flame-cluster.yaml (worker nodes)
cluster:
  name: flame
  endpoint: "https://192.168.1.100:8080"
  tls:
    ca_file: "/etc/flame/certs/ca.crt"
  executors:
    shim: host
    limits:
      max_executors: 64
```

5. Start services:
```bash
# Head node
sudo systemctl start flame-session-manager

# Worker nodes
sudo systemctl start flame-executor-manager
```

*Expected outcome:* Encrypted communication across all nodes managed by systemd

**Example 4: Separate Certificates for Session Manager and Object Cache**

*Description:* Different certificates for different services (e.g., different security zones)

*Use case:* Object Cache runs on a separate network segment with its own certificate authority.

*Workflow:*

1. Generate separate certificates for each service:
```bash
# Session Manager certificate (for management network)
ci/generate-certs.sh /etc/flame/certs/session-manager "192.168.1.100,session-manager.mgmt.local"

# Object Cache certificate (for data network)  
ci/generate-certs.sh /etc/flame/certs/object-cache "10.0.0.100,object-cache.data.local"
```

2. Configure with separate TLS for each service:
```yaml
# /etc/flame/flame-cluster.yaml
cluster:
  name: flame
  endpoint: "https://192.168.1.100:8080"
  storage: sqlite:///var/lib/flame/flame.db
  # TLS for Session Manager (Frontend + Backend APIs)
  tls:
    cert_file: "/etc/flame/certs/session-manager/server.crt"
    key_file: "/etc/flame/certs/session-manager/server.key"

cache:
  endpoint: "grpcs://10.0.0.100:9090"
  network_interface: "eth1"  # Data network interface
  storage: "/var/lib/flame/cache"
  
  # Separate TLS for Object Cache
  tls:
    cert_file: "/etc/flame/certs/object-cache/server.crt"
    key_file: "/etc/flame/certs/object-cache/server.key"

  executors:
    shim: host
```

3. Configure clients with both CA certificates:
```yaml
# ~/.flame/flame.yaml
current-context: flame
contexts:
  - name: flame
    cluster:
      endpoint: "https://192.168.1.100:8080"
      tls:
        ca_file: "/etc/flame/certs/session-manager/ca.crt"
    cache:
      endpoint: "grpcs://10.0.0.100:9090"
      tls:
        ca_file: "/etc/flame/certs/object-cache/ca.crt"
```

*Expected outcome:* Session Manager and Object Cache use independent certificate chains

**Example 5: Quick Development Setup**

*Description:* Fastest path to TLS-enabled local development

```bash
# One-liner: generate certs and start with TLS
flmadm cert generate --output-dir /tmp/flame-certs --san "localhost,127.0.0.1"

# Start session manager with TLS
flame-session-manager --config - <<EOF
cluster:
  name: dev
  endpoint: "https://localhost:8080"
  storage: mem
  tls:
    cert_file: /tmp/flame-certs/server.crt
    key_file: /tmp/flame-certs/server.key
  executors:
    shim: host
EOF

# In another terminal, test with flmctl
flmctl --insecure list session
# Or with CA verification
flmctl --ca-cert /tmp/flame-certs/ca.crt list session
```

*Expected outcome:* TLS working in under 1 minute for development

## 5. References

**Related Documents:**
- Issue #234: Enable TLS for all components
- `docs/designs/templates.md`: Design document template
- `docs/designs/RFE368-shim-config/FS.md`: Similar config change pattern

**External References:**
- [tonic TLS documentation](https://docs.rs/tonic/latest/tonic/transport/index.html#tls)
- [rustls security](https://github.com/rustls/rustls#security)
- [Arrow Flight TLS](https://arrow.apache.org/docs/format/Flight.html#tls)
- [OpenSSL Certificate Authority Guide](https://jamielinux.com/docs/openssl-certificate-authority/)

**Implementation References:**
- Session Manager server: `session_manager/src/apiserver/mod.rs`
- Executor Manager client: `executor_manager/src/client.rs`
- Object Cache server: `object_cache/src/cache.rs`
- Configuration: `common/src/ctx.rs`
- Rust SDK client: `sdk/rust/src/client/mod.rs`
- Python SDK client: `sdk/python/src/flamepy/core/client.py`

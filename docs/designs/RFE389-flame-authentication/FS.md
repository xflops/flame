# RFE389: Enable Authentication for Flame

---
Issue: #389
Author: [TBD]
Date: 2026-03-29
Status: Draft
---

## 1. Motivation

**Background:**

Flame currently supports TLS/mTLS for transport-layer security, encrypting all communication between components (clients, session manager, executor manager). However, TLS alone does not provide application-level authentication or authorization—any client with network access and valid TLS certificates can access any session or application in the cluster.

As Flame deployments scale to multi-workspace environments, isolation between workspaces becomes critical:
- **Data Leakage Prevention**: Users should not be able to access or manipulate other workspaces' sessions, tasks, or application data.
- **Resource Isolation**: Without authentication, a malicious or buggy client could interfere with other workloads.
- **Audit Trail**: Authentication enables tracking which user performed which operations for compliance and debugging.
- **Access Control**: Different users may need different permission levels (e.g., workspace-scoped vs. cluster-wide operations).

**Target:**

Enable application-level authentication for Flame with the following goals:

1. **Workspace Isolation**: Users can only operate on sessions and applications within their assigned workspace
2. **Cluster-Wide Access**: Cluster administrators can operate on all workspaces for management and debugging
3. **System Access**: Infrastructure components (executors, nodes) have dedicated access for internal operations
4. **Pluggable Architecture**: Support multiple authentication mechanisms with JWT as the default
5. **Minimal Overhead**: Authentication should add negligible latency to request processing (local JWT validation only)
6. **Backward Compatibility**: Existing deployments without authentication continue to work (opt-in feature)

**Success Criteria:**
- Users cannot access sessions outside their workspace (verified by integration tests)
- Cluster-scoped tokens can access all workspaces
- Token validation adds < 1ms latency to requests (no remote calls for auth)
- Existing unauthenticated deployments function without changes

## 2. Function Specification

### Configuration

Authentication is configured in `flame-cluster.yaml`. The same `auth` configuration is shared by all Flame components (session manager, executor manager, cache server) for consistent token validation:

```yaml
cluster:
  endpoint: "https://flame-session-manager:8080"
  tls:
    cert_file: "/etc/flame/server.crt"
    key_file: "/etc/flame/server.key"
    ca_file: "/etc/flame/ca.crt"
  
  # Authentication section (optional - omit entirely to disable)
  # Shared by session-manager, executor-manager, and cache server
  auth:
    provider: "jwt"                        # Default: "jwt". Future: "oidc", "custom"
    config:
      algorithm: "EdDSA"                   # Default: EdDSA. Options: EdDSA, RS256, HS256
      private_key_file: "/etc/flame/jwt-private.pem"   # Private key for signing tokens (flmadm only)
      public_key_file: "/etc/flame/jwt-public.pem"     # Public key for verifying tokens (all components)
      issuer: "flame"                      # Expected issuer claim
      leeway: 60                           # Clock skew tolerance in seconds
```

**Note:** Authentication is disabled by default. To enable authentication, add the `auth` section to your configuration. Omitting the `auth` section entirely means no authentication is required (backward compatible with existing deployments).

**Environment Variables:**
| Variable                      | Description                           | Default |
| ----------------------------- | ------------------------------------- | ------- |
| `FLAME_AUTH_PRIVATE_KEY_FILE` | Path to JWT private key (for signing) | None    |
| `FLAME_AUTH_PUBLIC_KEY_FILE`  | Path to JWT public key (for verify)   | None    |

### API

Authentication uses standard gRPC metadata. Clients include the token in the `authorization` header:

```
authorization: Bearer <jwt-token>
```

**Token Claims Structure:**

Flame uses two types of tokens:

**1. User Token** - issued by admin via `flmadm token sign`, used for session manager operations:

```json
{
  "sub": "user-123",                    // Subject: user or service identifier
  "iss": "flame",                       // Issuer
  "exp": 1711756800,                    // Expiration timestamp
  "iat": 1711670400,                    // Issued at timestamp
  "scope": "workspace",                 // Scope: "workspace" | "cluster" | "system"
  "workspaces": ["team-ml", "team-data"] // Workspace identifiers (required for workspace scope)
}
```

**2. Session Token** - issued by session manager when creating a session, used for cache operations:

```json
{
  "sub": "session-abc123",              // Subject: session ID
  "iss": "flame",                       // Issuer
  "exp": 1711756800,                    // Expiration timestamp (matches session TTL)
  "iat": 1711670400,                    // Issued at timestamp
  "scope": "session",                   // Scope: always "session"
  "workspace": "team-ml"                // Workspace this session belongs to
}
```

**Scope Definitions:**

| Scope       | Description                                                                 |
| ----------- | --------------------------------------------------------------------------- |
| `workspace` | User token: access to sessions and applications within specific workspaces  |
| `cluster`   | User token: access to all workspaces in the cluster (admin)                 |
| `system`    | User token: internal scope for Flame infrastructure (executors, nodes)      |
| `session`   | Session token: access to cache objects for a specific session only          |

**Workspace Concept:**

All Flame resources (sessions, applications, cached objects) are organized by workspace:
- A workspace is a logical grouping for resource isolation
- Users with `workspace` scope can only access resources in their assigned workspaces
- A token can grant access to multiple workspaces via the `workspaces` claim (array)
- Workspace is included in all resource paths: `{workspace}/{session_id}`, `{workspace}/{session_id}/{object_id}`

**Authorization Matrix (Session Manager):**

| Operation        | `workspace`          | `cluster`   | `system`   |
| ---------------- | -------------------- | ----------- | ---------- |
| CreateSession    | Own workspace only   | Any         | -          |
| OpenSession      | Own workspace only   | Any         | -          |
| GetSessionStatus | Own workspace only   | Any         | -          |
| CloseSession     | Own workspace only   | Any         | -          |
| SetCommonData    | Own workspace only   | Any         | -          |
| CreateTask       | Own workspace only   | Any         | -          |
| WatchTask        | Own workspace only   | Any         | -          |
| ListSessions     | Own workspace only   | All         | -          |
| RegisterNode     | -                    | Yes         | Yes        |
| RegisterExecutor | -                    | -           | Yes        |
| BindExecutor     | -                    | -           | Yes        |

**Session Token Flow:**

When a session is created, the session manager signs a session-scoped token and stores it in the session status. Clients retrieve it via `session.status.token`.

```
Client                              Session Manager
   │                                      │
   │ ── CreateSession(user_token) ──────► │
   │                                      │
   │                           ┌──────────┴──────────┐
   │                           │ 1. Validate user    │
   │                           │    token            │
   │                           │                     │
   │                           │ 2. Create session   │
   │                           │    in workspace     │
   │                           │                     │
   │                           │ 3. Sign session     │
   │                           │    token with       │
   │                           │    private key      │
   │                           │                     │
   │                           │ 4. Store token in   │
   │                           │    session status   │
   │                           └──────────┬──────────┘
   │                                      │
   │ ◄── CreateSessionResponse ───────────│
   │     - session_id                     │
   │     - status.token  ◄── session token│
   │     - ...                            │
```

**SetCommonData API:**

After session creation, clients can update the session's common data via `session.set_common_data()`. This is typically used to store an ObjectRef (which includes the session token) after putting data in the cache:

```protobuf
// In flame.proto
message SetCommonDataRequest {
  string session_id = 1;
  bytes common_data = 2;
}

message SetCommonDataResponse {
  // Empty on success
}

service FlameService {
  // ... existing RPCs ...
  rpc SetCommonData(SetCommonDataRequest) returns (SetCommonDataResponse);
}
```

**SDK Methods:**

```python
# Python
session.set_common_data(data: bytes) -> None
```

```rust
// Rust
session.set_common_data(data: Vec<u8>) -> Result<(), FlameError>
```

**Typical Usage Pattern:**

```
1. CreateSession(app, workspace, ...)
   └─► Returns session with status.token (session token)

2. put_object(session_id, data, token=session.status.token)
   └─► Returns ObjectRef { endpoint, key, token }

3. session.set_common_data(object_ref.encode())
   └─► Stores ObjectRef (with embedded token) as common_data

4. Executor receives common_data, decodes ObjectRef
   └─► Uses object_ref.token to retrieve data from cache
```

**Authorization Matrix (Cache Server):**

The cache server **only accepts session tokens** for cache operations. User tokens are not valid for cache access. This ensures that cache operations are scoped to specific sessions.

| Operation         | `session`                  | `workspace`/`cluster`/`system` |
| ----------------- | -------------------------- | ------------------------------ |
| do_put            | Own session only           | Rejected                       |
| do_get            | Own session only           | Rejected                       |
| get_flight_info   | Own session only           | Rejected                       |
| list_flights      | Own session only           | Rejected                       |
| do_action (PUT)   | Own session only           | Rejected                       |
| do_action (PATCH) | Own session only           | Rejected                       |
| do_action (DELETE)| Own session only           | Rejected                       |

**Cache Authentication Flow:**

```
Client                              Cache Server
   │                                     │
   │ ── do_put(session_token, data) ───► │
   │                                     │
   │                          ┌──────────┴──────────┐
   │                          │ 1. Extract token    │
   │                          │    from metadata    │
   │                          │                     │
   │                          │ 2. Validate JWT     │
   │                          │    using public key │
   │                          │                     │
   │                          │ 3. Verify scope     │
   │                          │    == "session"     │
   │                          │                     │
   │                          │ 4. Extract:         │
   │                          │    - session_id     │
   │                          │    - workspace      │
   │                          │                     │
   │                          │ 5. Verify object    │
   │                          │    key matches      │
   │                          │    session_id       │
   │                          └──────────┬──────────┘
   │                                     │
   │ ◄──────── PutResult ────────────────│
```

**Key Design Decision:** The cache server does NOT call the session manager for token validation. All components share the same JWT public key and validate tokens independently. This ensures:
- No added latency from inter-component calls
- No dependency on session manager availability for cache operations
- Session-scoped access: cache operations are limited to the session specified in the token

**Error Responses:**

| Condition                    | gRPC Status         | Message                                              |
| ---------------------------- | ------------------- | ---------------------------------------------------- |
| Missing authorization header | `UNAUTHENTICATED`   | "Missing authorization header"                       |
| Invalid token format         | `INVALID_ARGUMENT`  | "Invalid authorization header format"                |
| Expired token                | `UNAUTHENTICATED`   | "Token expired"                                      |
| Invalid signature            | `UNAUTHENTICATED`   | "Invalid token signature"                            |
| Workspace mismatch           | `PERMISSION_DENIED` | "Access denied to workspace {workspace}"             |
| Session mismatch             | `PERMISSION_DENIED` | "Access denied to session {session_id}"              |
| Wrong scope for operation    | `PERMISSION_DENIED` | "Scope '{scope}' not permitted for this operation"   |
| Non-session token for cache  | `PERMISSION_DENIED` | "Cache operations require session token"             |

### CLI

#### flmadm token commands

```bash
# Generate signing key pair (for initial setup)
flmadm token init [--algorithm <alg>] [--output-dir <dir>]

# Sign a token for workspace-scoped access (single workspace)
flmadm token sign --subject <user-id> --scope workspace --workspace <workspace> [--expiry <duration>]

# Sign a token for workspace-scoped access (multiple workspaces)
flmadm token sign --subject <user-id> --scope workspace --workspace <ws1> --workspace <ws2> [--expiry <duration>]

# Sign a token for cluster-wide access
flmadm token sign --subject <user-id> --scope cluster [--expiry <duration>]

# Sign a token for system (infrastructure) access
flmadm token sign --subject <service-id> --scope system [--expiry <duration>]

# Verify a token
flmadm token verify <token>

# List token metadata (without validation)
flmadm token inspect <token>
```

**Command Details:**

**`flmadm token init`**
| Option         | Description                             | Default       |
| -------------- | --------------------------------------- | ------------- |
| `--algorithm`  | Signing algorithm (EdDSA, RS256, HS256) | EdDSA         |
| `--output-dir` | Directory for key files                 | `/etc/flame/` |
| `--force`      | Overwrite existing keys                 | false         |

Output:
```
Generated EdDSA key pair:
  Private key: /etc/flame/jwt-private.pem
  Public key:  /etc/flame/jwt-public.pem

Add to flame-cluster.yaml:
  auth:
    config:
      private_key_file: "/etc/flame/jwt-private.pem"
      public_key_file: "/etc/flame/jwt-public.pem"
```

**`flmadm token sign`**
| Option          | Description                        | Default                      |
| --------------- | ---------------------------------- | ---------------------------- |
| `--subject`     | User/service identifier (required) | -                            |
| `--scope`       | Token scope (required)             | -                            |
| `--workspace`   | Workspace (required for workspace scope) | -                      |
| `--expiry`      | Token validity duration            | `24h`                        |
| `--private-key` | Path to private key          | `/etc/flame/jwt-private.pem` |
| `--output`      | Output format (token, json)        | `token`                      |

Output (workspace scope):
```
eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9...

Token details:
  Subject:   alice
  Scope:     workspace
  Workspace: team-ml
  Expires:   2026-03-30T12:00:00Z
```

Output (cluster scope):
```
eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9...

Token details:
  Subject:   admin
  Scope:     cluster
  Expires:   2026-03-30T12:00:00Z
```

Output (system scope):
```
eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9...

Token details:
  Subject:   executor-pool
  Scope:     system
  Expires:   2027-03-30T12:00:00Z
```

**`flmadm token verify`**
| Option         | Description        | Default                      |
| -------------- | ------------------ | ---------------------------- |
| `--public-key` | Path to public key | `/etc/flame/jwt-public.pem`  |

Output (valid token):
```
Token is valid
  Subject:   alice
  Scope:     workspace
  Workspace: team-ml
  Expires:   2026-03-30T12:00:00Z (in 23h 45m)
  Issuer:    flame
```

Output (invalid token):
```
Token is invalid: signature verification failed
```

#### flmctl authentication

Users configure their token via environment variable or config file:

```bash
# Environment variable
export FLAME_TOKEN="eyJhbGciOiJFZERTQSI..."

# Or in ~/.flame/config.yaml
cluster:
  endpoint: "https://flame.example.com:8080"
  token: "eyJhbGciOiJFZERTQSI..."
  # Or reference a file
  token_file: "~/.flame/token"
  # Workspace can also be set (overrides token's workspace for display purposes)
  workspace: "team-ml"
```

**Exit Codes:**

| Code | Meaning                |
| ---- | ---------------------- |
| 0    | Success                |
| 1    | General error          |
| 2    | Invalid arguments      |
| 3    | Authentication failure |
| 4    | Permission denied      |

### Other Interfaces

#### Python SDK

```python
import flame

# With token string
client = flame.connect(
    endpoint="flame.example.com:8080",
    token="eyJhbGciOiJFZERTQSI..."
)

# With token file
client = flame.connect(
    endpoint="flame.example.com:8080",
    token_file="~/.flame/token"
)

# From environment (FLAME_TOKEN)
client = flame.connect(endpoint="flame.example.com:8080")

# Create session (workspace determined from token)
session = client.create_session(app="my-app", slots=4)
# Session token available via session.status.token

# Set common data on session
session.set_common_data(common_data_bytes)
```

#### Rust SDK

```rust
use flame_sdk::{FlameClient, FlameConfig};

let config = FlameConfig::builder()
    .endpoint("https://flame.example.com:8080")
    .token("eyJhbGciOiJFZERTQSI...")  // Or .token_file("~/.flame/token")
    .build()?;

let client = FlameClient::connect(config).await?;

// Create session (workspace determined from token)
let session = client.create_session("my-app", 4).await?;
// Session token available via session.status.token

// Set common data on session
session.set_common_data(common_data_bytes).await?;
```

#### Python SDK Cache Module (`flamepy.core.cache`)

The cache module requires updates to support session tokens:

**ObjectRef with Token:**

```python
@dataclass
class ObjectRef:
    """Object reference for remote cached objects."""
    endpoint: str   # Cache server endpoint
    key: str        # Object key: {workspace}/{session_id}/{object_id}
    token: str      # Session token for cache authentication
    version: int = 0

    def encode(self) -> bytes:
        data = asdict(self)
        return bson.encode(data)

    @classmethod
    def decode(cls, json_data: bytes) -> "ObjectRef":
        data = bson.decode(json_data)
        return cls(**data)
```

**Cache Functions with Token:**

```python
def put_object(session_id: str, obj: Any, token: str) -> ObjectRef:
    """Put an object into the cache.
    
    Args:
        session_id: The session ID for the object
        obj: The object to cache (will be pickled)
        token: Session token for authentication
    
    Returns:
        ObjectRef with embedded token for later retrieval
    """
    # ... serialize and upload ...
    return ObjectRef(endpoint=endpoint, key=key, token=token, version=0)


def get_object(ref: ObjectRef, deserializer: Optional[Deserializer] = None) -> Any:
    """Get an object from the cache.
    
    Uses ref.token for authentication automatically.
    
    Args:
        ref: ObjectRef pointing to the cached object (includes token)
        deserializer: Optional function to combine base and deltas
    
    Returns:
        The deserialized object
    """
    client = _get_flight_client(ref.endpoint, tls_config)
    # Add token to request metadata
    options = flight.FlightCallOptions(headers=[(b"authorization", f"Bearer {ref.token}".encode())])
    ticket = flight.Ticket(ref.key.encode())
    reader = client.do_get(ticket, options)
    # ... deserialize and return ...
```

#### Python SDK Runner Module (`flamepy.runner`)

The runner module requires updates to propagate session tokens:

**RunnerService Changes:**

```python
class RunnerService:
    def __init__(self, app: str, execution_object: Any, stateful: bool = False, autoscale: bool = True):
        # ... existing setup code ...
        
        # Step 1: Create session first (to get session token)
        session_spec = SessionAttributes(
            id=session_id,
            application=app,
            slots=1,
            min_instances=runner_context.min_instances,
            max_instances=runner_context.max_instances,
            # Note: common_data not set yet
        )
        self._session = open_session(session_id=session_id, spec=session_spec)
        
        # Step 2: Get session token from session status
        self._session_token = self._session.status.token
        
        # Step 3: Put RunnerContext in cache WITH session token
        runner_context = RunnerContext(execution_object=execution_object, stateful=stateful, autoscale=autoscale)
        serialized_ctx = cloudpickle.dumps(runner_context, protocol=cloudpickle.DEFAULT_PROTOCOL)
        object_ref = put_object(session_id, serialized_ctx, token=self._session_token)
        
        # Step 4: Update session with common_data via session method
        self._session.set_common_data(object_ref.encode())
        
        # ... rest of init ...
```

**ObjectFuture Changes:**

```python
class ObjectFuture:
    def get(self) -> Any:
        """Retrieve the concrete object."""
        result = self._future.result()
        if isinstance(result, bytes):
            object_ref = ObjectRef.decode(result)
        elif isinstance(result, ObjectRef):
            object_ref = result
        else:
            object_ref = ObjectRef.decode(result)
        
        # Token is embedded in ObjectRef, get_object uses it automatically
        return get_object(object_ref)
```

**Executor-Side (FlameRunpyService):**

```python
class FlameRunpyService(FlameService):
    def on_session_enter(self, ctx: SessionContext) -> None:
        # Decode ObjectRef from common_data (includes token)
        object_ref = ObjectRef.decode(ctx.common_data)
        
        # get_object uses object_ref.token automatically
        runner_context = get_object(object_ref)
        
        # Store token for later cache operations
        self._session_token = object_ref.token
        
        # ... initialize execution object ...
    
    def on_task(self, ctx: TaskContext) -> bytes:
        # Execute task, get result
        result = self._execute_task(ctx)
        
        # Put result in cache with session token
        result_ref = put_object(ctx.session_id, result, token=self._session_token)
        
        # Return ObjectRef (includes token for client retrieval)
        return result_ref.encode()
```

### Scope

**In Scope:**
- JWT-based authentication with EdDSA, RS256, and HS256 algorithms
- Token generation and verification via `flmadm token` commands
- Shared JWT public key configuration across all Flame components
- Local token validation (no inter-component calls for auth)
- gRPC interceptor for token validation on session manager, executor manager, and cache server
- Workspace-based authorization for session/task/cache operations
- SDK support (Rust, Python) for token-based authentication
- Backward-compatible: authentication disabled by default

**Out of Scope:**
- OIDC/OAuth2 integration (future RFE)
- Token refresh mechanism (tokens are short-lived, regenerate as needed)
- Role-based access control (RBAC) beyond scope-based permissions
- Token revocation (rely on short expiry times)
- Multi-cluster token federation
- Web UI authentication

**Limitations:**
- Tokens must be manually distributed to users (no built-in user management)
- No automatic token rotation; users must regenerate expired tokens
- Single signing key per cluster (key rotation requires cluster restart)
- System tokens are long-lived; compromise requires regeneration of all system tokens

### Feature Interaction

**Related Features:**

| Feature                  | Interaction                                                       |
| ------------------------ | ----------------------------------------------------------------- |
| TLS (RFE234)             | Auth works alongside TLS; TLS encrypts the token in transit       |
| Session Context (RFE350) | Workspace stored in session context for authorization checks      |
| Recovery (RFE384)        | Workspace persisted and recovered; tokens not stored              |
| WatchNode (RFE370)       | System-scoped authentication required for WatchNode streaming     |
| Cache (RFE318)           | Cache paths include workspace: `{workspace}/{session_id}/{object_id}` |

**Updates Required:**

| Component              | Change                                                          |
| ---------------------- | --------------------------------------------------------------- |
| `session_manager`      | Add auth interceptor, workspace-based authorization, sign session tokens |
| `executor_manager`     | Add auth interceptor, include system token in requests          |
| `object_cache`         | Add auth interceptor, session token validation                  |
| `flmadm`               | Add `token` subcommand with workspace support                   |
| `flmctl`               | Add token configuration option                                  |
| `sdk/rust`             | Add token to client config                                      |
| `sdk/python/core`      | Add token to connect(), ObjectRef.token, cache functions        |
| `sdk/python/runner`    | Propagate session token to cache operations                     |
| `common`               | Add auth module with JWT validation (shared by all)             |

**Integration Points:**

```
┌─────────────┐     ┌──────────────────────────────────────────┐
│   Client    │────►│           Session Manager                │
│  (flmctl,   │     │  ┌────────────┐    ┌─────────────────┐  │
│   SDK)      │     │  │   Auth     │───►│   Frontend API  │  │
│             │     │  │ Interceptor│    │  (authorized)   │  │
└─────────────┘     │  │ (local JWT)│    └─────────────────┘  │
                    │  └────────────┘                         │
                    └──────────────────────────────────────────┘
                                             ▲
                                             │ shared auth config
┌─────────────┐     ┌──────────────────────────────────────────┐
│   Client    │────►│           Cache Server                   │
│             │     │  ┌────────────┐    ┌─────────────────┐  │
│             │     │  │   Auth     │───►│  Arrow Flight   │  │
│             │     │  │ Interceptor│    │  (authorized)   │  │
└─────────────┘     │  │ (local JWT)│    └─────────────────┘  │
                    │  └────────────┘                         │
                    └──────────────────────────────────────────┘
                                             ▲
                                             │ shared auth config
                    ┌────────────────────────┴───────────┐
                    │        Executor Manager            │
                    │  ┌────────────┐                    │
                    │  │   Auth     │ (system token for  │
                    │  │ Interceptor│  backend requests) │
                    │  │ (local JWT)│                    │
                    │  └────────────┘                    │
                    └────────────────────────────────────┘
```

**Key Design Decision:** All components share the same `auth` configuration from `flame-cluster.yaml` and validate tokens locally using the shared JWT public key. No inter-component RPC calls are made for authentication.

**Compatibility:**
- Existing clusters without `auth` section work unchanged (auth disabled)
- No migration required; add `auth` config when ready
- Mixed-mode not supported: all clients must use tokens once auth is configured

**Breaking Changes:**
- None for existing deployments (opt-in feature)
- When enabled: all clients MUST provide valid tokens

## 3. Implementation Detail

### Architecture

```
┌───────────────────────────────────────────────────────────────────────┐
│                          Session Manager                              │
│  ┌────────────────────────────────────────────────────────────────┐   │
│  │                        gRPC Server                             │   │
│  │  ┌──────────────┐   ┌──────────────┐   ┌───────────────────┐   │   │
│  │  │    TLS       │──►│    Auth      │──►│  Frontend/Backend │   │   │
│  │  │  (existing)  │   │ Interceptor  │   │     Services      │   │   │
│  │  └──────────────┘   └──────────────┘   └───────────────────┘   │   │
│  │                            │                     │             │   │
│  │                     ┌──────▼──────┐       ┌──────▼──────┐      │   │
│  │                     │  JwtAuth    │       │ AuthContext │      │   │
│  │                     │  Validator  │       │ (in request │      │   │
│  │                     └─────────────┘       │  extensions)│      │   │
│  │                                           └─────────────┘      │   │
│  └────────────────────────────────────────────────────────────────┘   │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────────┐   │
│  │                        Controller                              │   │
│  │  ┌─────────────────┐                                           │   │
│  │  │  Authorization  │◄── Check session ownership on mutations   │   │
│  │  │     Checks      │                                           │   │
│  │  └─────────────────┘                                           │   │
│  └────────────────────────────────────────────────────────────────┘   │
└───────────────────────────────────────────────────────────────────────┘
```

### Components

| Component                                | Responsibility                                       |
| ---------------------------------------- | ---------------------------------------------------- |
| `common/src/auth/mod.rs`                 | JWT validation, claims parsing, `JwtAuth` struct     |
| `common/src/auth/claims.rs`              | `FlameClaims` struct, `Scope` enum                   |
| `common/src/auth/context.rs`             | `AuthContext` struct with workspace access checks    |
| `common/src/auth/interceptor.rs`         | Tonic interceptor for token extraction/validation    |
| `common/src/auth/session_token.rs`       | Session token signing and validation                 |
| `session_manager/src/apiserver/auth.rs`  | Server-side auth configuration, interceptor wiring   |
| `session_manager/src/controller/auth.rs` | Workspace-based authorization checks                 |
| `session_manager/src/controller/session.rs` | Session token generation on CreateSession         |
| `executor_manager/src/auth.rs`           | Auth interceptor wiring, system token handling       |
| `object_cache/src/auth.rs`               | Auth interceptor for Arrow Flight, session token checks |
| `flmadm/src/commands/token.rs`           | Token CLI commands (init, sign, verify, inspect)     |
| `flmadm/src/managers/token.rs`           | Key generation, token signing logic                  |
| `sdk/rust/src/auth.rs`                   | Client-side token inclusion                          |
| `sdk/python/src/flamepy/auth.py`         | Python client token support                          |

### Data Structures

**JWT Claims (User Token):**

```rust
// common/src/auth/claims.rs

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Workspace,
    Cluster,
    System,
    Session,  // New: for session-scoped tokens
}

/// Claims for user tokens (issued by flmadm)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserClaims {
    /// Subject: user or service identifier
    pub sub: String,
    /// Issuer
    pub iss: String,
    /// Expiration timestamp (Unix seconds)
    pub exp: u64,
    /// Issued at timestamp (Unix seconds)
    pub iat: u64,
    /// Authorization scope (workspace, cluster, or system)
    pub scope: Scope,
    /// Workspace identifiers (required for Scope::Workspace)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
}
```

**JWT Claims (Session Token):**

```rust
/// Claims for session tokens (issued by session manager)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    /// Subject: session ID
    pub sub: String,
    /// Issuer
    pub iss: String,
    /// Expiration timestamp (Unix seconds)
    pub exp: u64,
    /// Issued at timestamp (Unix seconds)
    pub iat: u64,
    /// Authorization scope (always "session")
    pub scope: Scope,  // Always Scope::Session
    /// Workspace this session belongs to
    pub workspace: String,
}

impl SessionClaims {
    /// Get session ID (from sub claim)
    pub fn session_id(&self) -> &str {
        &self.sub
    }
}
```

**Auth Context (passed through request):**

```rust
// common/src/auth/context.rs

#[derive(Debug, Clone)]
pub enum AuthContext {
    /// User context (from user tokens)
    User {
        subject: String,
        scope: Scope,
        workspaces: Vec<String>,
    },
    /// Session context (from session tokens, session_id = sub claim)
    Session {
        session_id: String,  // From claims.sub
        workspace: String,
    },
}

impl AuthContext {
    /// Create session context from session claims
    pub fn from_session_claims(claims: &SessionClaims) -> Self {
        AuthContext::Session {
            session_id: claims.sub.clone(),  // sub is the session ID
            workspace: claims.workspace.clone(),
        }
    }

    /// Check if this context can access the given workspace
    pub fn can_access_workspace(&self, target_workspace: &str) -> bool {
        match self {
            AuthContext::User { scope, workspaces, .. } => match scope {
                Scope::Cluster => true,
                Scope::Workspace => workspaces.iter().any(|ws| ws == target_workspace),
                _ => false,
            },
            AuthContext::Session { workspace, .. } => workspace == target_workspace,
        }
    }
    
    /// Check if this context can access the given session (for cache operations)
    pub fn can_access_session(&self, target_session_id: &str) -> bool {
        match self {
            AuthContext::Session { session_id, .. } => session_id == target_session_id,
            _ => false,  // User tokens cannot access cache directly
        }
    }
}
```

**Session with Workspace (storage):**

```rust
// Extend existing SessionDao in storage/engine/types.rs

pub struct SessionDao {
    pub id: String,
    pub state: i32,
    // ... existing fields ...
    
    /// Workspace this session belongs to (new field)
    pub workspace: String,
    /// Session token for cache operations (new field)
    pub token: String,
}
```

**Session Status Response:**

```rust
// session_manager/src/apiserver/frontend.rs

pub struct SessionStatus {
    pub session_id: String,
    pub state: SessionState,
    pub workspace: String,
    pub token: String,  // Session token for cache operations
    // ... other fields ...
}
```

Clients retrieve the session token from `session.status.token` after `CreateSession`, `OpenSession`, or `GetSessionStatus`.

**ObjectRef with Token:**

The ObjectRef structure includes the session token, allowing clients to retrieve cached objects without needing to separately manage tokens:

```rust
// sdk/python: ObjectRef dataclass
// sdk/rust: ObjectRef struct

/// Reference to a cached object, including authentication token
pub struct ObjectRef {
    /// Cache server endpoint (e.g., "grpc://127.0.0.1:9090")
    pub endpoint: String,
    /// Object key: {workspace}/{session_id}/{object_id}
    pub key: String,
    /// Session token for cache authentication
    pub token: String,
}
```

```python
# Python SDK
@dataclass
class ObjectRef:
    endpoint: str    # Cache server endpoint
    key: str         # "workspace/session_id/object_id"
    token: str       # Session token for cache authentication
```

**ObjectRef Flow:**

When `put_object` is called, the session token is embedded in the returned ObjectRef. When `get_object` is called, the token from ObjectRef is used for authentication:

```
Client                              Cache Server
   │                                     │
   │ ── put_object(session_token, data)─►│
   │                                     │
   │ ◄── ObjectRef {                     │
   │       endpoint: "grpc://...",       │
   │       key: "ws/ssn/obj",            │
   │       token: "<session_token>"      │
   │     } ──────────────────────────────│
   │                                     │
   │  ... later, possibly different      │
   │      client/executor ...            │
   │                                     │
   │ ── get_object(ref) ────────────────►│
   │    (uses ref.token for auth)        │
   │                                     │
   │ ◄── data ───────────────────────────│
```

**Cache Object Path:**

```rust
// object_cache/src/cache.rs

/// Cache objects are stored with workspace prefix:
/// Path format: {workspace}/{session_id}/{object_id}
/// 
/// Storage structure:
///   /var/lib/flame/cache/
///   └── {workspace}/
///       └── {session_id}/
///           └── {object_id}.arrow

pub struct ObjectKey {
    pub workspace: String,
    pub session_id: String,
    pub object_id: String,
}

impl ObjectKey {
    pub fn parse(key: &str) -> Result<Self, FlameError> {
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 3 {
            return Err(FlameError::InvalidConfig(format!(
                "Invalid key format '{}': expected workspace/session_id/object_id",
                key
            )));
        }
        Ok(Self {
            workspace: parts[0].to_string(),
            session_id: parts[1].to_string(),
            object_id: parts[2].to_string(),
        })
    }
    
    pub fn to_string(&self) -> String {
        format!("{}/{}/{}", self.workspace, self.session_id, self.object_id)
    }
}
```

**Configuration:**

```rust
// common/src/ctx.rs (extend FlameClusterContext)

/// Authentication config. Presence of this section enables auth.
/// Omit entirely from config to disable authentication.
/// This config is shared by session-manager, executor-manager, and cache server.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_provider")]
    pub provider: String,  // "jwt"
    pub config: JwtConfig,
}

fn default_provider() -> String {
    "jwt".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct JwtConfig {
    pub algorithm: String,
    pub private_key_file: Option<PathBuf>,  // For signing (flmadm only)
    pub public_key_file: Option<PathBuf>,   // For verification (all components)
    pub issuer: String,
    #[serde(default = "default_leeway")]
    pub leeway: u64,
}

fn default_leeway() -> u64 {
    60
}

// In FlameClusterConfig:
pub struct FlameClusterConfig {
    // ... existing fields ...
    
    /// Authentication config. None = auth disabled.
    /// Shared by all Flame components for consistent token validation.
    pub auth: Option<AuthConfig>,
}
```

### Algorithms

**Token Validation Flow (Local - No RPC):**

```
┌──────────────────────────────────────────────────────────────────┐
│              Auth Interceptor Flow (All Components)              │
├──────────────────────────────────────────────────────────────────┤
│                                                                   │
│  1. Extract "authorization" header from gRPC metadata            │
│     └─► Missing? → Return UNAUTHENTICATED                        │
│                                                                   │
│  2. Parse "Bearer <token>" format                                │
│     └─► Invalid format? → Return INVALID_ARGUMENT                │
│                                                                   │
│  3. Decode and validate JWT (LOCAL - no RPC):                    │
│     a. Verify signature using public key (from config)           │
│     b. Check expiration (with leeway)                            │
│     c. Verify issuer matches config                              │
│     └─► Validation failed? → Return UNAUTHENTICATED              │
│                                                                   │
│  4. Parse claims and determine token type:                       │
│     - scope == "session" → SessionClaims (for cache)             │
│     - scope in [workspace, cluster, system] → UserClaims         │
│     └─► Invalid claims? → Return INVALID_ARGUMENT                │
│                                                                   │
│  5. Validate scope-specific requirements:                        │
│     - Scope::Workspace → workspaces field must be non-empty      │
│     - Scope::Session → sub (session_id) and workspace must exist │
│     - Scope::Cluster/System → no additional requirements         │
│     └─► Invalid? → Return INVALID_ARGUMENT                       │
│                                                                   │
│  6. Create AuthContext from claims                               │
│                                                                   │
│  7. Insert AuthContext into request extensions                   │
│                                                                   │
│  8. Pass request to next handler                                 │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
```

**Session Token Generation (Session Manager):**

```rust
// session_manager/src/controller/session.rs

fn create_session(
    request: CreateSessionRequest,
    user_auth: &AuthContext,
    jwt_config: &JwtConfig,
) -> Result<CreateSessionResponse, FlameError> {
    // 1. Verify user has access to the requested workspace
    let workspace = &request.workspace;
    if !user_auth.can_access_workspace(workspace) {
        return Err(FlameError::PermissionDenied(...));
    }
    
    // 2. Create the session
    let session_id = generate_session_id();
    let session = Session::new(session_id.clone(), workspace.clone(), ...);
    
    // 3. Sign a session token (sub = session_id)
    let session_claims = SessionClaims {
        sub: session_id.clone(),  // Session ID as subject
        iss: "flame".to_string(),
        exp: calculate_expiry(session.ttl),
        iat: now(),
        scope: Scope::Session,
        workspace: workspace.clone(),
    };
    let session_token = sign_jwt(&session_claims, &jwt_config.private_key)?;
    
    // 4. Store session with token in status
    session.status.token = session_token.clone();
    store_session(&session)?;
    
    // 5. Return response (token available via session.status.token)
    Ok(CreateSessionResponse {
        session_id,
        status: session.status,
        ...
    })
}
```

**Authorization Check (Session Manager - User Tokens):**

```rust
// Pseudocode for session manager authorization (uses user tokens)

fn authorize_session_access(
    auth: &AuthContext,
    session: &Session,
    operation: Operation,
) -> Result<(), AuthError> {
    match auth {
        AuthContext::User { scope, workspaces, .. } => match scope {
            // Cluster scope can access any workspace
            Scope::Cluster => Ok(()),
            
            // Workspace scope can only access sessions in their workspaces
            Scope::Workspace => {
                if workspaces.is_empty() {
                    return Err(AuthError::InvalidToken("workspaces required"));
                }
                
                if workspaces.contains(&session.workspace) {
                    Ok(())
                } else {
                    Err(AuthError::PermissionDenied(format!(
                        "Access denied to workspace {}", session.workspace
                    )))
                }
            }
            
            // System scope cannot access sessions directly
            Scope::System => Err(AuthError::WrongScope(
                "System scope cannot access sessions"
            )),
            
            _ => Err(AuthError::WrongScope("Invalid scope for session manager")),
        },
        
        // Session tokens are not valid for session manager operations
        AuthContext::Session { .. } => Err(AuthError::WrongScope(
            "Session tokens cannot be used for session manager operations"
        )),
    }
}
```

**Authorization Check (Cache Server - Session Tokens Only):**

```rust
// Pseudocode for cache authorization (requires session tokens)

fn authorize_cache_access(
    auth: &AuthContext,
    object_key: &ObjectKey,  // parsed from {workspace}/{session_id}/{object_id}
    operation: CacheOperation,
) -> Result<(), AuthError> {
    match auth {
        // Only session tokens are allowed for cache operations
        AuthContext::Session { session_id, workspace } => {
            // Verify the object belongs to this session
            if object_key.workspace != *workspace {
                return Err(AuthError::PermissionDenied(format!(
                    "Access denied to workspace {}", object_key.workspace
                )));
            }
            
            if object_key.session_id != *session_id {
                return Err(AuthError::PermissionDenied(format!(
                    "Access denied to session {}", object_key.session_id
                )));
            }
            
            Ok(())
        }
        
        // User tokens (workspace/cluster/system) are rejected
        AuthContext::User { .. } => Err(AuthError::WrongScope(
            "Cache operations require session token"
        )),
    }
}
        Scope::Cluster => Ok(()),
        
        // Workspace scope can only access objects in their workspaces
        Scope::Workspace => {
            if auth.workspaces.is_empty() {
                return Err(AuthError::InvalidToken("workspaces required"));
            }
            
            if auth.workspaces.contains(&object_key.workspace) {
                Ok(())
            } else {
                Err(AuthError::PermissionDenied(format!(
                    "Access denied to workspace {}", object_key.workspace
                )))
            }
        }
        
        // System scope (executors) can access workspaces they're bound to
        Scope::System => {
            // Executors have bound_workspaces in their context
            // (set during executor registration)
            if auth.can_access_bound_workspace(&object_key.workspace) {
                Ok(())
            } else {
                Err(AuthError::PermissionDenied(format!(
                    "System scope not bound to workspace {}", object_key.workspace
                )))
            }
        }
    }
}
```

**Key Generation (EdDSA):**

```rust
// flmadm/src/managers/token.rs

use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;

pub fn generate_ed25519_keypair() -> Result<(SigningKey, VerifyingKey)> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    Ok((signing_key, verifying_key))
}
```

### System Considerations

**Performance:**
- JWT validation is CPU-bound; EdDSA signature verification ~50μs on modern hardware
- Token parsed once per request, cached in request extensions
- No database lookup required for validation (stateless)
- Expected overhead: < 1ms per request

**Scalability:**
- Stateless authentication scales horizontally with session manager replicas
- Each replica has identical public key configuration
- No shared state required between replicas for auth

**Reliability:**
- Auth interceptor fails-closed: invalid/missing token = request rejected
- Clock skew handled via configurable leeway (default 60s)
- Key file loading at startup; missing key = startup failure (fail-fast)

**Resource Usage:**
- Public key loaded once at startup (~32 bytes for EdDSA)
- Per-request: ~1KB for token + claims parsing
- No persistent storage for tokens

**Security:**
- Private key stored only on admin machine (for signing tokens)
- Public key distributed to all Flame components (for verification)
- Tokens transmitted over TLS (already required)
- Short-lived tokens (default 24h) limit exposure window
- EdDSA recommended: fast, secure, small signatures

**Observability:**
- Metrics: `flame_auth_requests_total{result="success|unauthenticated|permission_denied"}`
- Logs: Auth failures logged at WARN level with subject (not token)
- Tracing: Auth span added to request trace

**Operational:**
- Key rotation: Generate new keypair, update config, rolling restart
- Token revocation: Not supported; use short expiry times
- Debugging: `flmadm token inspect` shows claims without validation

### Dependencies

**New crate dependencies:**

| Crate           | Version | Purpose                            |
| --------------- | ------- | ---------------------------------- |
| `jsonwebtoken`  | 9.x     | JWT encoding/decoding              |
| `ed25519-dalek` | 2.x     | EdDSA key generation               |
| `ring`          | 0.17.x  | RSA key operations (if RS256 used) |

**Internal dependencies:**
- `common` crate extended with `auth` module
- `session_manager` depends on `common::auth`
- `flmadm` depends on `jsonwebtoken` + `ed25519-dalek` for signing

## 4. Use Cases

### Basic Use Cases

**Example 1: Admin Sets Up Authentication**

1. Admin generates signing keys:
   ```bash
   sudo flmadm token init --algorithm EdDSA --output-dir /etc/flame/
   ```

2. Admin updates `flame-cluster.yaml` (shared by all components):
   ```yaml
   auth:
     provider: jwt
     config:
       algorithm: EdDSA
       private_key_file: /etc/flame/jwt-private.pem
       public_key_file: /etc/flame/jwt-public.pem
       issuer: flame
   ```

3. Admin restarts all Flame components (session manager, executor manager, cache server) to apply config

4. Admin creates workspaces and generates tokens:
   ```bash
   # Workspace-scoped tokens for team members
   flmadm token sign --subject alice --scope workspace --workspace team-ml --expiry 720h > alice.token
   flmadm token sign --subject bob --scope workspace --workspace team-ml --expiry 720h > bob.token
   flmadm token sign --subject charlie --scope workspace --workspace team-data --expiry 720h > charlie.token
   
   # Cluster-scoped token for admins
   flmadm token sign --subject admin --scope cluster --expiry 24h > admin.token
   
   # System-scoped token for executors
   flmadm token sign --subject executor-pool --scope system --expiry 8760h > executor.token
   ```

5. Admin distributes tokens to users and updates executor deployment

**Example 2: User Submits Job with Workspace Token**

1. User receives token from admin and configures environment:
   ```bash
   export FLAME_TOKEN="eyJhbGciOiJFZERTQSI..."  # workspace=team-ml
   ```

2. User creates session (automatically in their workspace):
   ```bash
   flmctl create session --app my-app --slots 4
   # Session created: team-ml/session-abc123
   ```

3. User submits tasks:
   ```bash
   flmctl create task --session team-ml/session-abc123 --input /data/input.json
   ```

4. User can query sessions in their workspace:
   ```bash
   flmctl list sessions
   # Shows only sessions in team-ml workspace
   ```

5. User cannot access other workspaces:
   ```bash
   flmctl get session team-data/other-session
   # Error: Access denied to workspace team-data
   ```

**Example 3: Python SDK with Authentication**

```python
import flame
import os

# Connect with token from environment
client = flame.connect(
    endpoint="flame.example.com:8080",
    tls_ca_file="/etc/flame/ca.crt"
)
# FLAME_TOKEN environment variable automatically used
# Workspace determined from token

# Create session - session token available in status
session = client.create_session(app="data-processor", slots=8)
print(f"Session token: {session.status.token}")  # JWT for cache operations

# Session token is also available via get_session_status
status = client.get_session_status(session.id)
print(f"Session token from status: {status.token}")

# Cache operations use session token automatically
from flamepy import put_object, get_object

# put_object uses session.status.token internally, embeds it in ObjectRef
ref = put_object(session.id, my_data, token=session.status.token)
print(f"Stored at: {ref.key}")    # team-ml/session-abc123/obj-456
print(f"Ref token: {ref.token}")  # Session token embedded in ObjectRef

# get_object uses ref.token automatically - no separate token needed
data = get_object(ref)  # Token from ref.token used for auth

# ObjectRef can be serialized and passed to executors
# Executors can retrieve data using the embedded token
ref_bytes = ref.to_bytes()  # Serialize for task input
# ... executor receives ref_bytes ...
ref = ObjectRef.from_bytes(ref_bytes)
data = get_object(ref)  # Works because token is embedded

# Update common data after session creation
client.set_common_data(session.id, {"config": "value"})

# Submit tasks
for i in range(100):
    session.create_task(input=f"batch-{i}")

# Wait for completion
results = session.wait_all()
```

### Advanced Use Cases

**Example 4: Multi-Workspace Deployment**

Admin creates workspace-scoped tokens for different teams:

```bash
# ML team workspaces
flmadm token sign --subject alice --scope workspace --workspace ml-training --expiry 720h
flmadm token sign --subject bob --scope workspace --workspace ml-inference --expiry 720h

# Data team workspaces
flmadm token sign --subject charlie --scope workspace --workspace data-pipeline --expiry 720h
```

Sessions and cached objects are organized by workspace, providing natural isolation:
```
/var/lib/flame/cache/
├── ml-training/
│   └── session-123/
│       └── object-456.arrow
├── ml-inference/
│   └── session-789/
│       └── object-012.arrow
└── data-pipeline/
    └── session-abc/
        └── object-def.arrow
```

**Example 5: Cluster Admin Debugging**

```bash
# Cluster admin can access any workspace
export FLAME_TOKEN="<cluster-scoped-token>"

# View all sessions across all workspaces
flmctl list sessions --all-workspaces
# Shows sessions from all workspaces

# Inspect specific workspace's session
flmctl get session team-ml/session-xyz

# Cancel stuck task in any workspace
flmctl cancel task --session team-data/session-abc --task task-123

# Access cache objects from any workspace
flmctl cache get team-ml/session-xyz/object-456
```

**Example 6: Executor System Token**

1. Admin generates system token for executor pool:
   ```bash
   flmadm token sign --subject executor-pool --scope system --expiry 8760h > executor.token
   ```

2. Executor deployment uses system token for backend communication:
   ```yaml
   # executor deployment config
   env:
     - name: FLAME_TOKEN
       valueFrom:
         secretKeyRef:
           name: flame-executor-token
           key: token
   ```

3. Executors use system token to:
   - Register with session manager
   - Bind to sessions (gain access to session's workspace)
   - Access cache objects for bound sessions

## 5. References

**Related Documents:**
- [RFE234: TLS Support](../RFE234-tls/) - Transport layer security (foundation for auth)
- [RFE318: Object Cache](../RFE318-cache/) - Cache with workspace-based path organization
- [RFE350: Session Context](../RFE350-flame-session-context/) - Session workspace model
- [RFE384: Flame Recovery](../RFE384-flame-recovery/) - State persistence including workspace field

**External References:**
- [RFC 7519: JSON Web Token (JWT)](https://datatracker.ietf.org/doc/html/rfc7519)
- [jsonwebtoken crate documentation](https://docs.rs/jsonwebtoken/)
- [tonic interceptors](https://docs.rs/tonic/latest/tonic/service/trait.Interceptor.html)
- [Ed25519 specification](https://ed25519.cr.yp.to/)

**Implementation References:**
- `common/src/ctx.rs` - Existing TLS configuration (extend with auth)
- `session_manager/src/apiserver/mod.rs` - gRPC server setup (add interceptor)
- `session_manager/src/apiserver/frontend.rs` - Frontend service (add auth checks)
- `executor_manager/src/main.rs` - Executor manager setup (add interceptor)
- `object_cache/src/cache.rs` - Cache server (add auth interceptor, update path format)
- `flmadm/src/commands/mod.rs` - CLI command structure (add token subcommand)
- `flmadm/src/main.rs` - CLI entry point (add Token variant to Commands)
